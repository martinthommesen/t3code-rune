use std::io::Cursor;

use image::{AnimationDecoder, Frame, ImageDecoder, ImageResult};

use super::{BoundedPng, Source, animation, limit_image, raster};

const MOTION_BYTES: u64 = 256 * 1024 * 1024;

enum Presentation {
    Original,
    Still,
    Once(u32),
}

pub(super) fn decode(bytes: Vec<u8>) -> Option<Source> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes)).ok()?;
    limit_image(&mut decoder)?;
    let size = decoder.dimensions();
    let mut frames = decoder.into_frames();
    let first = frames.next()?.ok()?;
    match presentation(&bytes, size).unwrap_or(Presentation::Original) {
        Presentation::Original => {
            drop(frames);
            Some(animation(bytes, first.into_buffer(), false))
        }
        Presentation::Still => Some(Source::Decoded(raster(first.into_buffer()))),
        Presentation::Once(count) => {
            let encoded = encode_once(count, &first, &mut frames);
            let first = first.into_buffer();
            Some(match encoded {
                Some(encoded) => animation(encoded, first, true),
                None => Source::Decoded(raster(first)),
            })
        }
    }
}

fn presentation(bytes: &[u8], (width, height): (u32, u32)) -> Option<Presentation> {
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let mut scanner = options.read_info(Cursor::new(bytes)).ok()?;
    // The image adapter maps a missing repeat extension and an infinite repeat to the same value.
    if !matches!(scanner.repeat(), gif::Repeat::Finite(0)) {
        return Some(Presentation::Original);
    }

    // Match the renderer's per-image budget, including the mip chain.
    let frame_bytes = u64::from(width) * u64::from(height) * 16 / 3;
    let mut count = 0;
    while scanner.next_frame_info().ok()?.is_some() {
        if !matches!(scanner.repeat(), gif::Repeat::Finite(0)) {
            return Some(Presentation::Original);
        }
        count += 1;
        if count > 1 && u64::from(count) * frame_bytes > MOTION_BYTES {
            return Some(Presentation::Still);
        }
    }
    Some(if count > 1 {
        Presentation::Once(count)
    } else {
        Presentation::Original
    })
}

fn encode_once(
    count: u32,
    first: &Frame,
    frames: &mut impl Iterator<Item = ImageResult<Frame>>,
) -> Option<Vec<u8>> {
    let mut bytes = BoundedPng::default();
    let (width, height) = first.buffer().dimensions();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.set_animated(count, 1).ok()?;
    let mut writer = encoder.write_header().ok()?;
    let mut write_frame = |frame: &Frame| {
        let (ms, denominator) = frame.delay().numer_denom_ms();
        let centiseconds = u16::try_from(ms / denominator.max(1) / 10).ok()?;
        writer.set_frame_delay(centiseconds, 100).ok()?;
        writer.write_image_data(frame.buffer().as_raw()).ok()
    };
    write_frame(first)?;
    for _ in 1..count {
        write_frame(&frames.next()?.ok()?)?;
    }
    writer.finish().ok()?;
    Some(bytes.0)
}
