//! Image cropping and encoding shared by the screenshot and locate handlers.
//!
//! Both handlers turn the RDP framebuffer into bytes, and both accept a region.
//! Keeping that in one place means a region crop cannot behave one way for
//! `screenshot` and another for `locate` - which would be the worst possible
//! outcome, since the whole point of `locate --region` is that its coordinates
//! agree with what a `screenshot --region` shows.

use std::io::Cursor;

use agent_rdp_protocol::{ImageFormat, Region};
use image::{ImageFormat as ImgFormat, RgbaImage};

/// Crop `image` to `region`, trimming the region to the image bounds.
///
/// Returns the cropped image together with the region actually used - callers
/// need that, not the requested region, because the offset they report back has
/// to match the pixels they are returning.
///
/// A region with no overlap at all is an error rather than a silent fall back
/// to the full image: a caller that asked for one row of a table would
/// otherwise get the whole desktop and no indication that anything was wrong.
pub fn crop_to_region(image: &RgbaImage, region: Region) -> Result<(RgbaImage, Region), String> {
    let (width, height) = (image.width(), image.height());

    let clamped = region.clamp_to(width, height).ok_or_else(|| {
        format!(
            "Region {}x{} at ({}, {}) lies outside the {}x{} desktop",
            region.width, region.height, region.x, region.y, width, height
        )
    })?;

    let cropped = image::imageops::crop_imm(
        image,
        clamped.x,
        clamped.y,
        clamped.width,
        clamped.height,
    )
    .to_image();

    Ok((cropped, clamped))
}

/// Encode an RGBA image to PNG or JPEG.
///
/// JPEG has no alpha channel, so the image crate refuses to encode an RGBA
/// buffer as JPEG. Dropping to RGB first is what makes `--format jpeg` work at
/// all; the framebuffer is fully opaque, so nothing is lost.
pub fn encode_image(image: &RgbaImage, format: ImageFormat) -> Result<Vec<u8>, String> {
    // Pre-size for the common case: a PNG/JPEG of a desktop is far smaller
    // than the raw buffer, but starts empty and would otherwise grow through
    // a dozen reallocs of multi-MB payloads.
    let estimate = (image.as_raw().len() / 4).max(16 * 1024);
    let mut buffer = Cursor::new(Vec::with_capacity(estimate));

    let result = match format {
        ImageFormat::Png => image.write_to(&mut buffer, ImgFormat::Png),
        ImageFormat::Jpeg => {
            // Pack RGBA to RGB directly instead of cloning the whole RGBA
            // buffer into a DynamicImage first - the clone was a full extra
            // framebuffer copy per JPEG screenshot.
            let mut rgb = Vec::with_capacity(image.as_raw().len() / 4 * 3);
            for px in image.as_raw().chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            let rgb = image::RgbImage::from_raw(image.width(), image.height(), rgb)
                .ok_or_else(|| "RGB conversion produced a mis-sized buffer".to_string())?;
            rgb.write_to(&mut buffer, ImgFormat::Jpeg)
        }
    };

    result.map_err(|e| format!("Failed to encode image: {}", e))?;
    Ok(buffer.into_inner())
}

/// FNV-1a 64-bit hash of raw pixel bytes, as a 16-hex-digit string.
///
/// Not for security - it's the cheap, dependency-free way to give a
/// screenshot a content fingerprint. Two screenshots that hash the same are
/// pixel-identical; a caller that expected a UI change but sees the same hash
/// (and the same `frame_seq`) knows the frame is stale rather than having to
/// infer it from wall-clock time or an external md5 of the saved file, which
/// is what QA was doing by hand before this existed.
pub fn hash_pixels(data: &[u8]) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// An image where every pixel encodes its own coordinates, so a crop that
    /// reads from the wrong place is detectable by looking at the pixels.
    /// Without this, a test that only checks dimensions would pass even if x
    /// and y were swapped.
    fn coordinate_image(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, (x / 256) as u8, 255])
        })
    }

    fn assert_crop_matches_source(source: &RgbaImage, cropped: &RgbaImage, at: Region) {
        for y in 0..cropped.height() {
            for x in 0..cropped.width() {
                assert_eq!(
                    cropped.get_pixel(x, y),
                    source.get_pixel(at.x + x, at.y + y),
                    "pixel ({}, {}) of the crop should come from ({}, {}) of the source",
                    x,
                    y,
                    at.x + x,
                    at.y + y
                );
            }
        }
    }

    // --- crop_to_region: positive cases ---

    #[test]
    fn test_crop_takes_pixels_from_the_requested_place() {
        let source = coordinate_image(1280, 800);
        let region = Region { x: 100, y: 380, width: 400, height: 30 };

        let (cropped, used) = crop_to_region(&source, region).unwrap();

        assert_eq!((cropped.width(), cropped.height()), (400, 30));
        assert_eq!(used, region);
        assert_crop_matches_source(&source, &cropped, used);
    }

    #[test]
    fn test_crop_is_not_transposed() {
        // A deliberately non-square region off the diagonal: if x and y were
        // swapped anywhere, the dimensions alone would give it away.
        let source = coordinate_image(640, 480);
        let region = Region { x: 500, y: 30, width: 100, height: 20 };

        let (cropped, used) = crop_to_region(&source, region).unwrap();

        assert_eq!((cropped.width(), cropped.height()), (100, 20));
        assert_eq!(*cropped.get_pixel(0, 0), *source.get_pixel(500, 30));
        assert_crop_matches_source(&source, &cropped, used);
    }

    #[test]
    fn test_crop_full_image_is_identity() {
        let source = coordinate_image(64, 48);
        let region = Region { x: 0, y: 0, width: 64, height: 48 };

        let (cropped, used) = crop_to_region(&source, region).unwrap();

        assert_eq!(used, region);
        assert_eq!(cropped.as_raw(), source.as_raw());
    }

    #[test]
    fn test_crop_single_pixel() {
        let source = coordinate_image(1280, 800);
        let (cropped, used) = crop_to_region(&source, Region { x: 640, y: 400, width: 1, height: 1 })
            .unwrap();

        assert_eq!((cropped.width(), cropped.height()), (1, 1));
        assert_eq!(used, Region { x: 640, y: 400, width: 1, height: 1 });
        assert_eq!(*cropped.get_pixel(0, 0), *source.get_pixel(640, 400));
    }

    #[test]
    fn test_crop_at_bottom_right_corner() {
        // The last pixel is an easy off-by-one: width-1 must still be valid.
        let source = coordinate_image(1280, 800);
        let (cropped, used) = crop_to_region(&source, Region { x: 1279, y: 799, width: 1, height: 1 })
            .unwrap();

        assert_eq!((cropped.width(), cropped.height()), (1, 1));
        assert_eq!(used, Region { x: 1279, y: 799, width: 1, height: 1 });
        assert_eq!(*cropped.get_pixel(0, 0), *source.get_pixel(1279, 799));
    }

    #[test]
    fn test_crop_trims_overhang_and_reports_the_trimmed_region() {
        let source = coordinate_image(1280, 800);
        // Asks for 400x300 but only 80x20 is on screen.
        let (cropped, used) = crop_to_region(&source, Region { x: 1200, y: 780, width: 400, height: 300 })
            .unwrap();

        assert_eq!(used, Region { x: 1200, y: 780, width: 80, height: 20 });
        assert_eq!((cropped.width(), cropped.height()), (80, 20));
        // The reported region must describe the pixels actually returned,
        // otherwise the offset sent back to the caller would be a lie.
        assert_crop_matches_source(&source, &cropped, used);
    }

    #[test]
    fn test_crop_does_not_overflow_on_huge_dimensions() {
        let source = coordinate_image(200, 100);
        let (cropped, used) =
            crop_to_region(&source, Region { x: 10, y: 10, width: u32::MAX, height: u32::MAX })
                .unwrap();

        assert_eq!(used, Region { x: 10, y: 10, width: 190, height: 90 });
        assert_eq!((cropped.width(), cropped.height()), (190, 90));
    }

    // --- crop_to_region: negative cases ---

    #[test]
    fn test_crop_rejects_region_past_the_right_edge() {
        let source = coordinate_image(1280, 800);
        let err = crop_to_region(&source, Region { x: 1280, y: 0, width: 10, height: 10 }).unwrap_err();
        assert!(err.contains("1280x800"), "error should name the desktop size: {}", err);
    }

    #[test]
    fn test_crop_rejects_region_past_the_bottom_edge() {
        let source = coordinate_image(1280, 800);
        assert!(crop_to_region(&source, Region { x: 0, y: 800, width: 10, height: 10 }).is_err());
        // Well past, not just touching.
        assert!(crop_to_region(&source, Region { x: 0, y: 5000, width: 10, height: 10 }).is_err());
    }

    #[test]
    fn test_crop_rejects_zero_area() {
        let source = coordinate_image(1280, 800);
        assert!(crop_to_region(&source, Region { x: 10, y: 10, width: 0, height: 30 }).is_err());
        assert!(crop_to_region(&source, Region { x: 10, y: 10, width: 30, height: 0 }).is_err());
    }

    // --- encode_image ---

    #[test]
    fn test_encode_png_round_trips_pixels() {
        let source = coordinate_image(40, 25);
        let bytes = encode_image(&source, ImageFormat::Png).unwrap();

        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (40, 25));
        // PNG is lossless, so this must be exact.
        assert_eq!(decoded.as_raw(), source.as_raw());
    }

    #[test]
    fn test_encode_jpeg_accepts_rgba() {
        // Regression: the image crate refuses to write an RGBA buffer as JPEG,
        // so `screenshot --format jpeg` failed until the RGB conversion was
        // added. Encoding must succeed and produce a real JPEG.
        let source = coordinate_image(40, 25);
        let bytes = encode_image(&source, ImageFormat::Jpeg)
            .expect("JPEG encoding of an RGBA framebuffer must succeed");

        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "should start with the JPEG SOI marker");

        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 25));
    }

    #[test]
    fn test_encode_formats_are_distinguishable() {
        let source = coordinate_image(16, 16);
        let png = encode_image(&source, ImageFormat::Png).unwrap();
        let jpeg = encode_image(&source, ImageFormat::Jpeg).unwrap();

        assert_eq!(&png[1..4], b"PNG");
        assert_ne!(png, jpeg);
    }

    #[test]
    fn test_encode_single_pixel_image() {
        // A 1x1 crop is a legitimate request and must encode in both formats.
        let source = coordinate_image(1, 1);
        assert!(encode_image(&source, ImageFormat::Png).is_ok());
        assert!(encode_image(&source, ImageFormat::Jpeg).is_ok());
    }

    // --- the two together, as the handlers use them ---

    #[test]
    fn test_crop_then_encode_preserves_the_cropped_pixels() {
        let source = coordinate_image(1280, 800);
        let (cropped, used) =
            crop_to_region(&source, Region { x: 100, y: 380, width: 600, height: 30 }).unwrap();

        let bytes = encode_image(&cropped, ImageFormat::Png).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();

        assert_eq!((decoded.width(), decoded.height()), (600, 30));
        assert_crop_matches_source(&source, &decoded, used);
    }

    #[test]
    fn test_hash_pixels_is_deterministic() {
        let data = vec![1u8, 2, 3, 4, 5];
        assert_eq!(hash_pixels(&data), hash_pixels(&data));
    }

    #[test]
    fn test_hash_pixels_known_vector() {
        // FNV-1a 64-bit of an empty input is the fixed offset basis.
        assert_eq!(hash_pixels(&[]), "cbf29ce484222325");
    }

    #[test]
    fn test_hash_pixels_differs_on_one_byte_change() {
        let a = vec![10u8, 20, 30, 40];
        let mut b = a.clone();
        b[2] = 31;
        assert_ne!(hash_pixels(&a), hash_pixels(&b));
    }

    #[test]
    fn test_hash_pixels_is_16_hex_digits() {
        let hash = hash_pixels(&[0u8; 100]);
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

