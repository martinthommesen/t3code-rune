use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(super) const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

pub(super) enum ReadError {
    Io(io::Error),
    TooLarge,
}

pub(super) async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    buffer: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<bool, ReadError> {
    loop {
        let available = reader.fill_buf().await.map_err(ReadError::Io)?;
        let eof = available.is_empty();
        let newline = available.iter().position(|&byte| byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if buffer.len() + count > max_bytes {
            return Err(ReadError::TooLarge);
        }
        buffer.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if !eof && newline.is_none() {
            continue;
        }
        if buffer.iter().all(u8::is_ascii_whitespace) {
            buffer.clear();
            if eof {
                return Ok(false);
            }
            continue;
        }
        return Ok(true);
    }
}
