use std::io::{self, Cursor, Write};
use std::sync::Arc;

use image::{DynamicImage, ImageDecoder, ImageEncoder, Limits};
use rune::ClipboardImage;
use rune::clipboard::MAX_IMAGE_BYTES;

pub(crate) enum Source {
    Png(Arc<[u8]>),
    Decoded(Raster),
}

pub(crate) struct Raster {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Arc<[u8]>,
    pub(crate) clipboard: Option<ClipboardImage>,
}

pub(crate) fn mime_type(path: &str) -> Option<&'static str> {
    let (_, extension) = path.rsplit_once('.')?;
    if extension.eq_ignore_ascii_case("png") {
        Some("image/png")
    } else if extension.eq_ignore_ascii_case("jpeg") || extension.eq_ignore_ascii_case("jpg") {
        Some("image/jpeg")
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
        None
    }
}

fn decode_jpeg(bytes: &[u8]) -> Option<Raster> {
    let mut decoder = image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes)).ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.reserve(decoder.total_bytes()).ok()?;
    decoder.set_limits(limits).ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut png = ClipboardPng::default();
    let clipboard = image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
        .ok()
        .and_then(|()| ClipboardImage::from_png(png.0));
    Some(Raster {
        width,
        height,
        pixels: rgba.into_raw().into(),
        clipboard,
    })
}

#[derive(Default)]
struct ClipboardPng(Vec<u8>);

impl Write for ClipboardPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_IMAGE_BYTES {
            return Err(io::Error::other(
                "The image is too large for the clipboard.",
            ));
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
