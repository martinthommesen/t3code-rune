mod history;
mod persistence;
mod process;
mod scrub;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::broadcast;

use history::{Cursor, History, PERSISTED_HISTORY_BYTES, PERSISTED_HISTORY_LINES};
use persistence::{Persistence, Storage};
use process::Process;
use scrub::Scrubber;

const MAX_INACTIVE_SESSIONS: usize = 128;
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;
const MAX_WRITE_UNITS: usize = 65_536;
const EVENT_CAPACITY: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TerminalKey {
    pub thread_id: String,
    pub terminal_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OpenTerminal {
    #[serde(flatten)]
    pub key: TerminalKey,
    pub cwd: PathBuf,
    #[serde(default, deserialize_with = "present_worktree_path")]
    pub worktree_path: Option<Option<PathBuf>>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub(super) fn present_worktree_path<'de, D: Deserializer<'de>>(
    input: D,
) -> Result<Option<Option<PathBuf>>, D::Error> {
    Option::<PathBuf>::deserialize(input).map(Some)
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Running,
    Exited,
    Error,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TerminalSnapshot {
    #[serde(flatten)]
    key: TerminalKey,
    cwd: PathBuf,
    worktree_path: Option<PathBuf>,
    status: Status,
    pid: Option<u32>,
    history: String,
    exit_code: Option<u32>,
    exit_signal: Option<i32>,
    label: String,
    #[serde(skip)]
    shell: Option<PathBuf>,
    updated_at: String,
    sequence: u64,
    cols: u16,
    rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl TerminalSnapshot {
    pub(super) fn is_running(&self) -> bool {
        matches!(self.status, Status::Running)
    }

    pub(super) fn shell(&self) -> Option<&Path> {
        self.shell.as_deref()
    }

    pub(super) fn retain_history(&mut self, bytes: usize) {
        history::retain_tail(&mut self.history, bytes);
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TerminalEvent {
    #[serde(flatten)]
    key: TerminalKey,
    sequence: u64,
    #[serde(flatten)]
    event: OutputEvent,
}

impl TerminalEvent {
    pub(super) fn output(&self) -> &OutputEvent {
        &self.event
    }
}

#[derive(Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub(super) enum OutputEvent {
    Output {
        data: String,
    },
    Exited {
        exit_code: Option<u32>,
        exit_signal: Option<i32>,
    },
    Closed,
    Error {
        message: String,
    },
    Cleared,
    Restarted {
        snapshot: Box<TerminalSnapshot>,
    },
    Activity {
        has_running_subprocess: bool,
        label: String,
        cols: u16,
        rows: u16,
    },
}

#[derive(Debug)]
pub(super) enum TerminalError {
    CwdNotFound(PathBuf),
    CwdNotDirectory(PathBuf),
    CwdInaccessible(PathBuf, io::Error),
    Unknown(TerminalKey),
    NotRunning(TerminalKey),
    Write(TerminalKey, u32, io::Error),
    Resize(TerminalKey, u32, Grid, String),
    History(TerminalKey, &'static str, io::Error),
    Invalid(&'static str),
}

impl TerminalError {
    pub fn wire(&self) -> Value {
        let mut error = match self {
            Self::CwdNotFound(cwd) => json!({"_tag": "TerminalCwdNotFoundError", "cwd": cwd}),
            Self::CwdNotDirectory(cwd) => {
                json!({"_tag": "TerminalCwdNotDirectoryError", "cwd": cwd})
            }
            Self::CwdInaccessible(cwd, cause) => json!({
                "_tag": "TerminalCwdStatError", "cwd": cwd, "cause": cause.to_string()
            }),
            Self::Unknown(key) => json!({
                "_tag": "TerminalSessionLookupError", "threadId": key.thread_id,
                "terminalId": key.terminal_id
            }),
            Self::NotRunning(key) => json!({
                "_tag": "TerminalNotRunningError", "threadId": key.thread_id,
                "terminalId": key.terminal_id
            }),
            Self::Write(key, pid, cause) => json!({
                "_tag": "TerminalWriteError", "threadId": key.thread_id,
                "terminalId": key.terminal_id, "terminalPid": pid, "cause": cause.to_string()
            }),
            Self::Resize(key, pid, grid, cause) => json!({
                "_tag": "TerminalResizeError", "threadId": key.thread_id,
                "terminalId": key.terminal_id, "terminalPid": pid,
                "cols": grid.cols, "rows": grid.rows, "cause": cause
            }),
            Self::History(key, operation, cause) => json!({
                "_tag": "TerminalHistoryError", "threadId": key.thread_id,
                "terminalId": key.terminal_id, "operation": operation, "cause": cause.to_string()
            }),
            Self::Invalid(message) => json!({"_tag": "InvalidTerminalInput", "message": message}),
        };
        error["message"] = json!(self.to_string());
        error
    }
}

impl fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CwdNotFound(cwd) => write!(
                formatter,
                "Terminal folder does not exist: {}. Restore the folder at this path before retrying.",
                cwd.display()
            ),
            Self::CwdNotDirectory(cwd) => write!(
                formatter,
                "Terminal cwd is not a directory: {}",
                cwd.display()
            ),
            Self::CwdInaccessible(cwd, _) => write!(
                formatter,
                "Failed to access terminal cwd: {}",
                cwd.display()
            ),
            Self::Unknown(key) => write!(
                formatter,
                "Unknown terminal thread: {}, terminal: {}",
                key.thread_id, key.terminal_id
            ),
            Self::NotRunning(key) => write!(
                formatter,
                "Terminal is not running for thread: {}, terminal: {}",
                key.thread_id, key.terminal_id
            ),
            Self::Write(key, pid, _) => write!(
                formatter,
                "Failed to write to terminal for thread: {}, terminal: {}, PID: {pid}",
                key.thread_id, key.terminal_id
            ),
            Self::Resize(key, pid, grid, _) => write!(
                formatter,
                "Failed to resize terminal for thread: {}, terminal: {}, PID: {pid} to {}x{}",
                key.thread_id, key.terminal_id, grid.cols, grid.rows
            ),
            Self::History(key, operation, _) => write!(
                formatter,
                "Failed to {operation} terminal history for thread: {}, terminal: {}",
                key.thread_id, key.terminal_id
            ),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TerminalError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Grid {
    cols: u16,
    rows: u16,
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

impl Grid {
    fn validate(self) -> Result<Self, TerminalError> {
        if (1..=1000).contains(&self.cols) && (1..=500).contains(&self.rows) {
            Ok(self)
        } else {
            Err(TerminalError::Invalid(
                "Terminal dimensions are out of range.",
            ))
        }
    }
}

enum Phase {
    Closed,
    Running(Process),
    Exited(Exit),
    Failed(String),
}

#[derive(Clone, Copy)]
struct Exit {
    code: Option<u32>,
    signal: Option<i32>,
}

struct State {
    cwd: PathBuf,
    worktree_path: Option<PathBuf>,
    env: BTreeMap<String, String>,
    stop_on_settle: bool,
    phase: Phase,
    grid: Grid,
    history: History,
    scrubber: Scrubber,
    persisted: Option<Cursor>,
    history_dirty: bool,
    sequence: u64,
    generation: u64,
    updated_at: OffsetDateTime,
    touched: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            cwd: PathBuf::new(),
            worktree_path: None,
            env: BTreeMap::new(),
            stop_on_settle: false,
            phase: Phase::Closed,
            grid: Grid::default(),
            history: History::default(),
            scrubber: Scrubber::default(),
            persisted: None,
            history_dirty: false,
            sequence: 0,
            generation: 0,
            updated_at: OffsetDateTime::now_utc(),
            touched: Instant::now(),
        }
    }
}

struct Session {
    key: TerminalKey,
    state: Mutex<State>,
    events: broadcast::Sender<TerminalEvent>,
    storage: Option<Arc<Storage>>,
}

impl Session {
    fn new(key: TerminalKey, storage: Option<Arc<Storage>>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            key,
            state: Mutex::default(),
            events,
            storage,
        }
    }

    fn snapshot(&self, state: &State) -> TerminalSnapshot {
        let (status, pid, exit, error) = match &state.phase {
            Phase::Closed => (Status::Exited, None, None, None),
            Phase::Running(process) => (Status::Running, Some(process.pid), None, None),
            Phase::Exited(exit) => (Status::Exited, None, Some(*exit), None),
            Phase::Failed(message) => (Status::Error, None, None, Some(message.clone())),
        };
        TerminalSnapshot {
            key: self.key.clone(),
            cwd: state.cwd.clone(),
            worktree_path: state.worktree_path.clone(),
            status,
            pid,
            history: state.history.text(),
            exit_code: exit.and_then(|exit| exit.code),
            exit_signal: exit.and_then(|exit| exit.signal),
            label: terminal_label(&self.key.terminal_id),
            shell: match &state.phase {
                Phase::Running(process) => Some(process.shell.clone()),
                _ => None,
            },
            updated_at: state
                .updated_at
                .format(&Rfc3339)
                .expect("current UTC time is RFC3339 compatible"),
            sequence: state.sequence,
            cols: state.grid.cols,
            rows: state.grid.rows,
            error,
        }
    }

    fn publish(&self, state: &mut State, event: impl FnOnce(&State) -> OutputEvent) {
        state.updated_at = OffsetDateTime::now_utc();
        state.touched = Instant::now();
        state.sequence += 1;
        let _ = self.events.send(TerminalEvent {
            key: self.key.clone(),
            sequence: state.sequence,
            event: event(state),
        });
    }

    fn output(self: &Arc<Self>, generation: u64, data: String) {
        let mut state = lock(&self.state);
        if generation == state.generation && matches!(state.phase, Phase::Running(_)) {
            let visible = state.scrubber.push(&data);
            let changed = !visible.is_empty();
            if changed {
                state.history.append(&visible);
                state.history_dirty = true;
            }
            self.publish(&mut state, |_| OutputEvent::Output { data });
            if changed && let Some(storage) = &self.storage {
                storage.queue(self);
            }
        }
    }

    fn exit(&self, generation: u64, exit: io::Result<Exit>) {
        let mut state = lock(&self.state);
        if generation != state.generation || !matches!(state.phase, Phase::Running(_)) {
            return;
        }
        state.scrubber.clear();
        self.persist(&mut state, false);
        match exit {
            Ok(exit) => {
                state.phase = Phase::Exited(exit);
                self.publish(&mut state, |_| OutputEvent::Exited {
                    exit_code: exit.code,
                    exit_signal: exit.signal,
                });
            }
            Err(error) => {
                let message = format!("Could not wait for the terminal shell: {error}");
                state.phase = Phase::Failed(message.clone());
                self.publish(&mut state, |_| OutputEvent::Error { message });
            }
        }
    }

    fn flush_history(&self, force: bool) {
        self.persist(&mut lock(&self.state), force);
    }

    fn persist(&self, state: &mut State, force: bool) {
        let Some(storage) = &self.storage else { return };
        if !force && !state.history_dirty {
            return;
        }
        let cursor = state.persisted.take().filter(|_| !force);
        let update = state.history.persistence_update(cursor);
        match storage.write(&self.key, &update.data, update.append) {
            Ok(()) => {
                state.persisted = Some(update.cursor);
                state.history_dirty = false;
            }
            Err(error) => eprintln!("Could not persist terminal history: {error}"),
        }
    }

    fn write_error(&self, generation: u64, error: io::Error) {
        let mut state = lock(&self.state);
        if generation == state.generation && matches!(state.phase, Phase::Running(_)) {
            self.publish(&mut state, |_| OutputEvent::Error {
                message: format!("Could not write to the terminal shell: {error}"),
            });
        }
    }

    fn resize(&self, state: &mut State, grid: Grid) -> Result<(), TerminalError> {
        let Phase::Running(process) = &state.phase else {
            return Ok(());
        };
        if grid == state.grid {
            return Ok(());
        }
        process
            .resize(grid)
            .map_err(|cause| TerminalError::Resize(self.key.clone(), process.pid, grid, cause))?;
        let label = terminal_label(&self.key.terminal_id);
        state.grid = grid;
        self.publish(state, |_| OutputEvent::Activity {
            has_running_subprocess: false,
            label,
            cols: grid.cols,
            rows: grid.rows,
        });
        Ok(())
    }

    fn close(&self, state: &mut State) {
        if !matches!(state.phase, Phase::Closed) {
            state.phase = Phase::Closed;
            state.scrubber.clear();
            self.persist(state, true);
            self.publish(state, |_| OutputEvent::Closed);
        }
    }
}

#[derive(Default)]
pub(super) struct TerminalManager {
    sessions: Mutex<HashMap<TerminalKey, Arc<Session>>>,
    persistence: Option<Persistence>,
}

impl TerminalManager {
    pub(super) fn with_state_dir(state_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            sessions: Mutex::default(),
            persistence: Some(Persistence::new(state_dir)?),
        })
    }

    pub fn open(&self, input: OpenTerminal) -> Result<TerminalSnapshot, TerminalError> {
        self.open_inner(input, None, |_| ())
            .map(|(snapshot, ())| snapshot)
    }

    pub(super) fn open_observed(
        &self,
        input: OpenTerminal,
        stop_on_settle: bool,
    ) -> Result<(TerminalSnapshot, broadcast::Receiver<TerminalEvent>), TerminalError> {
        self.open_inner(input, Some(stop_on_settle), |session| {
            session.events.subscribe()
        })
    }

    fn open_inner<R>(
        &self,
        input: OpenTerminal,
        stop_on_settle: Option<bool>,
        observe: impl FnOnce(&Session) -> R,
    ) -> Result<(TerminalSnapshot, R), TerminalError> {
        validate_open(&input)?;
        let mut sessions = lock(&self.sessions);
        let new = !sessions.contains_key(&input.key);
        if new {
            evict_inactive(&mut sessions);
        }
        let session = Arc::clone(sessions.entry(input.key.clone()).or_insert_with(|| {
            Arc::new(Session::new(
                input.key.clone(),
                self.persistence
                    .as_ref()
                    .map(|persistence| Arc::clone(&persistence.storage)),
            ))
        }));
        let mut state = lock(&session.state);
        drop(sessions);
        let receiver = observe(&session);
        if let Some(stop_on_settle) = stop_on_settle {
            state.stop_on_settle = stop_on_settle;
        }

        let grid = Grid {
            cols: input.cols.unwrap_or(state.grid.cols),
            rows: input.rows.unwrap_or(state.grid.rows),
        };
        let worktree_path = input
            .worktree_path
            .unwrap_or_else(|| state.worktree_path.clone());
        let context_changed = state.cwd != input.cwd
            || state.worktree_path != worktree_path
            || state.env != input.env;

        if !context_changed && matches!(state.phase, Phase::Running(_)) {
            session.resize(&mut state, grid)?;
            return Ok((session.snapshot(&state), receiver));
        }
        let cold_storage = session
            .storage
            .as_ref()
            .filter(|_| matches!(state.phase, Phase::Closed));
        if let Some(storage) = cold_storage {
            let history = storage.read(&session.key).map_err(|error| {
                TerminalError::History(session.key.clone(), error.operation, error.error)
            })?;
            let replay = history::neutralize_inherited(
                &history.text(),
                PERSISTED_HISTORY_LINES,
                PERSISTED_HISTORY_BYTES,
            );
            state.history = History::with_limits(PERSISTED_HISTORY_LINES, PERSISTED_HISTORY_BYTES);
            state.history.append(&replay);
            state.persisted = None;
            state.history_dirty = false;
            state.sequence = 0;
        } else if context_changed || !matches!(state.phase, Phase::Closed) {
            state.history.clear();
            state.history_dirty = true;
            session.persist(&mut state, true);
        }
        state.scrubber.clear();

        state.phase = Phase::Closed;
        state.cwd = input.cwd;
        state.worktree_path = worktree_path;
        state.env = input.env;
        state.grid = grid;
        state.generation += 1;
        state.phase = match Process::spawn(&session, &state) {
            Ok(process) => Phase::Running(process),
            Err(message) => Phase::Failed(message),
        };
        if !new {
            session.publish(&mut state, |state| OutputEvent::Restarted {
                snapshot: Box::new(session.snapshot(state)),
            });
        } else if cold_storage.is_some() && matches!(state.phase, Phase::Running(_)) {
            state.sequence += 1;
        }
        if let Phase::Failed(message) = &state.phase {
            let message = message.clone();
            session.publish(&mut state, |_| OutputEvent::Error { message });
        }
        Ok((session.snapshot(&state), receiver))
    }

    pub fn observe(
        &self,
        key: &TerminalKey,
    ) -> Result<(TerminalSnapshot, broadcast::Receiver<TerminalEvent>), TerminalError> {
        let session = self.session(key)?;
        let state = lock(&session.state);
        if matches!(state.phase, Phase::Closed) {
            return Err(TerminalError::Unknown(key.clone()));
        }
        Ok((session.snapshot(&state), session.events.subscribe()))
    }

    pub(super) fn has_active_subprocess(
        &self,
        key: &TerminalKey,
    ) -> Result<Option<bool>, TerminalError> {
        let session = self.session(key)?;
        let state = lock(&session.state);
        match &state.phase {
            Phase::Running(process) => Ok(process.has_active_subprocess()),
            _ => Err(TerminalError::NotRunning(key.clone())),
        }
    }

    pub(super) fn close_idle(&self, key: &TerminalKey) -> Result<bool, TerminalError> {
        let session = self.session(key)?;
        let mut state = lock(&session.state);
        if let Phase::Running(process) = &state.phase
            && process.has_active_subprocess() == Some(false)
        {
            session.close(&mut state);
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn write_if_idle(
        &self,
        key: &TerminalKey,
        cwd: &Path,
        worktree: Option<&Path>,
        env: &BTreeMap<String, String>,
        data: &str,
    ) -> Result<bool, TerminalError> {
        validate_write(data)?;
        let session = match self.session(key) {
            Ok(session) => session,
            Err(TerminalError::Unknown(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let state = lock(&session.state);
        let Phase::Running(process) = &state.phase else {
            return Ok(false);
        };
        if state.cwd != cwd
            || state.worktree_path.as_deref() != worktree
            || &state.env != env
            || state.stop_on_settle
            || process.has_active_subprocess() != Some(false)
        {
            return Ok(false);
        }
        process
            .write(data.as_bytes().to_vec())
            .map_err(|cause| TerminalError::Write(key.clone(), process.pid, cause))?;
        Ok(true)
    }

    pub fn write(&self, key: &TerminalKey, data: &str, binary: bool) -> Result<(), TerminalError> {
        validate_write(data)?;
        let session = self.session(key)?;
        let state = lock(&session.state);
        match &state.phase {
            Phase::Running(process) => {
                let bytes = if binary {
                    data.encode_utf16().map(|unit| unit as u8).collect()
                } else {
                    data.as_bytes().to_vec()
                };
                process
                    .write(bytes)
                    .map_err(|cause| TerminalError::Write(key.clone(), process.pid, cause))
            }
            Phase::Exited(_) => Ok(()),
            Phase::Closed => Err(TerminalError::Unknown(key.clone())),
            Phase::Failed(_) => Err(TerminalError::NotRunning(key.clone())),
        }
    }

    pub fn resize(&self, key: &TerminalKey, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.resize_optional(key, Some(cols), Some(rows))
    }

    pub(super) fn resize_optional(
        &self,
        key: &TerminalKey,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<(), TerminalError> {
        Grid {
            cols: cols.unwrap_or(DEFAULT_COLS),
            rows: rows.unwrap_or(DEFAULT_ROWS),
        }
        .validate()?;
        if let Some(session) = lock(&self.sessions).get(key).cloned() {
            let mut state = lock(&session.state);
            let grid = Grid {
                cols: cols.unwrap_or(state.grid.cols),
                rows: rows.unwrap_or(state.grid.rows),
            };
            session.resize(&mut state, grid)?;
        }
        Ok(())
    }

    pub(super) fn closed_event(&self, key: &TerminalKey) -> Option<TerminalEvent> {
        let session = self.session(key).ok()?;
        let state = lock(&session.state);
        matches!(state.phase, Phase::Closed).then(|| TerminalEvent {
            key: key.clone(),
            sequence: state.sequence,
            event: OutputEvent::Closed,
        })
    }

    pub fn clear(&self, key: &TerminalKey) -> Result<(), TerminalError> {
        let session = self.session(key)?;
        let mut state = lock(&session.state);
        if matches!(state.phase, Phase::Closed) {
            return Err(TerminalError::Unknown(key.clone()));
        }
        state.history.clear();
        state.scrubber.clear();
        state.history_dirty = true;
        session.persist(&mut state, true);
        session.publish(&mut state, |_| OutputEvent::Cleared);
        Ok(())
    }

    pub fn close(&self, thread_id: &str, terminal_id: Option<&str>, delete_history: bool) {
        if let Err(error) = self.close_with_history(thread_id, terminal_id, delete_history) {
            eprintln!("Could not delete terminal history: {error}");
        }
    }

    pub(super) fn cleanup_deleted_thread(&self, thread_id: &str) -> io::Result<()> {
        self.close_with_history(thread_id, None, true)
    }

    fn close_with_history(
        &self,
        thread_id: &str,
        terminal_id: Option<&str>,
        delete_history: bool,
    ) -> io::Result<()> {
        let mut sessions = lock(&self.sessions);
        for (key, session) in sessions.iter() {
            if key.thread_id != thread_id || terminal_id.is_some_and(|id| id != key.terminal_id) {
                continue;
            }
            let mut state = lock(&session.state);
            session.close(&mut state);
            if delete_history {
                state.history.clear();
                state.persisted = None;
                state.history_dirty = false;
            }
        }
        let history_result = if delete_history && let Some(persistence) = &self.persistence {
            if let Some(terminal_id) = terminal_id {
                persistence.storage.delete(&TerminalKey {
                    thread_id: thread_id.to_owned(),
                    terminal_id: terminal_id.to_owned(),
                });
                Ok(())
            } else {
                persistence.storage.delete_thread(thread_id)
            }
        } else {
            Ok(())
        };
        evict_inactive(&mut sessions);
        history_result
    }

    fn session(&self, key: &TerminalKey) -> Result<Arc<Session>, TerminalError> {
        lock(&self.sessions)
            .get(key)
            .cloned()
            .ok_or_else(|| TerminalError::Unknown(key.clone()))
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        if self.persistence.is_none() {
            return;
        }
        for session in lock(&self.sessions).values() {
            let mut state = lock(&session.state);
            state.phase = Phase::Closed;
            session.persist(&mut state, false);
        }
    }
}

fn terminal_label(terminal_id: &str) -> String {
    let number = (|| {
        let (prefix, suffix) = terminal_id.split_once('-')?;
        if !prefix.eq_ignore_ascii_case("term") && !prefix.eq_ignore_ascii_case("terminal") {
            return None;
        }
        let (digits, uuid) = match suffix.split_once('-') {
            Some((digits, uuid)) => (digits, Some(uuid)),
            None => (suffix, None),
        };
        (!digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && uuid.is_none_or(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()))
        .then_some(digits)
    })();
    match number {
        Some(digits) => format!("Terminal {digits}").chars().take(128).collect(),
        None => terminal_id.to_owned(),
    }
}

fn evict_inactive(sessions: &mut HashMap<TerminalKey, Arc<Session>>) {
    let mut inactive: Vec<_> = sessions
        .values()
        .filter_map(|session| {
            let state = lock(&session.state);
            (!matches!(state.phase, Phase::Running(_)))
                .then_some((state.touched, session.key.clone()))
        })
        .collect();
    if inactive.len() <= MAX_INACTIVE_SESSIONS {
        return;
    }
    inactive.sort_unstable_by_key(|(touched, _)| *touched);
    let excess = inactive.len() - MAX_INACTIVE_SESSIONS;
    for (_, key) in inactive.into_iter().take(excess) {
        sessions.remove(&key);
    }
}

fn validate_write(data: &str) -> Result<(), TerminalError> {
    let length = data.encode_utf16().count();
    if length == 0 || length > MAX_WRITE_UNITS {
        return Err(TerminalError::Invalid(
            "Terminal input is empty or exceeds the write limit.",
        ));
    }
    Ok(())
}

fn validate_open(input: &OpenTerminal) -> Result<(), TerminalError> {
    if input.key.thread_id.trim().is_empty()
        || input.key.terminal_id.trim().is_empty()
        || input.key.terminal_id.encode_utf16().count() > 128
        || input.key.thread_id.contains('\0')
        || input.key.terminal_id.contains('\0')
    {
        return Err(TerminalError::Invalid(
            "Invalid terminal or thread identifier.",
        ));
    }
    Grid {
        cols: input.cols.unwrap_or(DEFAULT_COLS),
        rows: input.rows.unwrap_or(DEFAULT_ROWS),
    }
    .validate()?;
    if input.env.len() > 128
        || input.env.iter().any(|(key, value)| {
            let mut bytes = key.bytes();
            key.len() > 128
                || !bytes
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
                || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || value.encode_utf16().count() > 8192
                || value.contains('\0')
        })
    {
        return Err(TerminalError::Invalid("Invalid terminal environment."));
    }
    validate_cwd(&input.cwd)
}

fn validate_cwd(cwd: &Path) -> Result<(), TerminalError> {
    match cwd.metadata() {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(TerminalError::CwdNotDirectory(cwd.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(TerminalError::CwdNotFound(cwd.to_owned()))
        }
        Err(error) => Err(TerminalError::CwdInaccessible(cwd.to_owned(), error)),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
