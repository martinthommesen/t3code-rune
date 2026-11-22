use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest};
use uuid::Uuid;

use super::history::{
    History, PERSISTED_HISTORY_BYTES, PERSISTED_HISTORY_LINES, Sequence, Utf8Output,
};
use super::{Session, TerminalKey, lock};

const DEFAULT_TERMINAL_ID: &str = "term-1";
const MAX_FILENAME_BYTES: usize = 143;
const PERSISTENCE_DELAY: Duration = Duration::from_millis(250);
const DISCARD_BUFFER_BYTES: usize = 64 * 1024;

pub(super) struct Persistence {
    pub(super) storage: Arc<Storage>,
    worker: Option<JoinHandle<()>>,
}

pub(super) struct Storage {
    directory: PathBuf,
    pending: Mutex<Pending>,
    wake: Condvar,
}

type PendingSession = (Instant, Weak<Session>);

#[derive(Default)]
struct Pending {
    sessions: HashMap<TerminalKey, PendingSession>,
    stopped: bool,
}

impl Persistence {
    pub(super) fn new(state_dir: &Path) -> io::Result<Self> {
        let state_dir = state_dir.canonicalize()?;
        let logs = state_dir.join("logs");
        make_directory(&logs)?;
        let directory = logs.join("terminals");
        make_directory(&directory)?;
        let storage = Arc::new(Storage {
            directory,
            pending: Mutex::default(),
            wake: Condvar::new(),
        });
        let worker_storage = Arc::clone(&storage);
        let worker = thread::Builder::new()
            .name("t3-terminal-history".into())
            .spawn(move || worker_storage.run())?;
        Ok(Self {
            storage,
            worker: Some(worker),
        })
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        lock(&self.storage.pending).stopped = true;
        self.storage.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Storage {
    pub(super) fn queue(&self, session: &Arc<Session>) {
        let mut pending = lock(&self.pending);
        pending
            .sessions
            .entry(session.key.clone())
            .and_modify(|request| request.1 = Arc::downgrade(session))
            .or_insert_with(|| (Instant::now() + PERSISTENCE_DELAY, Arc::downgrade(session)));
        self.wake.notify_one();
    }

    fn run(&self) {
        let mut pending = lock(&self.pending);
        while !pending.stopped {
            let next = pending.sessions.iter().min_by_key(|(_, (time, _))| *time);
            let Some((key, (time, _))) = next else {
                pending = self
                    .wake
                    .wait(pending)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            };
            let remaining = time.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                pending = self
                    .wake
                    .wait_timeout(pending, remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
                continue;
            }
            let key = key.clone();
            let (_, session) = pending
                .sessions
                .remove(&key)
                .expect("scheduled history write exists");
            drop(pending);
            if let Some(session) = session.upgrade() {
                session.flush_history(false);
            }
            pending = lock(&self.pending);
        }
    }

    pub(super) fn read(&self, key: &TerminalKey) -> Result<History, ReadError> {
        let names = Names::new(key);
        for name in names.candidates() {
            let migration = name != names.current;
            let operation = if migration { "migrate" } else { "read" };
            let wrap = |error| ReadError { operation, error };
            let path = self.directory.join(name);
            if !self.exists(name).map_err(wrap)? {
                continue;
            }
            let (raw, truncated) = read_tail(&path, PERSISTED_HISTORY_BYTES).map_err(wrap)?;
            let mut history =
                History::with_limits(PERSISTED_HISTORY_LINES, PERSISTED_HISTORY_BYTES);
            history.append(&raw);
            let capped = history.text();
            if migration || truncated || capped != raw {
                let update = self.replace(&self.directory.join(&names.current), capped.as_bytes());
                if migration {
                    update.map_err(wrap)?;
                } else if let Err(error) = update {
                    eprintln!("Could not compact terminal history: {error}");
                }
            }
            if migration {
                remove_file(&path);
            }
            return Ok(history);
        }
        Ok(History::with_limits(
            PERSISTED_HISTORY_LINES,
            PERSISTED_HISTORY_BYTES,
        ))
    }

    pub(super) fn write(&self, key: &TerminalKey, data: &str, append: bool) -> io::Result<()> {
        let path = self.directory.join(Names::new(key).current);
        if append {
            if data.is_empty() {
                return Ok(());
            }
            regular_file(&path)?;
            OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(data.as_bytes())
        } else {
            self.replace(&path, data.as_bytes())
        }
    }

    fn replace(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let temporary = self.directory.join(format!(".history-{}", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = file.write_all(data);
        drop(file);
        let result = result.and_then(|()| fs::rename(&temporary, path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(super) fn delete(&self, key: &TerminalKey) {
        for name in Names::new(key).candidates() {
            match self.exists(name) {
                Ok(true) => remove_file(&self.directory.join(name)),
                Ok(false) => {}
                Err(error) => eprintln!("Could not inspect terminal history to delete it: {error}"),
            }
        }
    }

    pub(super) fn delete_thread(&self, thread_id: &str) -> io::Result<()> {
        let encoded = encoded_thread(thread_id);
        let hashed = hashed_thread(thread_id);
        let legacy = legacy_name(thread_id);
        let mut first_error = None;
        for entry in fs::read_dir(&self.directory)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if (legacy.as_deref() == Some(name)
                || [encoded.as_str(), hashed.as_str()].into_iter().any(|part| {
                    name.strip_prefix(part).is_some_and(|rest| {
                        rest == ".log" || (rest.starts_with('_') && rest.ends_with(".log"))
                    })
                }))
                && let Err(error) = fs::remove_file(entry.path())
                && error.kind() != io::ErrorKind::NotFound
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn exists(&self, name: &str) -> io::Result<bool> {
        if name.len() > MAX_FILENAME_BYTES {
            for entry in fs::read_dir(&self.directory)? {
                if entry?.file_name() == name {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        match fs::symlink_metadata(self.directory.join(name)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

pub(super) struct ReadError {
    pub(super) operation: &'static str,
    pub(super) error: io::Error,
}

struct Names {
    current: String,
    old_encoded: Option<String>,
    old_flat: Option<String>,
}

impl Names {
    fn new(key: &TerminalKey) -> Self {
        let default = key.terminal_id == DEFAULT_TERMINAL_ID;
        let encoded = filename(
            encoded_thread(&key.thread_id),
            (!default).then(|| URL_SAFE_NO_PAD.encode(key.terminal_id.as_bytes())),
        );
        let (current, old_encoded) = if encoded.len() <= MAX_FILENAME_BYTES {
            (encoded, None)
        } else {
            (
                filename(
                    hashed_thread(&key.thread_id),
                    (!default).then(|| hashed_id(&key.terminal_id)),
                ),
                Some(encoded),
            )
        };
        Self {
            current,
            old_encoded,
            old_flat: default.then(|| legacy_name(&key.thread_id)).flatten(),
        }
    }

    fn candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.current.as_str())
            .chain(self.old_encoded.as_deref())
            .chain(self.old_flat.as_deref())
    }
}

fn legacy_name(thread_id: &str) -> Option<String> {
    let sanitized: String = thread_id
        .encode_utf16()
        .map(|unit| {
            let ascii = u8::try_from(unit).ok();
            match ascii {
                Some(byte)
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') =>
                {
                    char::from(byte)
                }
                _ => '_',
            }
        })
        .collect();
    (!sanitized.to_ascii_lowercase().starts_with("terminal_")).then(|| format!("{sanitized}.log"))
}

fn encoded_thread(thread_id: &str) -> String {
    format!("terminal_{}", URL_SAFE_NO_PAD.encode(thread_id.as_bytes()))
}

fn hashed_thread(thread_id: &str) -> String {
    format!("terminal_sha256.{}", hashed_id(thread_id))
}

fn hashed_id(id: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, id.as_bytes()).as_ref())
}

fn filename(thread: String, terminal: Option<String>) -> String {
    match terminal {
        Some(terminal) => format!("{thread}_{terminal}.log"),
        None => format!("{thread}.log"),
    }
}

fn make_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path)?.is_dir() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "The terminal history directory must be a directory.",
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn regular_file(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Terminal history must be a regular file.",
        ))
    }
}

fn remove_file(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("Could not delete terminal history: {error}");
    }
}

pub(super) fn read_tail(path: &Path, max_bytes: usize) -> io::Result<(String, bool)> {
    regular_file(path)?;
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let offset = size.saturating_sub(max_bytes as u64);
    let mut remaining = offset;
    let mut state = Sequence::Ground;
    let mut decoder = Utf8Output::default();
    let mut skipped = vec![0; (offset as usize).min(DISCARD_BUFFER_BYTES)];
    while remaining > 0 {
        let capacity = skipped.len().min(remaining as usize);
        let count = file.read(&mut skipped[..capacity])?;
        if count == 0 {
            break;
        }
        remaining -= count as u64;
        state = state.after(&decoder.decode(&skipped[..count]));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64).read_to_end(&mut bytes)?;
    let start = if offset == 0 {
        0
    } else {
        bytes
            .iter()
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count()
    };
    state = state.after(&decoder.decode(&bytes[..start]));
    state = state.after(&decoder.finish());
    let history = String::from_utf8_lossy(&bytes[start..]);
    let (drop, _) = state.until_ground(&history);
    Ok((history[drop..].to_owned(), offset > 0))
}
