use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::AppearanceMode;
use super::appearance::MODE_KEY;
use super::project_order::UI_STATE_KEY;

mod record;

use record::{Reader, decompress_snappy, invalid, matches_checksum, visit_log};

const ORIGINS: [&[u8]; 2] = [b"_t3code://app\0", b"_t3code-dev://app\0"];
const KEYS: [&str; 4] = [
    MODE_KEY,
    "t3code:theme-follow-system",
    "t3code:theme",
    UI_STATE_KEY,
];
const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
const TABLE_FOOTER_SIZE: usize = 48;
const BLOCK_TRAILER_SIZE: usize = 5;

/// Read Chromium's files directly so importing never opens or mutates its LevelDB database.
pub(super) fn read_appearance(electron_profile: &Path) -> io::Result<Option<AppearanceMode>> {
    Ok(read_storage(electron_profile)?.mode())
}

pub(super) fn read_project_order(electron_profile: &Path) -> io::Result<Option<String>> {
    Ok(read_storage(electron_profile)?
        .0
        .into_iter()
        .find_map(|origin| {
            let [_, _, _, value] = origin;
            value.and_then(|entry| entry.value)
        }))
}

fn read_storage(electron_profile: &Path) -> io::Result<StoredValues> {
    let profile = electron_profile.canonicalize()?;
    let directory = confined_path(&profile, &profile.join("Local Storage").join("leveldb"))?;
    let current = current_manifest(&directory)?;
    let manifest = read_file(&directory, &current)?;
    let live = read_manifest(&manifest)?;
    let mut values = StoredValues::default();
    for number in live.tables {
        let bytes = match read_file(&directory, &format!("{number:06}.ldb")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                read_file(&directory, &format!("{number:06}.sst"))?
            }
            result => result?,
        };
        read_table(&bytes, &mut values)?;
    }
    for item in fs::read_dir(&directory)? {
        let name = item?.file_name();
        let Some((name, number)) = name.to_str().and_then(|name| {
            let digits = name.strip_suffix(".log")?;
            if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let number = digits.parse::<u64>().ok()?;
            Some((name, number))
        }) else {
            continue;
        };
        if number >= live.log_number || number == live.previous_log_number {
            let bytes = read_file(&directory, name)?;
            visit_log(&bytes, |batch| read_batch(batch, &mut values))?;
        }
    }
    // A compaction can move a value from a log into a table after the initial manifest read.
    let manifest_path = confined_path(&directory, &directory.join(&current))?;
    if current_manifest(&directory)? != current
        || fs::metadata(manifest_path)?.len() != manifest.len() as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Chromium Local Storage changed while reading",
        ));
    }
    Ok(values)
}

fn current_manifest(directory: &Path) -> io::Result<String> {
    let data = read_file(directory, "CURRENT")?;
    let name = std::str::from_utf8(&data)
        .map_err(|_| invalid("Invalid LevelDB CURRENT file"))?
        .trim();
    if name.strip_prefix("MANIFEST-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(name.to_owned())
    } else {
        Err(invalid("Invalid LevelDB manifest name"))
    }
}

fn confined_path(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if canonical.starts_with(root) {
        Ok(canonical)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Chromium Local Storage path leaves the selected profile",
        ))
    }
}

fn read_file(directory: &Path, name: &str) -> io::Result<Vec<u8>> {
    let path = confined_path(directory, &directory.join(name))?;
    if !fs::metadata(&path)?.is_file() {
        return Err(invalid("Chromium Local Storage entry is not a file"));
    }
    let file = File::open(path)?;
    let mut data = Vec::new();
    file.take(MAX_FILE_SIZE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_FILE_SIZE {
        return Err(invalid("Chromium Local Storage file is too large"));
    }
    Ok(data)
}

#[derive(Default)]
struct LiveFiles {
    tables: BTreeSet<u64>,
    log_number: u64,
    previous_log_number: u64,
}

fn read_manifest(bytes: &[u8]) -> io::Result<LiveFiles> {
    let mut live = LiveFiles::default();
    let mut required = 0b111_u8;
    visit_log(bytes, |record| {
        let mut reader = Reader::new(record);
        while !reader.done() {
            match reader.varint()? {
                1 => {
                    reader.length_prefixed()?;
                }
                2 => {
                    live.log_number = reader.varint()?;
                    required &= !0b001;
                }
                tag @ (3 | 4) => {
                    reader.varint()?;
                    required &= !(1 << (tag - 2));
                }
                5 => {
                    reader.varint()?;
                    reader.length_prefixed()?;
                }
                6 => {
                    reader.varint()?;
                    live.tables.remove(&reader.varint()?);
                }
                7 => {
                    reader.varint()?;
                    live.tables.insert(reader.varint()?);
                    reader.varint()?;
                    reader.length_prefixed()?;
                    reader.length_prefixed()?;
                }
                9 => live.previous_log_number = reader.varint()?,
                _ => return Err(invalid("Unknown LevelDB manifest tag")),
            }
        }
        Ok(())
    })?;
    if required != 0 {
        return Err(invalid("Incomplete LevelDB manifest"));
    }
    Ok(live)
}

fn read_batch(batch: &[u8], values: &mut StoredValues) -> io::Result<()> {
    let mut reader = Reader::new(batch);
    let base_sequence = reader.u64()?;
    for index in 0..reader.u32()? {
        let kind = reader.byte()?;
        let key = reader.length_prefixed()?;
        let value = match kind {
            0 => None,
            1 => Some(reader.length_prefixed()?),
            _ => return Err(invalid("Unknown LevelDB write batch entry")),
        };
        let sequence = base_sequence
            .checked_add(u64::from(index))
            .ok_or_else(|| invalid("LevelDB sequence overflow"))?;
        values.record(key, value, sequence);
    }
    Ok(())
}

fn read_table(table: &[u8], values: &mut StoredValues) -> io::Result<()> {
    let footer_offset = table
        .len()
        .checked_sub(TABLE_FOOTER_SIZE)
        .ok_or_else(|| invalid("Incomplete LevelDB table footer"))?;
    let mut footer = Reader::new(&table[footer_offset..]);
    footer.varint()?;
    footer.varint()?;
    let index = read_block(table, &mut footer)?;
    visit_block(&index, |_, handle| {
        let block = read_block(table, &mut Reader::new(handle))?;
        visit_block(&block, |key, value| {
            let split = key
                .len()
                .checked_sub(8)
                .ok_or_else(|| invalid("Incomplete LevelDB internal key"))?;
            let (key, trailer) = key.split_at(split);
            let trailer = Reader::new(trailer).u64()?;
            values.record(key, (trailer & 0xff == 1).then_some(value), trailer >> 8);
            Ok(())
        })
    })
}

fn read_block<'a>(table: &'a [u8], handle: &mut Reader<'_>) -> io::Result<Cow<'a, [u8]>> {
    let offset = handle.size()?;
    let size = handle.size()?;
    let end = offset
        .checked_add(size)
        .and_then(|end| end.checked_add(BLOCK_TRAILER_SIZE))
        .ok_or_else(|| invalid("LevelDB block handle overflow"))?;
    let block = table
        .get(offset..end)
        .ok_or_else(|| invalid("LevelDB block handle is out of range"))?;
    let checksum = Reader::new(&block[size + 1..]).u32()?;
    if !matches_checksum(&block[..size + 1], checksum) {
        return Err(invalid("Invalid LevelDB block checksum"));
    }
    match block[size] {
        0 => Ok(Cow::Borrowed(&block[..size])),
        1 => Ok(Cow::Owned(decompress_snappy(&block[..size])?)),
        _ => Err(invalid("Unsupported LevelDB block compression")),
    }
}

fn visit_block(
    block: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let footer = block
        .len()
        .checked_sub(4)
        .ok_or_else(|| invalid("Incomplete LevelDB block"))?;
    let restart_count = Reader::new(&block[footer..]).u32()? as usize;
    let limit = restart_count
        .checked_mul(4)
        .and_then(|length| footer.checked_sub(length))
        .ok_or_else(|| invalid("LevelDB block restart array is out of range"))?;
    let mut reader = Reader::new(&block[..limit]);
    let mut key = Vec::new();
    while !reader.done() {
        let shared = reader.size()?;
        let unshared = reader.size()?;
        let value_length = reader.size()?;
        if shared > key.len() {
            return Err(invalid("LevelDB block key prefix is out of range"));
        }
        key.truncate(shared);
        key.extend_from_slice(reader.take(unshared)?);
        visit(&key, reader.take(value_length)?)?;
    }
    Ok(())
}

#[derive(Default)]
struct StoredValues([[Option<Entry>; KEYS.len()]; ORIGINS.len()]);

struct Entry {
    sequence: u64,
    value: Option<String>,
}

impl StoredValues {
    fn record(&mut self, raw_key: &[u8], raw_value: Option<&[u8]>, sequence: u64) {
        for (index, origin) in ORIGINS.into_iter().enumerate() {
            if let Some(key) = raw_key.strip_prefix(origin)
                && let Some(key_index) = KEYS
                    .iter()
                    .position(|expected| matches_chromium_key(key, expected))
            {
                let entry = &mut self.0[index][key_index];
                if entry
                    .as_ref()
                    .is_none_or(|current| current.sequence < sequence)
                {
                    *entry = Some(Entry {
                        sequence,
                        value: raw_value.and_then(|value| chromium_string(value, KEYS[key_index])),
                    });
                }
                return;
            }
        }
    }

    fn mode(&self) -> Option<AppearanceMode> {
        self.0.iter().find_map(|origin| {
            let [mode, follow, theme, _] = origin
                .each_ref()
                .map(|entry| entry.as_ref().and_then(|entry| entry.value.as_deref()));
            if let Some(mode) = mode.and_then(AppearanceMode::parse) {
                return Some(mode);
            }
            if mode.is_none() && follow.is_none() && theme.is_none() {
                return None;
            }
            let theme_mode = match theme {
                Some("dark" | "t3-chat-dark") => AppearanceMode::Dark,
                Some(
                    "light" | "t3-chat" | "grove" | "ocean" | "ember" | "iris" | "t3-grove"
                    | "t3-ocean" | "t3-ember" | "t3-iris",
                ) => AppearanceMode::Light,
                _ => AppearanceMode::System,
            };
            Some(match (follow, theme_mode) {
                (Some("true"), _) => AppearanceMode::System,
                (Some("false"), AppearanceMode::System) => AppearanceMode::Light,
                (_, mode) => mode,
            })
        })
    }
}

fn matches_chromium_key(actual: &[u8], expected: &str) -> bool {
    match actual.split_first() {
        Some((1, bytes)) => bytes == expected.as_bytes(),
        Some((0, bytes)) if bytes.len() == expected.len() * 2 => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .zip(expected.bytes())
            .all(|(unit, byte)| *unit == [byte, 0]),
        _ => false,
    }
}

fn chromium_string(bytes: &[u8], key: &str) -> Option<String> {
    if key != UI_STATE_KEY && bytes.len() > 4096 {
        return None;
    }
    match bytes.split_first()? {
        (1, body) => Some(body.iter().copied().map(char::from).collect()),
        (0, body) if body.len() % 2 == 0 => Some(String::from_utf16_lossy(
            &body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_le_bytes(*chunk))
                .collect::<Vec<_>>(),
        )),
        _ => None,
    }
}
