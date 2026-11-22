use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use super::history::Utf8Output;
use super::{Exit, Grid, Session, State};

#[cfg(target_os = "linux")]
mod activity;

const WRITE_QUEUE_CAPACITY: usize = 8;
const EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const CHUNK_BYTES: usize = 8192;

pub(super) struct Process {
    master: Box<dyn MasterPty + Send>,
    write: SyncSender<Vec<u8>>,
    stop: SyncSender<()>,
    pub(super) pid: u32,
    pub(super) shell: PathBuf,
}

impl Process {
    pub(super) fn spawn(session: &Arc<Session>, state: &State) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(size(state.grid))
            .map_err(|error| error.to_string())?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| error.to_string())?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|error| error.to_string())?;
        let env = shell_environment(&state.env, &session.key.thread_id);
        let (child, shell) = spawn_shell(pair.slave.as_ref(), &state.cwd, &state.env, &env)?;
        drop(pair.slave);
        let mut child = UnsupervisedChild(Some(child));
        let pid = child
            .0
            .as_ref()
            .and_then(|child| child.process_id())
            .ok_or_else(|| "Terminal shell did not return its process ID.".to_owned())?;
        let generation = state.generation;

        let (write, writes) = mpsc::sync_channel::<Vec<u8>>(WRITE_QUEUE_CAPACITY);
        let (stop, stops) = mpsc::sync_channel(1);
        let (output_closed, output_closes) = mpsc::channel();

        let writer_session = Arc::downgrade(session);
        thread::Builder::new()
            .name("t3-terminal-write".into())
            .spawn(move || {
                while let Ok(bytes) = writes.recv() {
                    if let Err(error) = writer.write_all(&bytes) {
                        if let Some(session) = writer_session.upgrade() {
                            session.write_error(generation, error);
                        }
                        break;
                    }
                }
            })
            .map_err(|error| error.to_string())?;

        let reader_session = Arc::downgrade(session);
        thread::Builder::new()
            .name("t3-terminal-read".into())
            .spawn(move || {
                read_output(reader.as_mut(), reader_session, generation);
                let _ = output_closed.send(());
            })
            .map_err(|error| error.to_string())?;

        let child_session = Arc::downgrade(session);
        thread::Builder::new()
            .name("t3-terminal-child".into())
            .spawn(move || {
                let mut child = child
                    .0
                    .take()
                    .expect("terminal child is present until supervision starts");
                let exit = supervise(child.as_mut(), &stops).map(exit_status);
                let _ = output_closes.recv_timeout(OUTPUT_DRAIN_TIMEOUT);
                if let Some(session) = child_session.upgrade() {
                    session.exit(generation, exit);
                }
            })
            .map_err(|error| error.to_string())?;

        Ok(Self {
            master: pair.master,
            write,
            stop,
            pid,
            shell,
        })
    }

    pub(super) fn resize(&self, grid: Grid) -> Result<(), String> {
        self.master
            .resize(size(grid))
            .map_err(|error| error.to_string())
    }

    pub(super) fn write(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.write.try_send(bytes).map_err(|error| match error {
            TrySendError::Full(_) => {
                io::Error::new(io::ErrorKind::WouldBlock, "Terminal input queue is full.")
            }
            TrySendError::Disconnected(_) => io::Error::new(
                io::ErrorKind::BrokenPipe,
                "The terminal stopped reading input.",
            ),
        })
    }

    pub(super) fn has_active_subprocess(&self) -> Option<bool> {
        #[cfg(target_os = "linux")]
        {
            activity::inspect(self.pid, &self.shell, self.master.process_group_leader())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
    }
}

struct UnsupervisedChild(Option<Box<dyn Child + Send + Sync>>);

impl Drop for UnsupervisedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = stop_child(child.as_mut());
        }
    }
}

fn supervise(child: &mut dyn Child, stops: &mpsc::Receiver<()>) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        match stops.recv_timeout(EXIT_CHECK_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return stop_child(child),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn stop_child(child: &mut dyn Child) -> io::Result<ExitStatus> {
    match child.kill() {
        Ok(()) => child.wait(),
        Err(error) => child.try_wait()?.ok_or(error),
    }
}

fn exit_status(status: ExitStatus) -> Exit {
    let Some(signal) = status.signal() else {
        return Exit {
            code: Some(status.exit_code()),
            signal: None,
        };
    };
    let signal = match signal {
        "Hangup" => Some(1),
        "Interrupt" => Some(2),
        "Quit" => Some(3),
        "Abort trap" | "Aborted" => Some(6),
        "Killed" => Some(9),
        "Broken pipe" => Some(13),
        "Alarm clock" => Some(14),
        "Terminated" => Some(15),
        name => name
            .strip_prefix("Signal ")
            .and_then(|number| number.parse().ok()),
    };
    Exit { code: None, signal }
}

fn read_output(reader: &mut dyn Read, session: Weak<Session>, generation: u64) {
    let mut decoder = Utf8Output::default();
    let mut bytes = [0; CHUNK_BYTES];
    loop {
        let data = match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(length) => decoder.decode(&bytes[..length]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let Some(session) = session.upgrade() else {
            return;
        };
        if !data.is_empty() {
            session.output(generation, data);
        }
    }
    let data = decoder.finish();
    if !data.is_empty()
        && let Some(session) = session.upgrade()
    {
        session.output(generation, data);
    }
}

fn size(grid: Grid) -> PtySize {
    PtySize {
        rows: grid.rows,
        cols: grid.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn spawn_shell(
    slave: &dyn portable_pty::SlavePty,
    cwd: &Path,
    overrides: &BTreeMap<String, String>,
    env: &BTreeMap<OsString, OsString>,
) -> Result<(Box<dyn Child + Send + Sync>, PathBuf), String> {
    let candidates = shell_candidates(overrides);
    let mut last_error = String::new();
    for shell in candidates {
        let label = Path::new(&shell)
            .file_name()
            .unwrap_or(shell.as_os_str())
            .to_string_lossy();
        let mut command = CommandBuilder::new(&shell);
        command.cwd(cwd);
        command.env_clear();
        for (key, value) in env {
            command.env(key, value);
        }
        shell_arguments(&mut command, &label);
        match slave.spawn_command(command) {
            Ok(child) => return Ok((child, shell.into())),
            Err(error) => last_error = format!("{}: {error}", shell.to_string_lossy()),
        }
    }
    Err(format!("Could not start a terminal shell. {last_error}"))
}

fn shell_candidates(overrides: &BTreeMap<String, String>) -> Vec<OsString> {
    let configured = overrides
        .get("T3CODE_TERMINAL_SHELL")
        .map(OsString::from)
        .or_else(|| std::env::var_os("T3CODE_TERMINAL_SHELL"));
    let mut candidates = Vec::new();
    let mut add = |shell: OsString| {
        if !shell.is_empty() && !candidates.contains(&shell) {
            candidates.push(shell);
        }
    };
    if let Some(configured) = configured {
        add(configured.to_string_lossy().trim().into());
    }
    if cfg!(windows) {
        add("pwsh.exe".into());
        let windows = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("windir"))
            .unwrap_or_else(|| "C:\\Windows".into());
        let system32 = Path::new(&windows).join("System32");
        add(system32
            .join("WindowsPowerShell/v1.0/powershell.exe")
            .into_os_string());
        add("powershell.exe".into());
        if let Some(comspec) = std::env::var_os("ComSpec") {
            add(comspec);
        }
        add(system32.join("cmd.exe").into_os_string());
        add("cmd.exe".into());
    } else {
        if let Some(shell) = std::env::var_os("SHELL") {
            add(shell);
        }
        for shell in ["/bin/zsh", "/bin/bash", "/bin/sh", "zsh", "bash", "sh"] {
            add(shell.into());
        }
    }
    candidates
}

fn shell_arguments(command: &mut CommandBuilder, label: &str) {
    let label = label.to_ascii_lowercase();
    if cfg!(windows) && matches!(label.as_str(), "pwsh.exe" | "powershell.exe") {
        command.args([
            "-NoLogo",
            "-NoExit",
            "-Command",
            "[Console]::OutputEncoding = $OutputEncoding = [System.Text.UTF8Encoding]::new()",
        ]);
    } else if cfg!(windows) && matches!(label.as_str(), "bash.exe" | "bash") {
        command.args(["--login", "-i"]);
    } else if !cfg!(windows) && label == "zsh" {
        command.args(["-o", "nopromptsp"]);
    } else if cfg!(target_os = "macos") && label == "bash" {
        command.arg("-l");
    }
}

fn shell_environment(
    overrides: &BTreeMap<String, String>,
    thread_id: &str,
) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<_, _> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let normalized = key.to_string_lossy().to_ascii_uppercase();
            let private = normalized.starts_with("T3CODE_")
                || normalized.starts_with("VITE_")
                || matches!(
                    normalized.as_str(),
                    "PORT"
                        | "ELECTRON_RENDERER_PORT"
                        | "ELECTRON_RUN_AS_NODE"
                        | "TMUX"
                        | "TMUX_PANE"
                        | "STY"
                        | "WINDOW"
                        | "ZELLIJ"
                        | "ZELLIJ_SESSION_NAME"
                        | "ZELLIJ_PANE_ID"
                );
            (!private).then_some((
                if cfg!(windows) {
                    normalized.into()
                } else {
                    key
                },
                value,
            ))
        })
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("TERM_PROGRAM".into(), "t3code".into());
    env.insert(
        "TERM_PROGRAM_VERSION".into(),
        env!("CARGO_PKG_VERSION").into(),
    );
    for (key, value) in overrides {
        let key: OsString = if cfg!(windows) {
            key.to_ascii_uppercase().into()
        } else {
            key.into()
        };
        env.insert(key, value.into());
    }
    env.insert("T3CODE_THREAD_ID".into(), thread_id.into());
    env.entry("COLORTERM".into())
        .or_insert_with(|| "truecolor".into());
    let features_key = if cfg!(windows) {
        "FISH_FEATURES"
    } else {
        "fish_features"
    };
    let features = env.entry(features_key.into()).or_default();
    if !features.to_string_lossy().contains("query-term") {
        if !features.is_empty() {
            features.push(",");
        }
        features.push("no-query-term");
    }
    strip_appimage_environment(&mut env);
    env
}

fn strip_appimage_environment(env: &mut BTreeMap<OsString, OsString>) {
    if !env.contains_key(OsStr::new("APPIMAGE")) && !env.contains_key(OsStr::new("APPDIR")) {
        return;
    }
    let directory = env.remove(OsStr::new("APPDIR"));
    for key in ["APPIMAGE", "ARGV0", "OWD"] {
        env.remove(OsStr::new(key));
    }
    let Some(directory) = directory.filter(|directory| !directory.is_empty()) else {
        return;
    };
    for key in [
        "PATH",
        "LD_LIBRARY_PATH",
        "XDG_DATA_DIRS",
        "GSETTINGS_SCHEMA_DIR",
    ] {
        let key = OsStr::new(key);
        let Some(value) = env.remove(key) else {
            continue;
        };
        let paths: Vec<_> = std::env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty() && !path.starts_with(Path::new(&directory)))
            .collect();
        if !paths.is_empty()
            && let Ok(value) = std::env::join_paths(paths)
        {
            env.insert(key.to_owned(), value);
        }
    }
}
