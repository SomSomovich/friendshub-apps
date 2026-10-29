//! Local processing for group and channel avatar images before the core upload.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageFormat, Limits};

const MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const OUTPUT_DIMENSION: u32 = 512;
const MAX_DECODE_ALLOCATION: u64 = 128 * 1024 * 1024;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum PreparationError {
    InputTooLarge,
    UnsupportedFormat,
    DimensionsTooLarge,
    InvalidImage,
    OutputTooLarge,
    Io(std::io::Error),
}

impl PreparationError {
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::InputTooLarge => "Исходное изображение должно быть не больше 20 МБ.",
            Self::UnsupportedFormat => "Поддерживаются изображения PNG, JPEG и WebP.",
            Self::DimensionsTooLarge => "Размеры изображения слишком велики для обработки.",
            Self::InvalidImage => "Не удалось прочитать изображение. Выберите другой файл.",
            Self::OutputTooLarge => "После обработки изображение превышает лимит 5 МБ.",
            Self::Io(_) => "Не удалось подготовить изображение для загрузки.",
        }
    }
}

/// WebP output kept in the file-picker's private directory through core upload.
pub struct PreparedAvatar {
    path: PathBuf,
    bytes: Vec<u8>,
}

impl PreparedAvatar {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for PreparedAvatar {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Validates the actual file contents, applies EXIF orientation, scales to fit
/// within 512x512, and writes a lossless WebP beside the imported picker file.
pub fn prepare(source: &Path) -> Result<PreparedAvatar, PreparationError> {
    let metadata = fs::metadata(source).map_err(PreparationError::Io)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_INPUT_BYTES {
        return Err(PreparationError::InputTooLarge);
    }

    let probe = image::ImageReader::open(source)
        .map_err(|_| PreparationError::InvalidImage)?
        .with_guessed_format()
        .map_err(|_| PreparationError::InvalidImage)?;
    let format = probe.format().ok_or(PreparationError::UnsupportedFormat)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err(PreparationError::UnsupportedFormat);
    }
    let (width, height) = probe
        .into_dimensions()
        .map_err(|_| PreparationError::InvalidImage)?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(PreparationError::DimensionsTooLarge);
    }

    let mut reader = image::ImageReader::open(source)
        .map_err(|_| PreparationError::InvalidImage)?
        .with_guessed_format()
        .map_err(|_| PreparationError::InvalidImage)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOCATION);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| PreparationError::InvalidImage)?;
    let orientation = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .as_deref()
        .and_then(image::metadata::Orientation::from_exif_chunk)
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded =
        DynamicImage::from_decoder(decoder).map_err(|_| PreparationError::InvalidImage)?;
    decoded.apply_orientation(orientation);

    let resized = decoded
        .thumbnail(OUTPUT_DIMENSION, OUTPUT_DIMENSION)
        .to_rgba8();
    let mut webp_bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes)
        .encode(
            resized.as_raw(),
            resized.width(),
            resized.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|_| PreparationError::InvalidImage)?;
    if webp_bytes.len() > MAX_OUTPUT_BYTES {
        return Err(PreparationError::OutputTooLarge);
    }

    let parent = source.parent().ok_or(PreparationError::InvalidImage)?;
    let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = parent.join(format!(".friendshub-avatar-{id}.webp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(PreparationError::Io)?;
    if let Err(error) = file.write_all(&webp_bytes).and_then(|()| file.flush()) {
        let _ = fs::remove_file(&path);
        return Err(PreparationError::Io(error));
    }

    Ok(PreparedAvatar {
        path,
        bytes: webp_bytes,
    })
}
