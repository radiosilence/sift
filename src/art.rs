//! Cover art post-processing: size and ratio rules, and resizing to fit.
//!
//! The rules are a pure function over bytes, so they can be tested without
//! a network fetch.

use image::imageops::FilterType;

use crate::config::Ratio;

/// The rules a candidate cover is checked against, taken from [`Config`](crate::config::Config).
#[derive(Debug, Clone, Copy)]
pub struct ArtRules {
    pub min_width: u32,
    pub max_width: u32,
    pub quality: u8,
    pub ratio: Option<Ratio>,
}

/// A candidate that passed `min_width` and `ratio`, ready to embed.
pub struct Prepared {
    pub bytes: Vec<u8>,
    /// Whether the original was resized and re-encoded to fit `max_width`.
    pub resized: bool,
}

/// Checks `bytes` against `rules`. Returns `None` when the image's size
/// cannot be read, it is narrower than `min_width`, or it fails `ratio` —
/// the caller should try its next candidate. An image wider than
/// `max_width` is resized down and re-encoded as JPEG at `quality`; one
/// already within bounds is returned unchanged. Only the header is read
/// unless the image is resized: decoding a large cover just to learn its
/// width costs tens of megabytes.
pub fn prepare(bytes: &[u8], rules: &ArtRules) -> Option<Prepared> {
    let reader = || {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()
    };
    let (width, height) = reader()?.into_dimensions().ok()?;
    if width < rules.min_width {
        return None;
    }
    if let Some(ratio) = rules.ratio
        && !ratio.allows(width, height)
    {
        return None;
    }
    if rules.max_width == 0 || width <= rules.max_width {
        return Some(Prepared {
            bytes: bytes.to_vec(),
            resized: false,
        });
    }
    let mut decoder = reader()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    decoder.limits(limits);
    let img = decoder.decode().ok()?;
    let new_height =
        ((f64::from(height) * f64::from(rules.max_width) / f64::from(width)).round() as u32).max(1);
    // JPEG has no alpha channel: a transparent PNG is flattened to RGB, or
    // the encoder refuses it and the cover would be dropped, not resized.
    let resized = image::DynamicImage::ImageRgb8(
        img.resize_exact(rules.max_width, new_height, FilterType::Lanczos3)
            .to_rgb8(),
    );
    let mut out = Vec::new();
    let encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, rules.quality.clamp(1, 100));
    resized.write_with_encoder(encoder).ok()?;
    Some(Prepared {
        bytes: out,
        resized: true,
    })
}

/// The most a cover may take decoded: about a 5600×5600 RGBA image, well
/// past any real cover. Resizing it takes roughly as much again.
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;

/// The most a fetched cover may weigh: far past a real one, short of what a
/// misbehaving server could send.
pub(crate) const MAX_FETCH_BYTES: usize = 64 * 1024 * 1024;

/// `resp`'s body, or `None` if it is larger than `max` bytes or fails to
/// arrive. Read a chunk at a time, so a body too large is dropped before it
/// is all in memory.
pub(crate) async fn read_capped(mut resp: reqwest::Response, max: usize) -> Option<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return None;
    }
    let mut out = Vec::with_capacity(resp.content_length().map_or(0, |n| n as usize));
    while let Some(chunk) = resp.chunk().await.ok()? {
        if out.len() + chunk.len() > max {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(encoder)
            .unwrap();
        out
    }

    fn rules(min_width: u32, max_width: u32, ratio: Option<Ratio>) -> ArtRules {
        ArtRules {
            min_width,
            max_width,
            quality: 90,
            ratio,
        }
    }

    #[test]
    fn percent_ratio_accepts_within_tolerance_and_rejects_outside() {
        let r = rules(0, 0, Some(Ratio::Percent(10.0)));
        assert!(prepare(&jpeg(1000, 950), &r).is_some());
        assert!(prepare(&jpeg(1000, 800), &r).is_none());
    }

    #[test]
    fn pixel_ratio_rejects_outside_tolerance() {
        let r = rules(0, 0, Some(Ratio::Pixels(10)));
        assert!(prepare(&jpeg(1000, 980), &r).is_none());
    }

    #[test]
    fn minwidth_rejects_a_smaller_image() {
        let r = rules(500, 0, None);
        assert!(prepare(&jpeg(400, 400), &r).is_none());
    }

    #[test]
    fn oversized_jpeg_is_resized_and_reencoded() {
        let r = rules(0, 1200, None);
        let prepared = prepare(&jpeg(3000, 3000), &r).unwrap();
        assert!(prepared.resized);
        let img = image::load_from_memory(&prepared.bytes).unwrap();
        assert_eq!(img.width(), 1200);
    }

    #[test]
    fn jpeg_within_max_width_is_unchanged() {
        let bytes = jpeg(1000, 1000);
        let r = rules(0, 1200, None);
        let prepared = prepare(&bytes, &r).unwrap();
        assert!(!prepared.resized);
        assert_eq!(prepared.bytes, bytes);
    }

    #[test]
    fn an_oversized_transparent_png_is_resized_not_dropped() {
        let img = image::RgbaImage::from_pixel(2000, 2000, image::Rgba([10, 20, 30, 128]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let out = prepare(&png, &rules(0, 1200, None)).expect("resized");
        assert!(out.resized);
        assert_eq!(
            image::guess_format(&out.bytes).unwrap(),
            image::ImageFormat::Jpeg
        );
    }
}
