//! Optional WebP preparation for chat image attachments.
//!
//! The picked file remains the source of truth. A prepared image is a separate
//! temporary file and exists only when it is smaller than the original.

use std::path::Path;

use image::ImageDecoder as _;

const MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODE_ALLOCATION: u64 = 128 * 1024 * 1024;
const OUTPUT_MAX_DIMENSION: u32 = 1_600;
const WEBP_QUALITY: f32 = 85.0;

/// Encoded WebP retained until its pending attachment is removed or sent.
pub struct PreparedAttachment {
    path: tempfile::TempPath,
    size_bytes: u64,
}

impl PreparedAttachment {
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

/// Decode a supported image, apply EXIF orientation, and encode a lossy WebP
/// at the same dimensions and quality used by the web chat composer. Failure,
/// unsupported formats, and outputs that are not smaller all return `None`;
/// callers can keep using the original file without a special fallback path.
pub fn prepare(source: &Path) -> Option<PreparedAttachment> {
    let metadata = std::fs::metadata(source).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_INPUT_BYTES {
        return None;
    }
    let original_size = metadata.len();

    let mut reader = image::ImageReader::open(source)
        .ok()?
        .with_guessed_format()
        .ok()?;
    if !matches!(
        reader.format()?,
        image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP
    ) {
        return None;
    }

    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOCATION);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return None;
    }

    let orientation = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .as_deref()
        .and_then(image::metadata::Orientation::from_exif_chunk)
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded = image::DynamicImage::from_decoder(decoder).ok()?;
    decoded.apply_orientation(orientation);

    let scale =
        (OUTPUT_MAX_DIMENSION as f64 / f64::from(decoded.width().max(decoded.height()))).min(1.0);
    let output_width = (f64::from(decoded.width()) * scale).round().max(1.0) as u32;
    let output_height = (f64::from(decoded.height()) * scale).round().max(1.0) as u32;
    let pixels = if output_width == decoded.width() && output_height == decoded.height() {
        decoded.to_rgba8()
    } else {
        decoded
            .resize_exact(
                output_width,
                output_height,
                image::imageops::FilterType::Lanczos3,
            )
            .to_rgba8()
    };
    let encoded = webp::Encoder::from_rgba(pixels.as_raw(), output_width, output_height)
        .encode_simple(false, WEBP_QUALITY)
        .ok()?;
    if encoded.is_empty() || encoded.len() as u64 >= original_size {
        return None;
    }

    let mut output = tempfile::Builder::new()
        .prefix(".friendshub-chat-image-")
        .suffix(".webp")
        .tempfile_in(source.parent()?)
        .ok()?;
    use std::io::Write as _;
    output.write_all(&encoded).ok()?;
    output.flush().ok()?;

    Some(PreparedAttachment {
        path: output.into_temp_path(),
        size_bytes: encoded.len() as u64,
    })
}
