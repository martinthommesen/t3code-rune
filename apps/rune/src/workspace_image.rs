use std::cmp::Reverse;
use std::io::{self, Cursor, Write};
use std::sync::Arc;

use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder, Limits, Rgba, RgbaImage};
use rune::ClipboardImage;
use rune::clipboard::MAX_IMAGE_BYTES;

mod gif_playback;
mod tiff;

pub(crate) enum Source {
    Png(Arc<[u8]>),
    Decoded(Raster),
    Animated(Animation),
}

pub(crate) struct Raster {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Arc<[u8]>,
    pub(crate) clipboard: RasterCopy,
}

#[derive(Clone)]
pub(crate) enum RasterCopy {
    Pixels(ClipboardImage),
    Unavailable,
    DecodeError,
}

impl From<Option<ClipboardImage>> for RasterCopy {
    fn from(image: Option<ClipboardImage>) -> Self {
        image.map_or(Self::Unavailable, Self::Pixels)
    }
}

pub(crate) struct Animation {
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) size: (u32, u32),
    pub(crate) clipboard: Option<ClipboardImage>,
    pub(crate) restart_on_expand: bool,
}

pub(crate) fn mime_type(path: &str) -> Option<&'static str> {
    let (_, extension) = path.rsplit_once('.')?;
    if extension.eq_ignore_ascii_case("png") {
        Some("image/png")
    } else if extension.eq_ignore_ascii_case("jpeg") || extension.eq_ignore_ascii_case("jpg") {
        Some("image/jpeg")
    } else if extension.eq_ignore_ascii_case("bmp") {
        Some("image/bmp")
    } else if extension.eq_ignore_ascii_case("ico") {
        Some("image/x-icon")
    } else if extension.eq_ignore_ascii_case("tif") || extension.eq_ignore_ascii_case("tiff") {
        Some("image/tiff")
    } else if extension.eq_ignore_ascii_case("gif") {
        Some("image/gif")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("image/webp")
    } else {
        None
    }
}

impl Source {
    pub(crate) async fn read(bytes: Vec<u8>) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png(bytes.into()));
        }
        if bytes.starts_with(b"\xff\xd8\xff") {
            return tokio::task::spawn_blocking(move || decode_jpeg(&bytes))
                .await
                .ok()?
                .map(Self::Decoded);
        }
        if bytes.starts_with(b"BM") {
            return tokio::task::spawn_blocking(move || {
                let decoder = image::codecs::bmp::BmpDecoder::new(Cursor::new(bytes)).ok()?;
                decode_still(decoder)
            })
            .await
            .ok()?
            .map(Self::Decoded);
        }
        if bytes.starts_with(b"\x00\x00\x01\x00") {
            return tokio::task::spawn_blocking(move || decode_ico(bytes))
                .await
                .ok()?
                .map(Self::Decoded);
        }
        if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
            return tokio::task::spawn_blocking(move || tiff::decode(&bytes))
                .await
                .ok()?
                .map(Self::Decoded);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return tokio::task::spawn_blocking(move || gif_playback::decode(bytes))
                .await
                .ok()?;
        }
        if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            return tokio::task::spawn_blocking(move || decode_webp(bytes))
                .await
                .ok()?;
        }
        None
    }
}

fn limit_image(decoder: &mut impl ImageDecoder) -> Option<()> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.reserve(decoder.total_bytes()).ok()?;
    decoder.set_limits(limits).ok()
}

fn decode_jpeg(bytes: &[u8]) -> Option<Raster> {
    let decoder = image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes)).ok()?;
    decode_still(decoder)
}

fn decode_ico(mut bytes: Vec<u8>) -> Option<Raster> {
    const HEADER_SIZE: usize = 6;
    const ENTRY_SIZE: usize = 16;

    let count = usize::from(u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?));
    let directory = bytes.get(HEADER_SIZE..HEADER_SIZE + count * ENTRY_SIZE)?;
    let (index, _) = directory
        .as_chunks::<ENTRY_SIZE>()
        .0
        .iter()
        .enumerate()
        .max_by_key(|(index, entry)| {
            let dimension = |encoded| {
                if encoded == 0 {
                    256
                } else {
                    u32::from(encoded)
                }
            };
            let area = dimension(entry[0]) * dimension(entry[1]);
            let depth = u16::from_le_bytes([entry[6], entry[7]]);
            (area, depth, Reverse(*index))
        })?;

    // The preview uses the largest entry. The decoder otherwise picks by depth first.
    // Keep its original data offset when presenting it as a single-entry directory.
    let start = HEADER_SIZE + index * ENTRY_SIZE;
    bytes.copy_within(start..start + ENTRY_SIZE, HEADER_SIZE);
    bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
    let decoder = image::codecs::ico::IcoDecoder::new(Cursor::new(bytes)).ok()?;
    decode_still(decoder)
}

fn decode_webp(bytes: Vec<u8>) -> Option<Source> {
    let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(&bytes)).ok()?;
    if !decoder.has_animation() {
        return decode_still(decoder).map(Source::Decoded);
    }
    limit_image(&mut decoder)?;
    decoder.set_background_color(Rgba([0, 0, 0, 0])).ok()?;
    let frame = decoder.into_frames().next()?.ok()?.into_buffer();
    Some(animation(bytes, frame, false))
}

fn animation(bytes: Vec<u8>, frame: RgbaImage, restart_on_expand: bool) -> Source {
    Source::Animated(Animation {
        bytes: bytes.into(),
        size: frame.dimensions(),
        clipboard: clipboard_png(&frame),
        restart_on_expand,
    })
}

fn decode_still(mut decoder: impl ImageDecoder) -> Option<Raster> {
    limit_image(&mut decoder)?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(raster(image.into_rgba8()))
}

fn raster(rgba: RgbaImage) -> Raster {
    let (width, height) = rgba.dimensions();
    Raster {
        width,
        height,
        clipboard: clipboard_png(&rgba).into(),
        pixels: rgba.into_raw().into(),
    }
}

fn clipboard_png(rgba: &RgbaImage) -> Option<ClipboardImage> {
    let mut png = BoundedPng::default();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            rgba,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    ClipboardImage::from_png(png.0)
}

#[derive(Default)]
struct BoundedPng(Vec<u8>);

impl Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_IMAGE_BYTES {
            return Err(io::Error::other("The encoded image is too large."));
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
