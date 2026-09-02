use crate::command;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits, RgbImage, Rgba, RgbaImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::FilterType,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_DIMENSION: u32 = 2_048;
pub const DEFAULT_BATCH_MAX_DIMENSION: u32 = 1_600;
pub const MAX_REQUESTED_DIMENSION: u32 = 4_096;
pub const DEFAULT_JPEG_QUALITY: u8 = 88;
pub const MIN_JPEG_QUALITY: u8 = 45;
pub const MAX_JPEG_QUALITY: u8 = 95;
pub const MAX_BATCH_IMAGES: usize = 6;
pub const MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_SOURCE_PIXELS: u64 = 120_000_000;
pub const MAX_SOURCE_DIMENSION: u32 = 50_000;
pub const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_SINGLE_ENCODED_BYTES: usize = 1_500_000;
pub const MAX_BATCH_IMAGE_ENCODED_BYTES: usize = 900_000;
pub const MAX_BATCH_ENCODED_BYTES: usize = 5_400_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedImageMetadata {
    pub path: String,
    pub source_width: u32,
    pub source_height: u32,
    pub width: u32,
    pub height: u32,
    pub source_bytes: u64,
    pub encoded_bytes: usize,
    pub mime_type: &'static str,
    pub resized: bool,
    pub orientation_applied: bool,
}

#[derive(Debug)]
pub struct PreparedImage {
    pub metadata: PreparedImageMetadata,
    pub base64_data: String,
}

fn canonicalize(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|error| format!("Cannot resolve image path {}: {error}", path.display()))
}

/// Resolve a local image path. Relative paths stay contained in the workspace;
/// an explicit absolute path may point anywhere readable by the MoonDesk user.
pub fn resolve_image_path(
    workspace_root: &str,
    requested: &str,
    allow_external_absolute: bool,
) -> Result<PathBuf, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("Image path must not be empty".to_string());
    }

    let requested_path = Path::new(requested);
    let resolved = if requested_path.is_absolute() {
        let candidate = canonicalize(requested_path)?;
        if allow_external_absolute {
            candidate
        } else {
            let root = canonicalize(Path::new(workspace_root))?;
            if !candidate.starts_with(&root) {
                return Err(format!(
                    "Absolute image path is outside the workspace in read-only mode: {requested}"
                ));
            }
            candidate
        }
    } else {
        let root = canonicalize(Path::new(workspace_root))?;
        let candidate = canonicalize(&root.join(requested_path))?;
        if !candidate.starts_with(&root) {
            return Err(format!(
                "Relative image path escapes the workspace: {requested}. Use an explicit absolute path only when MoonDesk is not in read-only mode and the task genuinely requires reading an image outside the workspace."
            ));
        }
        candidate
    };

    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| format!("Cannot inspect image {}: {error}", resolved.display()))?;
    if !metadata.is_file() {
        return Err(format!("Image path is not a file: {}", resolved.display()));
    }
    Ok(resolved)
}

fn validate_source_size(path: &Path) -> Result<u64, String> {
    let bytes = std::fs::metadata(path)
        .map_err(|error| format!("Cannot inspect image {}: {error}", path.display()))?
        .len();
    if bytes > MAX_INPUT_BYTES {
        return Err(format!(
            "Image is too large to inspect safely: {} bytes (maximum {} bytes)",
            bytes, MAX_INPUT_BYTES
        ));
    }
    Ok(bytes)
}

fn decode_oriented(path: &Path) -> Result<(DynamicImage, ImageFormat, u32, u32, bool), String> {
    let mut reader = ImageReader::open(path)
        .map_err(|error| format!("Cannot open image {}: {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("Cannot detect image format {}: {error}", path.display()))?;

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);

    let format = reader
        .format()
        .ok_or_else(|| format!("Unsupported or unknown image format: {}", path.display()))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("Cannot decode image {}: {error}", path.display()))?;
    let (source_width, source_height) = decoder.dimensions();
    let pixels = u64::from(source_width).saturating_mul(u64::from(source_height));
    if pixels > MAX_SOURCE_PIXELS {
        return Err(format!(
            "Image dimensions are too large to inspect safely: {source_width}x{source_height} ({pixels} pixels, maximum {MAX_SOURCE_PIXELS})"
        ));
    }

    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let needs_orientation = orientation != image::metadata::Orientation::NoTransforms;
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("Cannot decode image {}: {error}", path.display()))?;
    image.apply_orientation(orientation);
    Ok((
        image,
        format,
        source_width,
        source_height,
        needs_orientation,
    ))
}

fn resize_to_bound(image: &DynamicImage, max_dimension: u32) -> DynamicImage {
    let (width, height) = image.dimensions();
    if width <= max_dimension && height <= max_dimension {
        return image.clone();
    }
    image.resize(max_dimension, max_dimension, FilterType::Lanczos3)
}

fn flatten_to_rgb(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut canvas = RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]));
    image::imageops::overlay(&mut canvas, &rgba, 0, 0);
    DynamicImage::ImageRgba8(canvas).to_rgb8()
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let rgba = image.to_rgba8();
    let mut output = Vec::new();
    PngEncoder::new(&mut output)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("Cannot encode PNG preview: {error}"))?;
    Ok(output)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let rgb = flatten_to_rgb(image);
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, quality)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("Cannot encode JPEG preview: {error}"))?;
    Ok(output)
}

fn encode_bounded_jpeg(
    mut image: DynamicImage,
    requested_quality: u8,
    target_bytes: usize,
) -> Result<(DynamicImage, Vec<u8>), String> {
    let quality_steps = [
        requested_quality,
        requested_quality.min(82),
        requested_quality.min(74),
        requested_quality.min(66),
        requested_quality.min(58),
        MIN_JPEG_QUALITY,
    ];

    loop {
        let mut previous_quality = None;
        let mut last_encoded = None;
        for quality in quality_steps {
            if previous_quality == Some(quality) {
                continue;
            }
            previous_quality = Some(quality);
            let encoded = encode_jpeg(&image, quality)?;
            if encoded.len() <= target_bytes {
                return Ok((image, encoded));
            }
            last_encoded = Some(encoded);
        }

        let (width, height) = image.dimensions();
        if width <= 512 && height <= 512 {
            return Ok((image, last_encoded.unwrap_or_default()));
        }
        let next_width = ((width as f64) * 0.78).round().max(1.0) as u32;
        let next_height = ((height as f64) * 0.78).round().max(1.0) as u32;
        image = image.resize_exact(next_width, next_height, FilterType::Lanczos3);
    }
}

pub fn prepare_image(
    workspace_root: &str,
    requested_path: &str,
    max_dimension: u32,
    jpeg_quality: u8,
    target_bytes: usize,
    allow_external_absolute: bool,
) -> Result<PreparedImage, String> {
    if !(1..=MAX_REQUESTED_DIMENSION).contains(&max_dimension) {
        return Err(format!(
            "max_dimension must be between 1 and {MAX_REQUESTED_DIMENSION}"
        ));
    }
    if !(MIN_JPEG_QUALITY..=MAX_JPEG_QUALITY).contains(&jpeg_quality) {
        return Err(format!(
            "quality must be between {MIN_JPEG_QUALITY} and {MAX_JPEG_QUALITY}"
        ));
    }

    let path = resolve_image_path(workspace_root, requested_path, allow_external_absolute)?;
    let source_bytes = validate_source_size(&path)?;
    let (decoded, format, source_width, source_height, needs_orientation) = decode_oriented(&path)?;
    let oriented_source_dimensions = decoded.dimensions();

    let direct_mime_type = match format {
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Png => Some("image/png"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    };
    if !needs_orientation
        && oriented_source_dimensions.0 <= max_dimension
        && oriented_source_dimensions.1 <= max_dimension
        && source_bytes <= target_bytes as u64
        && let Some(mime_type) = direct_mime_type
    {
        let encoded = std::fs::read(&path)
            .map_err(|error| format!("Cannot read image {}: {error}", path.display()))?;
        let metadata = PreparedImageMetadata {
            path: path.to_string_lossy().into_owned(),
            source_width,
            source_height,
            width: oriented_source_dimensions.0,
            height: oriented_source_dimensions.1,
            source_bytes,
            encoded_bytes: encoded.len(),
            mime_type,
            resized: false,
            orientation_applied: false,
        };
        return Ok(PreparedImage {
            metadata,
            base64_data: BASE64_STANDARD.encode(encoded),
        });
    }

    let resized = resize_to_bound(&decoded, max_dimension);

    let (final_image, encoded, mime_type) = if format == ImageFormat::Png {
        let png = encode_png(&resized)?;
        if png.len() <= target_bytes {
            (resized, png, "image/png")
        } else {
            let (image, jpeg) = encode_bounded_jpeg(resized, jpeg_quality, target_bytes)?;
            (image, jpeg, "image/jpeg")
        }
    } else {
        let (image, jpeg) = encode_bounded_jpeg(resized, jpeg_quality, target_bytes)?;
        (image, jpeg, "image/jpeg")
    };

    if encoded.is_empty() || encoded.len() > target_bytes {
        return Err(format!(
            "Could not reduce image preview below the {target_bytes}-byte response budget"
        ));
    }

    let (width, height) = final_image.dimensions();
    let metadata = PreparedImageMetadata {
        path: path.to_string_lossy().into_owned(),
        source_width,
        source_height,
        width,
        height,
        source_bytes,
        encoded_bytes: encoded.len(),
        mime_type,
        resized: oriented_source_dimensions != (width, height),
        orientation_applied: needs_orientation,
    };
    Ok(PreparedImage {
        metadata,
        base64_data: BASE64_STANDARD.encode(encoded),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::io::Cursor;
    use uuid::Uuid;

    fn temp_workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("moondesk-vision-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp workspace");
        root
    }

    #[test]
    fn relative_paths_cannot_escape_workspace() {
        let root = temp_workspace();
        let outside = root
            .parent()
            .expect("temp parent")
            .join(format!("outside-{}.png", Uuid::new_v4()));
        std::fs::write(&outside, b"not-an-image").expect("write outside fixture");
        let relative = Path::new("..")
            .join(outside.file_name().expect("outside fixture name"))
            .to_string_lossy()
            .into_owned();
        let error = resolve_image_path(&root.to_string_lossy(), &relative, false)
            .expect_err("relative escape must be rejected");
        assert!(error.contains("escapes the workspace"));
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_absolute_path_can_be_read_outside_workspace() {
        let root = temp_workspace();
        let outside = std::env::temp_dir().join(format!("outside-{}.png", Uuid::new_v4()));
        std::fs::write(&outside, b"not-an-image").expect("write outside fixture");
        let resolved =
            resolve_image_path(&root.to_string_lossy(), &outside.to_string_lossy(), true)
                .expect("explicit absolute path should resolve");
        assert_eq!(resolved, canonicalize(&outside).expect("canonical outside"));
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_absolute_path_does_not_require_workspace_root_to_exist() {
        let missing_root =
            std::env::temp_dir().join(format!("missing-workspace-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("outside-{}.png", Uuid::new_v4()));
        std::fs::write(&outside, b"not-an-image").expect("write outside fixture");
        let resolved = resolve_image_path(
            &missing_root.to_string_lossy(),
            &outside.to_string_lossy(),
            true,
        )
        .expect("explicit absolute path must not depend on workspace availability");
        assert_eq!(resolved, canonicalize(&outside).expect("canonical outside"));
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn read_only_path_policy_rejects_absolute_path_outside_workspace() {
        let root = temp_workspace();
        let outside = std::env::temp_dir().join(format!("outside-{}.png", Uuid::new_v4()));
        std::fs::write(&outside, b"not-an-image").expect("write outside fixture");
        let error = resolve_image_path(&root.to_string_lossy(), &outside.to_string_lossy(), false)
            .expect_err("read-only path policy must reject external absolute image paths");
        assert!(error.contains("outside the workspace in read-only mode"));
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn in_budget_jpeg_is_attached_without_reencoding() {
        let root = temp_workspace();
        let path = root.join("direct.jpg");
        let pixels = RgbImage::from_pixel(64, 48, Rgb([120, 80, 40]));
        let mut original = Vec::new();
        JpegEncoder::new_with_quality(Cursor::new(&mut original), 91)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode direct jpeg fixture");
        std::fs::write(&path, &original).expect("write direct jpeg fixture");

        let prepared = prepare_image(
            &root.to_string_lossy(),
            "direct.jpg",
            DEFAULT_MAX_DIMENSION,
            DEFAULT_JPEG_QUALITY,
            MAX_SINGLE_ENCODED_BYTES,
            true,
        )
        .expect("prepare direct jpeg");
        let attached = BASE64_STANDARD
            .decode(&prepared.base64_data)
            .expect("decode attached jpeg");
        assert_eq!(
            attached, original,
            "in-budget JPEG bytes should pass through unchanged"
        );
        assert_eq!(prepared.metadata.mime_type, "image/jpeg");
        assert!(!prepared.metadata.resized);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn large_jpeg_is_resized_and_bounded_for_mcp_vision() {
        let root = temp_workspace();
        let path = root.join("photo.jpg");
        let mut pixels = RgbImage::new(3_000, 2_000);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = Rgb([
                ((x * 17 + y * 3) % 255) as u8,
                ((x * 5 + y * 11) % 255) as u8,
                ((x * 13 + y * 7) % 255) as u8,
            ]);
        }
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(Cursor::new(&mut bytes), 95)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode fixture");
        std::fs::write(&path, bytes).expect("write jpeg fixture");

        let prepared = prepare_image(
            &root.to_string_lossy(),
            "photo.jpg",
            1_200,
            DEFAULT_JPEG_QUALITY,
            350_000,
            true,
        )
        .expect("prepare image");
        assert_eq!(prepared.metadata.mime_type, "image/jpeg");
        assert!(prepared.metadata.width <= 1_200);
        assert!(prepared.metadata.height <= 1_200);
        assert!(prepared.metadata.encoded_bytes <= 350_000);
        assert!(prepared.metadata.resized);
        assert!(!prepared.metadata.orientation_applied);
        assert!(!prepared.base64_data.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn common_formats_are_attached_directly_or_converted_safely() {
        let root = temp_workspace();
        let pixels = RgbImage::from_pixel(48, 36, Rgb([33, 144, 211]));
        for (name, format, expected_mime, passthrough) in [
            ("sample.png", ImageFormat::Png, "image/png", true),
            ("sample.webp", ImageFormat::WebP, "image/webp", true),
            ("sample.bmp", ImageFormat::Bmp, "image/jpeg", false),
            ("sample.gif", ImageFormat::Gif, "image/jpeg", false),
        ] {
            let path = root.join(name);
            DynamicImage::ImageRgb8(pixels.clone())
                .save_with_format(&path, format)
                .unwrap_or_else(|error| panic!("write {name} fixture: {error}"));
            let original = std::fs::read(&path).expect("read image fixture");
            let prepared = prepare_image(
                &root.to_string_lossy(),
                name,
                DEFAULT_MAX_DIMENSION,
                DEFAULT_JPEG_QUALITY,
                MAX_SINGLE_ENCODED_BYTES,
                true,
            )
            .unwrap_or_else(|error| panic!("prepare {name}: {error}"));
            let attached = BASE64_STANDARD
                .decode(&prepared.base64_data)
                .expect("decode attached image");
            assert_eq!(prepared.metadata.mime_type, expected_mime, "{name}");
            assert!(!prepared.metadata.resized, "{name}");
            assert!(!prepared.metadata.orientation_applied, "{name}");
            if passthrough {
                assert_eq!(attached, original, "{name} should pass through unchanged");
            } else {
                assert!(
                    attached.starts_with(&[0xFF, 0xD8]),
                    "{name} should be converted to a JPEG vision payload"
                );
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    fn add_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(b"II");
        payload.extend_from_slice(&42u16.to_le_bytes());
        payload.extend_from_slice(&8u32.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0x0112u16.to_le_bytes());
        payload.extend_from_slice(&3u16.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&orientation.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        let segment_len = u16::try_from(payload.len() + 2).expect("EXIF fixture length");

        let mut output = Vec::with_capacity(jpeg.len() + payload.len() + 4);
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&[0xFF, 0xE1]);
        output.extend_from_slice(&segment_len.to_be_bytes());
        output.extend_from_slice(&payload);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    #[test]
    fn exif_orientation_is_applied_before_attaching_pixels() {
        let root = temp_workspace();
        let path = root.join("rotated.jpg");
        let pixels = RgbImage::from_pixel(40, 20, Rgb([210, 70, 30]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), 90)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode oriented JPEG fixture");
        let oriented = add_exif_orientation(&jpeg, 6);
        std::fs::write(&path, oriented).expect("write oriented JPEG fixture");

        let prepared = prepare_image(
            &root.to_string_lossy(),
            "rotated.jpg",
            DEFAULT_MAX_DIMENSION,
            DEFAULT_JPEG_QUALITY,
            MAX_SINGLE_ENCODED_BYTES,
            true,
        )
        .expect("prepare oriented JPEG");
        assert_eq!(prepared.metadata.source_width, 40);
        assert_eq!(prepared.metadata.source_height, 20);
        assert_eq!(prepared.metadata.width, 20);
        assert_eq!(prepared.metadata.height, 40);
        assert!(prepared.metadata.orientation_applied);
        assert!(!prepared.metadata.resized);
        let attached = BASE64_STANDARD
            .decode(&prepared.base64_data)
            .expect("decode oriented attachment");
        let decoded = image::load_from_memory(&attached).expect("decode oriented result");
        assert_eq!(decoded.dimensions(), (20, 40));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_images_fail_without_panicking() {
        let root = temp_workspace();
        std::fs::write(root.join("broken.jpg"), b"this is not actually a jpeg")
            .expect("write corrupt fixture");
        let error = prepare_image(
            &root.to_string_lossy(),
            "broken.jpg",
            DEFAULT_MAX_DIMENSION,
            DEFAULT_JPEG_QUALITY,
            MAX_SINGLE_ENCODED_BYTES,
            true,
        )
        .expect_err("corrupt image must fail");
        assert!(
            error.contains("Cannot decode image") || error.contains("Unsupported or unknown image")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_source_file_is_rejected_before_decode() {
        let root = temp_workspace();
        let path = root.join("too-large.jpg");
        let file = std::fs::File::create(&path).expect("create sparse oversized fixture");
        file.set_len(MAX_INPUT_BYTES + 1)
            .expect("size sparse oversized fixture");
        let error = prepare_image(
            &root.to_string_lossy(),
            "too-large.jpg",
            DEFAULT_MAX_DIMENSION,
            DEFAULT_JPEG_QUALITY,
            MAX_SINGLE_ENCODED_BYTES,
            true,
        )
        .expect_err("oversized source must be rejected");
        assert!(error.contains("Image is too large to inspect safely"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_preview_bounds_are_rejected() {
        let root = temp_workspace();
        let path = root.join("small.jpg");
        DynamicImage::ImageRgb8(RgbImage::from_pixel(16, 16, Rgb([1, 2, 3])))
            .save_with_format(&path, ImageFormat::Jpeg)
            .expect("write small JPEG fixture");

        let dimension_error = prepare_image(
            &root.to_string_lossy(),
            "small.jpg",
            0,
            DEFAULT_JPEG_QUALITY,
            MAX_SINGLE_ENCODED_BYTES,
            true,
        )
        .expect_err("zero max dimension must fail");
        assert!(dimension_error.contains("max_dimension must be between"));

        let quality_error = prepare_image(
            &root.to_string_lossy(),
            "small.jpg",
            DEFAULT_MAX_DIMENSION,
            MIN_JPEG_QUALITY - 1,
            MAX_SINGLE_ENCODED_BYTES,
            true,
        )
        .expect_err("too-low quality must fail");
        assert!(quality_error.contains("quality must be between"));

        let _ = std::fs::remove_dir_all(root);
    }
}
