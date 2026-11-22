use std::io;

const LOG_BLOCK_SIZE: usize = 32 * 1024;
const LOG_HEADER_SIZE: usize = 7;
const MAX_DECOMPRESSED_BLOCK: usize = 16 * 1024 * 1024;

pub(super) fn invalid(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub(super) fn done(&self) -> bool {
        self.position == self.bytes.len()
    }

    pub(super) fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| invalid("LevelDB length overflow"))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("Truncated LevelDB record"))?;
        self.position = end;
        Ok(bytes)
    }

    pub(super) fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(super) fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    pub(super) fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    pub(super) fn varint(&mut self) -> io::Result<u64> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            if shift == 63 && byte > 1 {
                break;
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("Invalid LevelDB varint"))
    }

    pub(super) fn size(&mut self) -> io::Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| invalid("LevelDB record is too large"))
    }

    pub(super) fn length_prefixed(&mut self) -> io::Result<&'a [u8]> {
        let length = self.size()?;
        self.take(length)
    }
}

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0x82f63b78
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

pub(super) fn matches_checksum(bytes: &[u8], masked: u32) -> bool {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc = CRC32C_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc == masked.wrapping_sub(0xa282ead8).rotate_right(17)
}

pub(super) fn visit_log(
    bytes: &[u8],
    mut visit: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut offset = 0;
    let mut pending: Option<Vec<u8>> = None;
    while offset + LOG_HEADER_SIZE <= bytes.len() {
        let remaining = LOG_BLOCK_SIZE - offset % LOG_BLOCK_SIZE;
        if remaining < LOG_HEADER_SIZE {
            offset += remaining;
            continue;
        }
        let length = usize::from(u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]));
        let record_size = LOG_HEADER_SIZE + length;
        let fits = record_size <= remaining;
        let end = offset + record_size;
        if fits && end > bytes.len() {
            break;
        }
        let kind = bytes[offset + 6];
        let checksum =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("log header"));
        if kind == 0 || !fits || !matches_checksum(&bytes[offset + 6..end], checksum) {
            offset += remaining;
            pending = None;
            continue;
        }
        let payload = &bytes[offset + LOG_HEADER_SIZE..end];
        offset = end;
        match kind {
            1 => {
                pending = None;
                visit(payload)?;
            }
            2 => pending = Some(payload.to_vec()),
            3 => {
                if let Some(record) = &mut pending {
                    record.extend_from_slice(payload);
                }
            }
            4 => {
                if let Some(mut record) = pending.take() {
                    record.extend_from_slice(payload);
                    visit(&record)?;
                }
            }
            _ => pending = None,
        }
    }
    Ok(())
}

pub(super) fn decompress_snappy(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(bytes);
    let size = reader.size()?;
    if size > MAX_DECOMPRESSED_BLOCK {
        return Err(invalid("Chromium Local Storage block is too large"));
    }
    let mut output = Vec::with_capacity(size);
    while !reader.done() {
        let tag = reader.byte()?;
        let kind = tag & 3;
        if kind == 0 {
            let literal = usize::from(tag >> 2);
            let length = if literal < 60 {
                literal + 1
            } else {
                let mut length = [0; 4];
                let bytes = reader.take(literal - 59)?;
                length[..bytes.len()].copy_from_slice(bytes);
                usize::try_from(u64::from(u32::from_le_bytes(length)) + 1)
                    .map_err(|_| invalid("Snappy literal is too large"))?
            };
            if length > size - output.len() {
                return Err(invalid("Snappy literal overflows block"));
            }
            output.extend_from_slice(reader.take(length)?);
        } else {
            let (length, distance) = match kind {
                1 => (
                    4 + usize::from((tag >> 2) & 7),
                    (usize::from(tag >> 5) << 8) | usize::from(reader.byte()?),
                ),
                2 => (
                    usize::from(tag >> 2) + 1,
                    usize::from(u16::from_le_bytes(
                        reader.take(2)?.try_into().expect("two bytes"),
                    )),
                ),
                _ => (
                    usize::from(tag >> 2) + 1,
                    usize::try_from(reader.u32()?)
                        .map_err(|_| invalid("Snappy distance too large"))?,
                ),
            };
            if distance == 0 || distance > output.len() || length > size - output.len() {
                return Err(invalid("Snappy copy is out of range"));
            }
            for _ in 0..length {
                output.push(output[output.len() - distance]);
            }
        }
    }
    if output.len() != size {
        return Err(invalid("Incomplete Snappy block"));
    }
    Ok(output)
}
