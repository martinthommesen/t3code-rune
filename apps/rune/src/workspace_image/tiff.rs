use std::io::Cursor;

use ::tiff::ColorType;
use ::tiff::decoder::{ChunkType, Decoder, DecodingResult};
use ::tiff::tags::Tag;

use super::{Raster, RasterCopy};

const MAX_PIXELS: u64 = 32_000_000;
const MAX_CHUNKS: u32 = 2048;
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

pub(super) fn decode(bytes: &[u8]) -> Option<Raster> {
    let mut decoder = Decoder::new(Cursor::new(bytes)).ok()?;
    let (width, height) = decoder.dimensions().ok()?;
    let pixels = u64::from(width) * u64::from(height);
    if !(1..=MAX_PIXELS).contains(&pixels) {
        return None;
    }

    let orientation = decoder
        .find_tag_unsigned::<u16>(Tag::Orientation)
        .ok()?
        .unwrap_or(1);
    let planar = decoder
        .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
        .ok()?
        .unwrap_or(1);
    let compression = decoder
        .find_tag_unsigned::<u16>(Tag::Compression)
        .ok()?
        .unwrap_or(1);
    if orientation > 1 || planar != 1 || !matches!(compression, 1 | 5 | 8 | 32946) {
        return None;
    }

    let (channels, depth) = match decoder.colortype().ok()? {
        ColorType::Gray(bits) => (1, bits),
        ColorType::GrayA(bits) => (2, bits),
        ColorType::RGB(bits) => (3, bits),
        ColorType::RGBA(bits) => (4, bits),
        _ => return None,
    };
    if !matches!(depth, 8 | 16) {
        return None;
    }
    let chunks = match decoder.get_chunk_type() {
        ChunkType::Strip => decoder.strip_count().ok()?,
        ChunkType::Tile if channels == 1 => decoder.tile_count().ok()?,
        _ => return None,
    };
    let (chunk_width, chunk_height) = decoder.chunk_dimensions();
    let chunk_pixels = u64::from(chunk_width) * u64::from(chunk_height);
    if !(1..=MAX_CHUNKS).contains(&chunks) || !(1..=MAX_PIXELS).contains(&chunk_pixels) {
        return None;
    }
    let decoded_bytes = chunk_pixels * u64::from(chunks) * channels as u64 * u64::from(depth / 8);
    if decoded_bytes > MAX_DECODED_BYTES {
        return None;
    }

    let mut data = DecodingResult::U8(Vec::new());
    decoder.read_image_to_buffer(&mut data).ok()?;
    let samples = match data {
        DecodingResult::U8(samples) => samples,
        DecodingResult::U16(samples) => samples
            .into_iter()
            .map(|sample| ((u32::from(sample) + 128) / 257) as u8)
            .collect(),
        _ => return None,
    };
    if samples.len() != pixels as usize * channels {
        return None;
    }

    let rgba = match channels {
        1 => samples
            .into_iter()
            .flat_map(|gray| [gray, gray, gray, 255])
            .collect(),
        2 => samples
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[gray, alpha]| [gray, gray, gray, alpha])
            .collect(),
        3 => samples
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[red, green, blue]| [red, green, blue, 255])
            .collect(),
        _ => samples,
    };
    Some(Raster {
        width,
        height,
        pixels: rgba.into(),
        clipboard: RasterCopy::DecodeError,
    })
}
