//! Pictures for the lobby chat: shrinking a pasted or dropped picture to a
//! file small enough to send, and turning received ones into textures.

use std::io::Cursor;

use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};
use gf_lobby::MAX_IMAGE;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};

/// Longest side of a picture that is sent; bigger ones are shrunk to it.
const MAX_SIDE: u32 = 1600;
/// Longest side of a picture as shown in the chat.
pub const THUMB_SIDE: u32 = 480;
/// Longest side accepted from the lobby: sent pictures are at most
/// [`MAX_SIDE`], and the server only checks the file's type and size, so a
/// bad client could otherwise make every window decode a huge picture.
pub const RECEIVED_SIDE: u32 = 4096;
/// Longest side of a picture file the user drops onto the window.
pub const DROPPED_SIDE: u32 = 16384;

/// Decodes a picture file (PNG or JPEG) of at most `max_side` pixels a side.
pub fn decode(bytes: &[u8], max_side: u32) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    // RGBA, and the same again while converting.
    limits.max_alloc = Some((u64::from(max_side).pow(2) * 8).min(1 << 30));
    reader.limits(limits);
    reader.decode().map_err(|e| e.to_string())
}

/// The picture as a file of at most [`MAX_IMAGE`] bytes: shrunk to
/// [`MAX_SIDE`], then a PNG if that is small enough (text stays crisp),
/// else a JPEG, halved until it fits. `None` only if even a tiny JPEG is
/// too big, which doesn't happen.
pub fn encode(img: &DynamicImage) -> Option<Vec<u8>> {
    let img = shrink(img, MAX_SIDE);
    let mut png = Cursor::new(Vec::new());
    if img.write_to(&mut png, ImageFormat::Png).is_ok() && png.get_ref().len() <= MAX_IMAGE {
        return Some(png.into_inner());
    }
    let mut side = MAX_SIDE;
    while side >= 64 {
        // JPEG has no transparency: see-through parts turn white.
        let small = shrink(&img, side).to_rgba8();
        let white = image::Rgba([255, 255, 255, 255]);
        let mut flat = image::RgbaImage::from_pixel(small.width(), small.height(), white);
        image::imageops::overlay(&mut flat, &small, 0, 0);
        let rgb = DynamicImage::ImageRgba8(flat).to_rgb8();
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&rgb)
            .ok()?;
        if out.len() <= MAX_IMAGE {
            return Some(out);
        }
        side /= 2;
    }
    None
}

/// The picture with its longest side at most `side`.
pub fn shrink(img: &DynamicImage, side: u32) -> DynamicImage {
    if img.width().max(img.height()) <= side {
        img.clone()
    } else {
        img.thumbnail(side, side)
    }
}

/// The picture as a texture, for `egui::Image`.
pub fn texture(ctx: &Context, name: &str, img: &DynamicImage) -> TextureHandle {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    ctx.load_texture(name, color, TextureOptions::LINEAR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gf_lobby::Image;

    /// Big and noisy: far too big as a PNG, so it becomes a JPEG.
    fn noisy(alpha: u8) -> DynamicImage {
        let mut seed = 1u32;
        DynamicImage::ImageRgba8(image::RgbaImage::from_fn(3000, 2000, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = seed.to_le_bytes();
            image::Rgba([b[1], b[2], b[3], alpha])
        }))
    }

    #[test]
    fn pictures_are_shrunk_to_fit() {
        // A JPEG, shrunk until it fits, keeping its shape.
        let file = encode(&noisy(255)).unwrap();
        assert!(file.starts_with(&[0xFF, 0xD8, 0xFF]), "not a JPEG");
        assert!(Image(file.clone()).ok(), "{} bytes", file.len());
        let back = decode(&file, RECEIVED_SIDE).unwrap();
        assert!(back.width() <= MAX_SIDE && back.height() < back.width());
        assert!((back.width() as f32 / back.height() as f32 - 1.5).abs() < 0.01);
        // See-through parts of a JPEG turn white, not black.
        let file = encode(&noisy(0)).unwrap();
        let back = decode(&file, RECEIVED_SIDE).unwrap().to_rgb8();
        assert!(
            back.get_pixel(10, 10).0.iter().all(|&v| v > 240),
            "{:?}",
            back.get_pixel(10, 10)
        );
        // Small and flat: stays a PNG at its own size.
        let flat = image::RgbaImage::from_pixel(300, 200, image::Rgba([10, 20, 30, 255]));
        let file = encode(&DynamicImage::ImageRgba8(flat)).unwrap();
        assert!(file.starts_with(b"\x89PNG"));
        assert_eq!(decode(&file, RECEIVED_SIDE).unwrap().width(), 300);
        assert!(decode(b"not a picture", RECEIVED_SIDE).is_err());
    }

    #[test]
    fn received_pictures_have_a_size_limit() {
        // A thin PNG just over the lobby limit: fine from a dropped file,
        // refused from the lobby.
        let wide = image::RgbaImage::from_pixel(RECEIVED_SIDE + 1, 20, image::Rgba([0, 0, 0, 255]));
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(wide)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        assert!(decode(png.get_ref(), DROPPED_SIDE).is_ok());
        assert!(decode(png.get_ref(), RECEIVED_SIDE).is_err());
    }
}
