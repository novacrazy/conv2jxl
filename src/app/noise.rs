//! Detection of images whose content is dominated by _random_ noise, the
//! film-grain / "dust" overlays that are fashionable in digital art and that
//! make lossless compression nearly useless (an 8000x4007 lossless JXL of a
//! grain-covered painting can be 150 MB where a visually-identical lossy one is
//! 25 MB).
//!
//! The image is cut into 32x32 blocks and each block is judged on its own, so
//! grain used as texture in only part of a picture still registers. A block
//! counts as noisy when both of these hold, measured on whichever colour
//! channel is noisiest (grain is often nearly confined to one channel, blue in
//! particular, where a luma-weighted measurement would miss it):
//!
//! 1. **Amplitude**: high-frequency sigma, estimated with Immerkaer's 3x3
//!    kernel, reaches `--noise-threshold`. The kernel is blind to flat fills and
//!    linear gradients, so clean art contributes nothing from its flat areas.
//! 2. **Whiteness**: that sigma divided by the sigma of the same block after
//!    2x2 box downsampling lands inside a band around 2.0. Averaging four
//!    samples of white noise halves its standard deviation, which is what puts
//!    random noise at 2.0. Real detail survives downsampling and sits near 1.0,
//!    while a pattern that cancels itself out under downsampling (dithering,
//!    checkerboards, aliased line art) shoots past the top of the band.
//!
//! The image as a whole is then noisy if at least `--noise-coverage` of its
//! blocks are.
//!
//! The same measurement runs again on the image downscaled 2x, because grain is
//! often applied before an upscale (or rendered at half resolution), which
//! spreads each grain over 2x2 pixels and hides it from the full-resolution
//! test. The scale that finds the most noise wins.
//!
//! What this deliberately does not catch is band-limited texture (Perlin-style
//! cloud noise, soft painted grain), which is smooth from pixel to pixel at
//! every scale and so is indistinguishable from deliberate soft shading. Those
//! also compress far better than white grain does, so there is less to win.
//!
//! Pixels come from an in-process decode: `jxl-oxide` for JPEG XL, the `image`
//! crate for the formats it is built with here. The JPEG XL decode continues
//! from the same header read that the lossy check used, so a file that the
//! cheap gates reject is never read past its first few KB.

use std::{borrow::Cow, fs::File, io::Read, path::Path};

use jxl_oxide::{InitializeResult, JxlImage, JxlThreadPool};

use crate::cli::{Conv2JxlArgs, FileType};

/// Block size for the per-block statistics. Large enough that the sigma
/// estimate is stable, small enough that flat areas of a detailed image still
/// land in blocks of their own.
const BLOCK: usize = 32;

/// sqrt(pi / 2) / 6, the scaling that turns the mean absolute Immerkaer
/// response into an estimate of the noise standard deviation.
const IMMERKAER_SCALE: f64 = 0.208_868_10;

/// Cap on a single decoded image, as an estimate of `width * height *
/// channels * 4` bytes. Both decoders materialize the whole frame before a
/// row can be read, so this bounds the memory one worker can take, times
/// `--parallel` for the run.
///
/// The estimate is deliberately pessimistic. Measured peak working set for
/// jxl-oxide is about 8 bytes per pixel: 2.0 GB on a 250 MP 8-bit RGB and
/// 2.0 GB on a 292 MP 8-bit RGBA, against estimates of 3.0 GB and 4.7 GB. So
/// this cap admits roughly 350 MP of RGB at about 3 GB real.
const MAX_DECODE_BYTES: u64 = 4 << 30;

/// Upper end of the whiteness band. White noise sits at 2.0 and measurement
/// spread keeps it well under this. A block that goes past it lost nearly all
/// of its energy to 2x2 averaging, which is a self-cancelling pattern
/// (dithering, a checkerboard, aliased hatching) rather than noise.
const MAX_WHITENESS: f32 = 3.0;

/// Channels analyzed per pixel. Alpha is never one of them: a noise overlay
/// lives in the colour channels, and a hard-edged cutout mask would only add
/// high-frequency energy that has nothing to do with grain.
const MAX_CHANNELS: usize = 3;

/// Colour channels in a buffer of `depth` interleaved samples, where the last
/// one is alpha for the even depths (gray+alpha, rgb+alpha).
const fn colour_channels(depth: usize) -> usize {
    match depth {
        1 | 2 => 1, // gray, gray+alpha
        _ => 3,     // rgb, rgb+alpha
    }
}

/// How many extra octaves below full resolution to analyze. One extra level
/// (a 2x downscale) covers grain that was upscaled or rendered at half size.
/// Further levels start blurring genuine texture into something noise-shaped.
const EXTRA_OCTAVES: u32 = 1;

#[derive(Debug, Clone, Copy)]
pub struct NoiseStats {
    /// Fraction of blocks that look like noise. This is the decision.
    pub coverage: f32,
    /// Median per-block noise sigma over the noisy blocks, in 0-255 units:
    /// how heavy the grain is where it is present.
    pub sigma: f32,
    /// Median whiteness over the noisy blocks. Reported for calibration.
    pub whiteness: f32,
    /// Median per-block sigma over the whole image, noisy or not. A high value
    /// with low coverage means detail rather than grain.
    pub detail: f32,
    pub blocks: usize,
    /// Downscale factor these numbers were measured at: 1 for full resolution,
    /// 2 if the noise only showed up an octave down.
    pub scale: u32,
}

impl std::fmt::Display for NoiseStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.0}% of blocks, sigma {:.2}, white {:.2}, detail {:.2}",
            self.coverage * 100.0,
            self.sigma,
            self.whiteness,
            self.detail
        )?;

        if self.scale > 1 {
            write!(f, ", at 1/{}", self.scale)?;
        }

        Ok(())
    }
}

impl NoiseStats {
    /// Does this image look like it is carrying a random-noise overlay?
    pub fn is_noisy(&self, args: &Conv2JxlArgs) -> bool {
        self.coverage >= args.noise_coverage
    }
}

#[derive(Debug)]
pub enum Verdict {
    /// Not applicable: analysis disabled, format not decodable, image too
    /// small, or the file is below `--noise-min-bpp` (already efficiently
    /// coded, so there is nothing for a lossy pass to win).
    NotApplicable,
    /// A JXL source that was already encoded lossily. Re-encoding it would
    /// stack generation loss on top of what is there, so it is left alone.
    AlreadyLossy,
    /// Analysis could not run: the decoder is missing, or it gave up on this
    /// file. The message is reported against the file.
    Failed(Cow<'static, str>),
    Clean(NoiseStats),
    Noisy(NoiseStats),
}

/// Run the whole gate for one file: cheap prefilters first, decode last.
pub fn evaluate(path: &Path, ext: FileType, size: u64, args: &Conv2JxlArgs) -> Verdict {
    if size < args.noise_min_size {
        return Verdict::NotApplicable;
    }

    // Bits per pixel of the _stored_ file. Anything already compact is either
    // clean art or an existing lossy encode. Either way a lossy re-encode has
    // little to win and something to lose. The check itself costs only a
    // header read, far cheaper than the decode it avoids, let alone the encode
    // after that.
    let dim = match imagesize::size(path) {
        Ok(dim) => dim,
        Err(e) => return Verdict::Failed(format!("could not read image dimensions: {e}").into()),
    };

    let pixels = (dim.width as u64).saturating_mul(dim.height as u64);

    if pixels == 0 {
        return Verdict::NotApplicable;
    }

    if (size as f64 * 8.0 / pixels as f64) < (args.noise_min_bpp * bpp_scale(ext)) as f64 {
        return Verdict::NotApplicable;
    }

    let limits = Limits {
        sigma: args.noise_threshold,
        whiteness: args.noise_whiteness,
    };

    let stats = match ext {
        FileType::JXL => {
            // One header read serves both the lossy check and, if the file
            // gets that far, the decode.
            let source = match JxlSource::open(path) {
                Ok(source) => source,
                // Without a readable header a second run over the same
                // directory could re-encode our own lossy output, so refuse
                // rather than guess.
                Err(e) => return Verdict::Failed(format!("could not read the JPEG XL header: {e}").into()),
            };

            if !args.reencode_lossy_jxl {
                match source.is_lossy() {
                    Some(true) => return Verdict::AlreadyLossy,
                    Some(false) => {}
                    None => return Verdict::Failed("the file ends before its first frame".into()),
                }
            }

            analyze_jxl(source, limits)
        }
        _ if decodable(ext) => analyze_decoded(path, ext, limits),
        _ => return Verdict::NotApplicable,
    };

    match stats {
        Ok(stats) if stats.is_noisy(args) => Verdict::Noisy(stats),
        Ok(stats) => Verdict::Clean(stats),
        Err(e) => Verdict::Failed(format!("noise analysis failed: {e}").into()),
    }
}

/// How much room to give `--noise-min-bpp` for a format weaker at lossless
/// compression than JPEG XL, which is what the threshold is calibrated on.
///
/// Measured over real artwork, the same clean image is 1.8x to 4.3x larger as
/// a PNG than as a lossless JXL, because JXL's modular mode is simply better at
/// flat colour and smooth shading. A noisy image is only 1.1x to 1.3x larger,
/// since neither format can compress noise. So the gap between well-behaved and
/// noisy is _wider_ in PNG, just shifted up, and a threshold that is meaningful
/// for JXL would let every PNG through.
///
/// Formats that do not compress at all (BMP, TGA, uncompressed TIFF) always sit
/// at their raw bit depth and so always pass. That is the right answer for
/// them: their size says nothing about their content, so the only way to know
/// is to look.
const fn bpp_scale(ext: FileType) -> f32 {
    match ext {
        FileType::JXL => 1.0,
        _ => 3.0,
    }
}

/// Formats the `image` crate is compiled with here (see Cargo.toml features).
fn decodable(ext: FileType) -> bool {
    matches!(
        ext,
        FileType::PNG | FileType::TIFF | FileType::TGA | FileType::QOI | FileType::BMP
    )
}

/// What the conversion needs to know about a JPEG XL source before deciding
/// to touch it.
pub struct JxlInfo {
    /// Was the encoder allowed to discard information? `None` if the file
    /// ended before its first frame header, in which case the caller decides
    /// how cautious to be.
    pub lossy: Option<bool>,
    pub animated: bool,
}

/// Read the header and stop the moment it is complete, a few KB off the
/// front of the file whatever its size. That matters at archive scale: this
/// runs on every JPEG XL the scan turns up, and shelling out to `jxlinfo` a
/// few million times would cost more than all the real work.
///
/// `None` if the header could not be read at all.
pub fn inspect_jxl(path: &Path) -> Option<JxlInfo> {
    let source = JxlSource::open(path).ok()?;

    Some(JxlInfo {
        lossy: source.is_lossy(),
        animated: source.is_animated(),
    })
}

/// A JPEG XL file with its header decoded and the rest still unread. The
/// header is all the lossy check needs, so the decode proper only happens for
/// a file that gets past every cheaper gate, and it carries on from the same
/// file handle and buffer.
struct JxlSource {
    image: JxlImage,
    file: File,
    buf: Vec<u8>,
    valid: usize,
}

impl JxlSource {
    /// Feed the decoder from the front of the file until it says it has the
    /// header.
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = File::open(path)?;

        // Single-threaded on purpose. Files are decoded `--parallel` at a
        // time, one per worker, so per-file threading would only oversubscribe
        // the machine. `-t` is for cjxl, whose encode is worth the threads.
        let mut uninit = JxlImage::builder().pool(JxlThreadPool::none()).build_uninit();

        let mut buf = vec![0u8; 8 << 10];
        let mut valid = 0usize;

        loop {
            // A header with a large embedded colour profile can outgrow the
            // buffer, so grow it instead of spinning on a zero-length read.
            if valid == buf.len() {
                buf.resize(buf.len() * 2, 0);
            }

            match file.read(&mut buf[valid..])? {
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                read => valid += read,
            }

            let consumed = uninit
                .feed_bytes(&buf[..valid])
                .map_err(std::io::Error::other)?;

            buf.copy_within(consumed..valid, 0);
            valid -= consumed;

            match uninit.try_init().map_err(std::io::Error::other)? {
                InitializeResult::NeedMoreData(need_more) => uninit = need_more,
                InitializeResult::Initialized(image) => {
                    let mut source = JxlSource {
                        image,
                        file,
                        buf,
                        valid,
                    };
                    source.read_first_frame_header()?;
                    return Ok(source);
                }
            }
        }
    }

    /// Feed a little past the image header, until the first frame's header
    /// has been parsed. That is where the encoding (VarDCT or Modular) lives,
    /// and it sits after the ICC profile and preview, so the bytes that
    /// initialized the decoder usually stop short of it.
    fn read_first_frame_header(&mut self) -> std::io::Result<()> {
        loop {
            if self.valid > 0 {
                let consumed = self
                    .image
                    .feed_bytes(&self.buf[..self.valid])
                    .map_err(std::io::Error::other)?;
                self.buf.copy_within(consumed..self.valid, 0);
                self.valid -= consumed;
            }

            if self.image.frame_header(0).is_some() {
                return Ok(());
            }

            if self.valid == self.buf.len() {
                self.buf.resize(self.buf.len() * 2, 0);
            }

            match self.file.read(&mut self.buf[self.valid..])? {
                // A file that ends here has no frame. Not an error at this
                // stage: `is_lossy` reports it as unknown.
                0 => return Ok(()),
                read => self.valid += read,
            }
        }
    }

    /// Whether the encoder was allowed to discard information. `None` if the
    /// file ended before its first frame header.
    ///
    /// Two signals, either of which settles it. XYB is the colour space lossy
    /// encodes work in, and is what `jxlinfo` reports as lossy versus
    /// "(possibly) lossless". VarDCT is quantized by construction, so a VarDCT
    /// frame is lossy whatever its colour space: a losslessly recompressed
    /// JPEG is VarDCT in YCbCr without XYB, and by the first test alone would
    /// pass as lossless.
    fn is_lossy(&self) -> Option<bool> {
        if self.image.image_header().metadata.xyb_encoded {
            return Some(true);
        }

        let frame = self.image.frame_header(0)?;

        Some(frame.encoding == jxl_oxide::frame::Encoding::VarDct)
    }

    fn is_animated(&self) -> bool {
        self.image.image_header().metadata.animation.is_some()
    }

    /// Feed the rest of the file and hand back the decoder, ready to render.
    fn read_to_end(self) -> std::io::Result<JxlImage> {
        let JxlSource {
            mut image,
            mut file,
            mut buf,
            mut valid,
        } = self;

        // Whatever was left over from the header read goes in first.
        if valid > 0 {
            let consumed = image.feed_bytes(&buf[..valid]).map_err(std::io::Error::other)?;
            buf.copy_within(consumed..valid, 0);
            valid -= consumed;
        }

        // Reads are big from here on: the header buffer was sized for a few KB.
        buf.resize(1 << 20, 0);

        while !image.is_loading_done() {
            let read = file.read(&mut buf[valid..])?;

            if read == 0 {
                break;
            }

            valid += read;

            let consumed = image.feed_bytes(&buf[..valid]).map_err(std::io::Error::other)?;
            buf.copy_within(consumed..valid, 0);
            valid -= consumed;
        }

        image.finalize().map_err(std::io::Error::other)?;

        Ok(image)
    }
}

// ---------------------------------------------------------------------------
// analysis
// ---------------------------------------------------------------------------

/// What a block has to look like to be counted as noise.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Minimum high-frequency sigma, in 0-255 units.
    pub sigma: f32,
    /// Minimum whiteness. [`MAX_WHITENESS`] is the other end of the band.
    pub whiteness: f32,
}

/// Accumulates per-block statistics from interleaved pixel rows fed in top to
/// bottom. Blocks are judged independently, so noise confined to part of an
/// image still shows up in [`NoiseStats::coverage`].
pub struct Analyzer {
    width: usize,
    channels: usize,
    limits: Limits,
    /// BLOCK rows of interleaved samples, `width * channels` wide.
    band: Vec<f32>,
    /// Rows currently filled in `band`.
    filled: usize,
    block: Vec<f32>,
    half: Vec<f32>,
    /// Per-block sigma of the noisiest channel, and that channel's whiteness.
    sigma: Vec<f32>,
    whiteness: Vec<f32>,
    /// The same analysis one octave down, fed 2x2-averaged rows.
    coarse: Option<Box<Analyzer>>,
    /// Half-resolution row being accumulated for `coarse`: holds the column
    /// sums of one input row until its partner row arrives.
    carry: Vec<f32>,
    carried: bool,
    scale: u32,
}

impl Analyzer {
    pub fn new(width: usize, channels: usize, limits: Limits) -> Self {
        Self::with_octaves(width, channels, limits, EXTRA_OCTAVES, 1)
    }

    fn with_octaves(width: usize, channels: usize, limits: Limits, octaves: u32, scale: u32) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);

        // only worth another octave if it still has a whole block in it
        let coarse = (octaves > 0 && width / 2 >= BLOCK).then(|| {
            Box::new(Self::with_octaves(
                width / 2,
                channels,
                limits,
                octaves - 1,
                scale * 2,
            ))
        });

        Self {
            width,
            channels,
            limits,
            band: vec![0.0; width * channels * BLOCK],
            filled: 0,
            block: vec![0.0; BLOCK * BLOCK],
            half: vec![0.0; (BLOCK / 2) * (BLOCK / 2)],
            sigma: Vec::new(),
            whiteness: Vec::new(),
            carry: vec![0.0; if coarse.is_some() { (width / 2) * channels } else { 0 }],
            carried: false,
            coarse,
            scale,
        }
    }

    /// Feed one row of interleaved samples in 0-255 units, `width * channels`
    /// long. Rows past the last full band of [`BLOCK`] rows are ignored, as are
    /// columns past the last full block.
    pub fn push_row(&mut self, row: &[f32]) {
        let stride = self.width * self.channels;

        debug_assert_eq!(row.len(), stride);

        self.band[self.filled * stride..][..stride].copy_from_slice(row);
        self.filled += 1;

        if self.filled == BLOCK {
            self.flush_band();
            self.filled = 0;
        }

        self.feed_coarse(row);
    }

    /// Fold `row` into the half-resolution image: sum pairs of columns, then on
    /// every second row add the two together and hand the average down.
    fn feed_coarse(&mut self, row: &[f32]) {
        let Some(coarse) = self.coarse.as_mut() else {
            return;
        };

        let channels = self.channels;

        for (out, pair) in self
            .carry
            .chunks_exact_mut(channels)
            .zip(row.chunks_exact(channels * 2))
        {
            for (c, out) in out.iter_mut().enumerate() {
                let sum = pair[c] + pair[channels + c];

                if self.carried {
                    *out = (*out + sum) * 0.25;
                } else {
                    *out = sum;
                }
            }
        }

        if self.carried {
            coarse.push_row(&self.carry);
        }

        self.carried = !self.carried;
    }

    fn flush_band(&mut self) {
        let stride = self.width * self.channels;

        for bx in (0..self.width.saturating_sub(BLOCK - 1)).step_by(BLOCK) {
            // Grain is often nearly confined to one channel, so score each and
            // keep the worst rather than averaging it away.
            let mut worst = (0.0f32, 0.0f32);

            for c in 0..self.channels {
                for y in 0..BLOCK {
                    let row = &self.band[y * stride + (bx * self.channels + c)..];

                    for x in 0..BLOCK {
                        self.block[y * BLOCK + x] = row[x * self.channels];
                    }
                }

                // 2x2 box downsample, under which white noise loses exactly half its sigma
                const H: usize = BLOCK / 2;
                for y in 0..H {
                    for x in 0..H {
                        self.half[y * H + x] = 0.25
                            * (self.block[(2 * y) * BLOCK + 2 * x]
                                + self.block[(2 * y) * BLOCK + 2 * x + 1]
                                + self.block[(2 * y + 1) * BLOCK + 2 * x]
                                + self.block[(2 * y + 1) * BLOCK + 2 * x + 1]);
                    }
                }

                let sigma = immerkaer(&self.block, BLOCK, BLOCK);

                if sigma > worst.0 {
                    let half = immerkaer(&self.half, H, H);

                    // A block whose energy vanishes under downsampling is a
                    // self-cancelling pattern, not noise, so park it above the
                    // band rather than dividing by ~0.
                    worst = (sigma, if half > 1e-4 { sigma / half } else { f32::INFINITY });
                }
            }

            self.sigma.push(worst.0);
            self.whiteness.push(worst.1);
        }
    }

    /// Statistics for whichever scale found the most noise.
    pub fn finish(self) -> Option<NoiseStats> {
        let Analyzer {
            limits,
            sigma,
            whiteness,
            coarse,
            scale,
            ..
        } = self;

        let coarse = coarse.and_then(|coarse| coarse.finish());

        let here = Self::stats(limits, sigma, whiteness, scale);

        match (here, coarse) {
            (Some(here), Some(coarse)) if coarse.coverage > here.coverage => Some(coarse),
            (here, coarse) => here.or(coarse),
        }
    }

    fn stats(limits: Limits, sigma: Vec<f32>, whiteness: Vec<f32>, scale: u32) -> Option<NoiseStats> {
        if sigma.len() < 4 {
            return None; // too small to say anything useful
        }

        let noisy = |i: usize| {
            sigma[i] >= limits.sigma && (limits.whiteness..=MAX_WHITENESS).contains(&whiteness[i])
        };

        let mut noisy_sigma: Vec<f32> = Vec::new();
        let mut noisy_white: Vec<f32> = Vec::new();

        for i in 0..sigma.len() {
            if noisy(i) {
                noisy_sigma.push(sigma[i]);
                noisy_white.push(whiteness[i]);
            }
        }

        let mut all_sigma = sigma.clone();

        Some(NoiseStats {
            coverage: noisy_sigma.len() as f32 / sigma.len() as f32,
            sigma: median(&mut noisy_sigma),
            whiteness: median(&mut noisy_white),
            detail: median(&mut all_sigma),
            blocks: sigma.len(),
            scale,
        })
    }
}

fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }

    values.sort_unstable_by(f32::total_cmp);

    values[values.len() / 2]
}

/// Immerkaer's noise estimator: mean absolute response of the 3x3 kernel
/// `[[1, -2, 1], [-2, 4, -2], [1, -2, 1]]`, scaled to a standard deviation.
/// The kernel is blind to linear gradients, so smooth shading reads as zero.
fn immerkaer(px: &[f32], w: usize, h: usize) -> f32 {
    if w < 3 || h < 3 {
        return 0.0;
    }

    let mut sum = 0.0f64;

    for y in 1..h - 1 {
        let (up, mid, down) = (&px[(y - 1) * w..], &px[y * w..], &px[(y + 1) * w..]);

        for x in 1..w - 1 {
            let r = 4.0 * mid[x] - 2.0 * (up[x] + down[x] + mid[x - 1] + mid[x + 1])
                + (up[x - 1] + up[x + 1] + down[x - 1] + down[x + 1]);

            sum += r.abs() as f64;
        }
    }

    (sum / ((w - 2) * (h - 2)) as f64 * IMMERKAER_SCALE) as f32
}

// ---------------------------------------------------------------------------
// pixel sources
// ---------------------------------------------------------------------------

/// Decode a JPEG XL in-process with `jxl-oxide` and analyze it row by row.
///
/// The decoder holds the whole frame before the first row can be read, so a
/// 54 MP RGB image costs about 650 MB of f32 samples per worker at peak. That
/// is the same shape `djxl` had, minus the process, the 160 MB pipe, and the
/// threads it would have contended for with every other worker.
fn analyze_jxl(source: JxlSource, limits: Limits) -> Result<NoiseStats, String> {
    let (width, height) = (source.image.width() as usize, source.image.height() as usize);

    if width < BLOCK || height < BLOCK {
        return Err("image is smaller than one analysis block".to_owned());
    }

    let format = source.image.pixel_format();

    // CMYK is the one layout where "colour channels" is not 1 or 3.
    if format.has_black() {
        return Err(format!("unsupported pixel format {format:?}"));
    }

    let channels = if format.is_grayscale() { 1 } else { 3 };

    if (width * height * channels) as u64 * size_of::<f32>() as u64 > MAX_DECODE_BYTES {
        return Err("image is too large to decode in one piece".to_owned());
    }

    let image = source
        .read_to_end()
        .map_err(|e| format!("could not read the JPEG XL file: {e}"))?;

    if image.num_loaded_keyframes() == 0 {
        return Err("the file ended before its first frame".to_owned());
    }

    // The first keyframe is the image for a still, and as good a sample as
    // any for an animation.
    let render = image
        .render_frame(0)
        .map_err(|e| format!("could not decode the JPEG XL frame: {e}"))?;

    // Orientation is applied by the stream, so its dimensions are the ones
    // the rows come out in.
    let mut stream = render.stream_no_alpha();
    let (width, height) = (stream.width() as usize, stream.height() as usize);

    if stream.channels() as usize != channels {
        return Err(format!("decoder produced {} channels, expected {channels}", stream.channels()));
    }

    let stride = width * channels;
    let mut row = vec![0.0f32; stride];
    let mut analyzer = Analyzer::new(width, channels, limits);

    // only whole bands contribute, so stop at the last one
    for _ in 0..(height / BLOCK) * BLOCK {
        if stream.write_to_buffer(&mut row) != stride {
            return Err("the decoder stopped early".to_owned());
        }

        // samples come out with a nominal range of 0-1
        for v in &mut row {
            *v *= 255.0;
        }

        analyzer.push_row(&row);
    }

    analyzer
        .finish()
        .ok_or_else(|| "image is smaller than one analysis block".to_owned())
}

/// Decode with the `image` crate (PNG/TIFF/TGA/QOI/BMP) and analyze the buffer.
fn analyze_decoded(path: &Path, ext: FileType, limits: Limits) -> Result<NoiseStats, String> {
    use image::{ColorType, ImageDecoder};

    let decoder = super::conv2png::decoder_for(path, ext).map_err(|e| e.to_string())?;

    let (width, height) = decoder.dimensions();
    let (width, height) = (width as usize, height as usize);

    if width < BLOCK || height < BLOCK {
        return Err("image is smaller than one analysis block".to_owned());
    }

    if decoder.total_bytes() > MAX_DECODE_BYTES {
        return Err("image is too large to decode in one piece".to_owned());
    }

    let color = decoder.color_type();

    let (channels, two_byte) = match color {
        ColorType::L8 => (1, false),
        ColorType::La8 => (2, false),
        ColorType::Rgb8 => (3, false),
        ColorType::Rgba8 => (4, false),
        ColorType::L16 => (1, true),
        ColorType::La16 => (2, true),
        ColorType::Rgb16 => (3, true),
        ColorType::Rgba16 => (4, true),
        // float/HDR buffers aren't worth the special case
        other => return Err(format!("unsupported pixel format {other:?}")),
    };

    let mut bytes = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut bytes).map_err(|e| e.to_string())?;

    let sample = if two_byte { 2 } else { 1 };
    let stride = width * channels * sample;

    if bytes.len() < stride * height {
        return Err("decoder returned a short buffer".to_owned());
    }

    let analyzed = colour_channels(channels);

    let mut row = vec![0.0f32; width * analyzed];
    let mut analyzer = Analyzer::new(width, analyzed, limits);

    for raw in bytes.chunks_exact(stride).take((height / BLOCK) * BLOCK) {
        for (px, out) in raw
            .chunks_exact(channels * sample)
            .zip(row.chunks_exact_mut(analyzed))
        {
            for (c, out) in out.iter_mut().enumerate() {
                *out = if two_byte {
                    // the image crate hands back native-endian 16-bit samples
                    (u16::from_ne_bytes([px[c * 2], px[c * 2 + 1]]) as f32) / 257.0
                } else {
                    px[c] as f32
                };
            }
        }

        analyzer.push_row(&row);
    }

    analyzer
        .finish()
        .ok_or_else(|| "image is smaller than one analysis block".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, cheap white noise, so no real RNG dependency is needed here.
    struct Lcg(u64);

    impl Lcg {
        fn next_unit(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u32 << 24) as f32) - 0.5
        }
    }

    const LIMITS: Limits = Limits {
        sigma: 0.5,
        whiteness: 1.6,
    };

    fn analyze(w: usize, h: usize, mut pixel: impl FnMut(usize, usize) -> f32) -> NoiseStats {
        analyze_rgb(w, h, |x, y| [pixel(x, y); 3])
    }

    fn analyze_rgb(w: usize, h: usize, mut pixel: impl FnMut(usize, usize) -> [f32; 3]) -> NoiseStats {
        let mut analyzer = Analyzer::new(w, 3, LIMITS);
        let mut row = vec![0.0; w * 3];

        for y in 0..h {
            for (x, px) in row.chunks_exact_mut(3).enumerate() {
                px.copy_from_slice(&pixel(x, y));
            }
            analyzer.push_row(&row);
        }

        analyzer.finish().unwrap()
    }

    #[test]
    fn flat_and_gradient_content_reads_as_clean() {
        // flat fill plus a linear ramp: the estimator is blind to both
        let stats = analyze(256, 256, |x, _| 40.0 + x as f32 * 0.5);

        assert_eq!(stats.coverage, 0.0, "{stats}");
        assert!(stats.detail < 0.01, "{stats}");
    }

    #[test]
    fn white_noise_reads_as_noisy() {
        let mut rng = Lcg(1);
        // sigma of a uniform(-4, 4) draw is about 2.3
        let stats = analyze(256, 256, |_, _| 128.0 + rng.next_unit() * 8.0);

        assert!(stats.coverage > 0.95, "{stats}");
        assert!(stats.whiteness > 1.8 && stats.whiteness < 2.2, "{stats}");
    }

    #[test]
    fn structured_high_frequency_is_not_noise() {
        // a hard checkerboard has far more high-frequency energy than grain,
        // but 2x2 averaging erases it completely, which noise never does
        let stats = analyze(256, 256, |x, y| if (x + y) % 2 == 0 { 20.0 } else { 220.0 });

        assert_eq!(stats.coverage, 0.0, "{stats}");
        assert!(stats.detail > 10.0, "{stats}");
    }

    #[test]
    fn noise_in_part_of_the_image_is_still_found() {
        // grain used as texture in one region: the point of judging blocks
        // independently is that this is not averaged away
        let mut rng = Lcg(7);
        let stats = analyze(256, 256, |x, y| {
            if x < 96 && y < 96 { 128.0 + rng.next_unit() * 8.0 } else { 128.0 }
        });

        assert!((0.1..0.2).contains(&stats.coverage), "{stats}");
    }

    #[test]
    fn grain_applied_before_an_upscale_is_found() {
        // 2x2 blocks of identical noise: invisible to the full-resolution test
        // (each 2x2 group is flat), obvious one octave down
        let mut rng = Lcg(11);
        let mut grain = vec![0.0f32; 128 * 128];

        for g in grain.iter_mut() {
            *g = rng.next_unit() * 16.0;
        }

        let stats = analyze(256, 256, |x, y| 128.0 + grain[(y / 2) * 128 + x / 2]);

        assert!(stats.coverage > 0.9, "{stats}");
        assert_eq!(stats.scale, 2, "{stats}");
    }

    #[test]
    fn noise_in_one_channel_is_found() {
        // grain confined to blue: a luma-weighted measurement would scale this
        // down by 0.114 and miss it
        let mut rng = Lcg(3);
        let stats = analyze_rgb(256, 256, |_, _| [90.0, 140.0, 60.0 + rng.next_unit() * 8.0]);

        assert!(stats.coverage > 0.95, "{stats}");
    }

    /// Calibration helper, not part of the normal suite: point it at real
    /// files to see what the detector makes of them.
    ///
    /// ```text
    /// CONV2JXL_SAMPLES="a.jxl b.png" cargo test --bin conv2jxl -- --ignored --nocapture samples
    /// ```
    #[test]
    #[ignore = "needs sample files"]
    fn samples() {
        let samples = std::env::var("CONV2JXL_SAMPLES").unwrap_or_default();

        for sample in samples.split_ascii_whitespace() {
            let path = Path::new(sample);

            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|e| e.parse::<FileType>().ok())
                .expect("unrecognized extension");

            let size = match std::fs::metadata(path) {
                Ok(m) => m.len(),
                Err(e) => {
                    println!("{sample:34} cannot stat: {e}");
                    continue;
                }
            };

            let extra = std::env::var("CONV2JXL_ARGS").unwrap_or_default();
            let mut argv: Vec<&str> = vec!["-N", "90"];
            argv.extend(extra.split_ascii_whitespace());

            let mut args = <Conv2JxlArgs as argh::FromArgs>::from_args(&["conv2jxl"], &argv).unwrap();
            args.normalize();

            let started = std::time::Instant::now();
            let verdict = evaluate(path, ext, size, &args);
            let took = started.elapsed().as_secs_f64();

            let (label, stats) = match verdict {
                Verdict::Noisy(stats) => ("NOISY", Some(stats)),
                Verdict::Clean(stats) => ("clean", Some(stats)),
                other => {
                    println!("{sample:34} {other:?} ({took:.2}s)");
                    continue;
                }
            };

            let stats = stats.unwrap();

            println!(
                "{sample:34} {label:5} cov {:5.3} sigma {:6.2} white {:5.2} detail {:6.2} blocks {} ({took:.2}s)",
                stats.coverage, stats.sigma, stats.whiteness, stats.detail, stats.blocks
            );
        }
    }

}
