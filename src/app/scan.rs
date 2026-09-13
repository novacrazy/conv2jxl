use std::{ffi::OsStr, path::Path, str::FromStr as _};

use crate::cli::{Conv2JxlArgs, SortMethod, SortOrder};

use super::*;

#[derive(Debug, Default)]
pub struct FileScanObserver {
    pub found: AtomicU64,
    pub bytes: AtomicU64,
}

#[derive(Debug, Default)]
pub struct ScanObserver {
    pub dir_read: AtomicU64,
    pub dir_found: AtomicU64,
    /// Entries skipped by --filter / --exclude regexes, or by name as
    /// recompressed JPEGs (see [`Conv2JxlArgs::skip_by_name`]).
    pub excluded: AtomicU64,
    /// Directories or entries skipped due to I/O errors.
    pub errors: AtomicU64,
    pub files: PerFileType<FileScanObserver>,
    /// Directory currently being read, for the live "Current:" line.
    pub current: Mutex<PathBuf>,
    /// Set by the UI to request the scan abort early.
    pub cancel: std::sync::atomic::AtomicBool,
}

impl ScanObserver {
    fn err(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// `foo.jpg.jxl` or `foo.jpeg.jxl`, in any case. The inner extension is how
/// this tool (and most others) name a recompressed JPEG.
fn is_recompressed_jpeg_name(path: &Path) -> bool {
    path.file_stem()
        .map(Path::new)
        .and_then(Path::extension)
        .and_then(OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

impl Conv2JxlArgs {
    /// Leave out a JPEG XL source whose name marks it as a recompressed JPEG.
    /// Those are VarDCT, so lossy, and the header check would skip them
    /// anyway. Deciding here saves opening every one of them.
    ///
    /// A file that is named that way but holds a lossless encode is missed,
    /// which costs a skipped opportunity and never a file.
    pub fn skip_by_name(&self, path: &Path, ext: FileType) -> bool {
        ext == FileType::JXL && !self.reencode_lossy_jxl && is_recompressed_jpeg_name(path)
    }

    pub fn normalize(&mut self) {
        self.threads = self.threads.clamp(-1, i32::MAX);
        self.quality = self.quality.clamp(0, 100);
        self.effort = self.effort.clamp(0, 10);
        self.randomize = self.randomize.clamp(0.0, 1.0);
        self.min_ratio = self.min_ratio.max(0.0);
        self.quality_if_inefficient = self.quality_if_inefficient.map(|q| q.min(100));
        self.quality_if_noisy = self.quality_if_noisy.map(|q| q.min(100));
        self.noise_threshold = self.noise_threshold.max(0.0);
        self.noise_coverage = self.noise_coverage.clamp(0.0, 1.0);
        self.noise_whiteness = self.noise_whiteness.max(0.0);
        self.noise_min_bpp = self.noise_min_bpp.max(0.0);
        self.min_size = self.min_size.max(1); // always exclude empty files

        // ensure min_size <= max_size
        self.max_size = self.max_size.max(self.min_size);

        // ensure min_depth <= max_depth
        self.max_depth = self.max_depth.max(self.min_depth);

        if self.parallel == -1 {
            self.parallel = std::thread::available_parallelism()
                .map(|n| n.get() as i32)
                .unwrap_or(1);
        } else {
            self.parallel = self.parallel.max(1);
        }
    }

    /// Walk the requested paths and build the [`ConversionState`].
    ///
    /// This is intentionally infallible: per-directory and per-entry I/O errors
    /// are skipped and tallied in `observer.errors` rather than aborting the
    /// whole scan. Regex patterns are validated by the caller before this runs,
    /// so an invalid pattern here is simply treated as absent.
    pub fn scan(&self, observer: &ScanObserver) -> ConversionState {
        let filter = self.filter.as_deref().and_then(|s| regex::Regex::new(s).ok());
        let exclude = self.exclude.as_deref().and_then(|s| regex::Regex::new(s).ok());

        let mut visited = std::collections::HashSet::<PathBuf, _>::with_capacity_and_hasher(
            1024,
            foldhash::fast::FixedState::default(),
        );

        let mut files: Vec<FileEntry> = Vec::new();
        let mut current_files: Vec<FileEntry> = Vec::new();
        let mut pending_dirs = Vec::new();

        for path in &self.paths {
            // Check link-ness on the path as given: `canonicalize` resolves
            // symlinks, and `fs::metadata` follows them, so both would report
            // the target and `is_symlink()` would never be true.
            if !self.follow_links
                && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink())
            {
                continue;
            }

            let Ok(path) = path.canonicalize() else {
                observer.err();
                continue;
            };

            let Ok(metadata) = std::fs::metadata(&path) else {
                observer.err();
                continue;
            };

            if metadata.is_file() && (self.min_size..=self.max_size).contains(&metadata.len()) {
                let Some(ext) = path
                    .extension()
                    .and_then(OsStr::to_str)
                    .and_then(|s| FileType::from_str(s).ok())
                else {
                    continue;
                };

                if !self.extensions.contains(&ext) {
                    continue;
                }

                if self.skip_by_name(&path, ext) {
                    observer.excluded.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let f = observer.files.get(ext);

                f.found.fetch_add(1, Ordering::Relaxed);
                f.bytes.fetch_add(metadata.len(), Ordering::Relaxed);

                files.push(FileEntry::new(path.clone(), ext, metadata));
            } else if metadata.is_dir() && visited.insert(path.clone()) {
                pending_dirs.push((0u64, path));

                observer.dir_found.fetch_add(1, Ordering::Relaxed);
            }
        }

        while let Some((depth, path)) = pending_dirs.pop() {
            if observer.cancelled() {
                break;
            }

            observer.dir_read.fetch_add(1, Ordering::Relaxed);

            if depth > self.max_depth {
                continue;
            }

            if let Ok(mut current) = observer.current.lock() {
                current.clone_from(&path);
            }

            // optionally simulate a slow scan so the scan UI can be exercised.
            // Sleep in small chunks so a cancel stays responsive.
            if self.dry_run && self.dry_run_delay > 0 {
                let mut remaining = self.dry_run_delay;
                while remaining > 0 {
                    let chunk = remaining.min(50);
                    std::thread::sleep(std::time::Duration::from_millis(chunk));
                    remaining -= chunk;

                    if observer.cancelled() {
                        break;
                    }
                }
            }

            current_files.clear();

            let read_dir = match std::fs::read_dir(&path) {
                Ok(rd) => rd,
                Err(_) => {
                    observer.err();
                    continue;
                }
            };

            for entry in read_dir {
                let Ok(entry) = entry else {
                    observer.err();
                    continue;
                };
                let Ok(mut ft) = entry.file_type() else {
                    observer.err();
                    continue;
                };

                // avoid computing metadata unless necessary
                let mut ext = None;
                let mut metadata = None;

                let path = entry.path();

                // store and filter by extension only for files,
                // before potentially expensive metadata calls
                if ft.is_file() {
                    ext = match path
                        .extension()
                        .and_then(OsStr::to_str)
                        .and_then(|s| FileType::from_str(s).ok())
                    {
                        Some(ext) if self.extensions.contains(&ext) => Some(ext),
                        _ => continue,
                    };
                }

                if (filter.is_some() || exclude.is_some())
                    && let Some(path) = path.to_str()
                    && (matches!(filter, Some(ref filter) if !filter.is_match(path))
                        || matches!(exclude, Some(ref exclude) if exclude.is_match(path)))
                {
                    observer.excluded.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                if ft.is_symlink() {
                    if !self.follow_links {
                        continue;
                    }

                    // `fs::metadata` follows the link; `symlink_metadata` would
                    // just describe the link again and nothing would ever be
                    // followed.
                    let Ok(new_metadata) = std::fs::metadata(&path) else {
                        observer.err();
                        continue;
                    };
                    ft = new_metadata.file_type();
                    metadata = Some(new_metadata);

                    // the extension check above only ran for entries that were
                    // already known to be files, so redo it for a link that
                    // turned out to point at one
                    if ft.is_file() {
                        ext = match path
                            .extension()
                            .and_then(OsStr::to_str)
                            .and_then(|s| FileType::from_str(s).ok())
                        {
                            Some(ext) if self.extensions.contains(&ext) => Some(ext),
                            _ => continue,
                        };
                    }
                }

                if ft.is_dir() {
                    if self.recurse && visited.insert(path.clone()) {
                        pending_dirs.push((depth + 1, path));
                    }

                    continue;
                }

                if !ft.is_file() || depth < self.min_depth {
                    continue;
                }

                // Some() for every path that reaches here: plain files got it
                // from the pre-metadata check, followed links from the re-check
                let Some(ext) = ext else { continue };

                // before the metadata call, which is the expensive part here
                if self.skip_by_name(&path, ext) {
                    observer.excluded.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let metadata = match metadata {
                    Some(m) => m,
                    None => match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            observer.err();
                            continue;
                        }
                    },
                };

                if !(self.min_size..=self.max_size).contains(&metadata.len()) {
                    continue;
                }

                let f = observer.files.get(ext);

                f.found.fetch_add(1, Ordering::Relaxed);
                f.bytes.fetch_add(metadata.len(), Ordering::Relaxed);

                current_files.push(FileEntry::new(path, ext, metadata));
            }

            files.append(&mut current_files);
        }

        match (self.sort, self.sort_order) {
            (SortMethod::Name, SortOrder::Asc) => files.sort_by(|a, b| a.path.cmp(&b.path)),
            (SortMethod::Name, SortOrder::Desc) => files.sort_by(|a, b| b.path.cmp(&a.path)),

            (SortMethod::Size, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.len()),
            (SortMethod::Size, SortOrder::Desc) => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.len())),

            (SortMethod::ATime, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.accessed().ok()),
            (SortMethod::CTime, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.created().ok()),
            (SortMethod::MTime, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.modified().ok()),

            (SortMethod::ATime, SortOrder::Desc) => {
                files.sort_by_key(|f| std::cmp::Reverse(f.metadata.accessed().ok()))
            }
            (SortMethod::CTime, SortOrder::Desc) => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.created().ok())),
            (SortMethod::MTime, SortOrder::Desc) => {
                files.sort_by_key(|f| std::cmp::Reverse(f.metadata.modified().ok()))
            }

            (SortMethod::None, _) => {}
        }

        if self.randomize > 0.0 {
            use rand::{RngExt as _, SeedableRng, rngs::SmallRng, seq::SliceRandom};

            let mut rng = SmallRng::from_rng(&mut rand::rng());

            if self.randomize >= 1.0 {
                files.shuffle(&mut rng);
            } else {
                // partial shuffle based on the randomization factor, using a variant of the Fisher-Yates shuffle
                // and an offset based on the randomization factor to control the degree of shuffling
                // from nearby to fully random
                let width = ((self.randomize * files.len() as f64).ceil() as usize).max(1);

                for i in (1..files.len()).rev() {
                    if rng.random_bool(self.randomize) {
                        let start = i.saturating_sub(width);
                        files.swap(i, rng.random_range(start..=i));
                    }
                }
            }
        }

        if let Some(limit) = self.limit {
            files.truncate(limit);
        }

        let mut progress: PerFileType<Box<ConversionProgress>> = PerFileType::default();

        let mut final_counts = PerFileType::<(u64, u64)>::default(); // (count, bytes)

        for file in &files {
            let progress = progress.get_mut(file.ext);

            *progress.total_bytes.get_mut() += file.metadata.len();
            *progress.total.get_mut() += 1;

            let (count, bytes) = final_counts.get_mut(file.ext);

            *count += 1;
            *bytes += file.metadata.len();
        }

        for (ext, &(count, bytes)) in final_counts.iter() {
            let p = observer.files.get(ext);

            p.bytes.store(bytes, Ordering::Relaxed);
            p.found.store(count, Ordering::Relaxed);
        }

        // Move the locally-built Vec into the concurrent, stable-indexed
        // boxcar::Vec that workers (and the watcher's promoter, in watch mode)
        // share. From here on, growth happens only via push from the promoter.
        let concurrent_files: boxcar::Vec<FileEntry> = boxcar::Vec::new();
        for entry in files {
            concurrent_files.push(entry);
        }

        ConversionState {
            excluded: observer.excluded.load(Ordering::Relaxed) as usize,
            files: concurrent_files,
            idx: AtomicUsize::new(0),
            active: Vec::from_iter((0..self.parallel).map(|_| ThreadState {
                file_idx: AtomicUsize::new(usize::MAX),
                start_time: AtomicU64::new(0),
                quality: AtomicU8::new(QUALITY_UNSET),
            })),
            non_success: Default::default(),
            progress,
            paused: Default::default(),
            shutdown: Default::default(),
            wake: Default::default(),
            produced: Default::default(),
            logs: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(dir: &Path, flags: &[&str]) -> (Vec<String>, u64) {
        let mut argv: Vec<&str> = flags.to_vec();
        argv.push(dir.to_str().unwrap());

        let mut args = <Conv2JxlArgs as argh::FromArgs>::from_args(&["conv2jxl"], &argv).unwrap();
        args.normalize();

        let observer = ScanObserver::default();
        let state = args.scan(&observer);

        let mut names: Vec<String> = state
            .files
            .iter()
            .map(|(_, f)| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();

        (names, observer.excluded.load(Ordering::Relaxed))
    }

    #[test]
    fn recompressed_jpegs_are_skipped_by_name() {
        let dir = tempfile::tempdir().unwrap();

        // contents do not matter, the scan never opens them
        for name in ["a.jpeg.jxl", "b.JPG.jxl", "c.png.jxl", "d.jxl", "e.jpg.png"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        assert_eq!(
            scanned(dir.path(), &["--ext", "jxl"]),
            (vec!["c.png.jxl".to_owned(), "d.jxl".to_owned()], 2)
        );

        // the override brings them back
        assert_eq!(scanned(dir.path(), &["--ext", "jxl", "--reencode-lossy-jxl"]).0.len(), 4);

        // other formats are untouched by the rule, whatever their inner extension
        assert_eq!(scanned(dir.path(), &["--ext", "png"]), (vec!["e.jpg.png".to_owned()], 0));
    }
}
