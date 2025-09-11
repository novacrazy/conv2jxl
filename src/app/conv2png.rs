use std::{fs::File, io::BufReader, io::Write, path::Path};

use image::{ImageDecoder, ImageEncoder};
use tempfile::NamedTempFile;

use crate::cli::FileType;

pub fn conv2png(path: &Path, ext: FileType) -> Result<NamedTempFile, Box<dyn std::error::Error>> {
    let file = BufReader::new(File::open(path)?);
    let mut tmp = NamedTempFile::new()?;

    let mut decoder: Box<dyn ImageDecoder> = match ext {
        FileType::TIFF => Box::new(image::codecs::tiff::TiffDecoder::new(file)?),
        FileType::TGA => Box::new(image::codecs::tga::TgaDecoder::new(file)?),
        FileType::QOI => Box::new(image::codecs::qoi::QoiDecoder::new(file)?),
        FileType::PNG => Box::new(image::codecs::png::PngDecoder::new(file)?),
        FileType::BMP => Box::new(image::codecs::bmp::BmpDecoder::new(file)?),
        _ => return Err(format!("Unsupported file type for conversion to PNG: {:?}", ext).into()),
    };

    let dimensions = decoder.dimensions();
    let color_type = decoder.color_type();
    let icc_profile = decoder.icc_profile()?;

    let mut bytes = vec![0; decoder.total_bytes() as usize];
    decoder.read_image(&mut bytes)?;

    // fast compression, no filter, as cjxl will do its own compression
    let mut encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut tmp,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    );

    if let Some(icc_profile) = icc_profile {
        encoder.set_icc_profile(icc_profile)?;
    }

    encoder.write_image(&bytes, dimensions.0, dimensions.1, color_type.into())?;

    tmp.flush()?;

    Ok(tmp)
}
