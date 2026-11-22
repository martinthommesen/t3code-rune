use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use reqwest::Url;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{Instant, interval, sleep_until};

const STALL_AFTER: Duration = Duration::from_secs(20);
const LFS_POLL: Duration = Duration::from_secs(2);
const MAX_LINE_BYTES: usize = 16 * 1024;
const STAGES: [(&str, Stage); 8] = [
    ("remote: Enumerating objects", Stage::Counting),
    ("remote: Counting objects", Stage::Counting),
    ("remote: Compressing objects", Stage::Counting),
    ("Receiving objects", Stage::Receiving),
    ("Resolving deltas", Stage::Resolving),
    ("Updating files", Stage::Checkout),
    ("Checking out files", Stage::Checkout),
    ("Filtering content", Stage::Checkout),
];

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Stage {
    Connecting,
    Counting,
    Receiving,
    Resolving,
    Checkout,
}

#[derive(Clone)]
pub(super) struct Progress {
    pub(super) stage: Stage,
    pub(super) percent: Option<u8>,
    pub(super) detail: Option<String>,
}

pub(super) enum Update {
    Progress(Progress),
    Stalled,
}

pub(super) enum Outcome {
    Done,
    Failed(String),
    Cancelled,
}

impl Progress {
    fn parse(line: &str) -> Option<Self> {
        let (prefix, stage) = STAGES.iter().find(|(prefix, _)| line.starts_with(prefix))?;
        let values = line[prefix.len()..]
            .match_indices(':')
            .find_map(|(position, _)| counters(&line[prefix.len() + position + 1..]));
        let (percent, detail) = values.unwrap_or_default();
        Some(Self {
            stage: *stage,
            percent,
            detail,
        })
    }
}

fn counters(value: &str) -> Option<(Option<u8>, Option<String>)> {
    let value = after_space(value)?;
    let (percent, value) = digits(value)?;
    let value = after_space(value.strip_prefix('%')?)?.strip_prefix('(')?;
    let (_, value) = digits(value)?;
    let value = value.strip_prefix('/')?;
    let (_, value) = digits(value)?;
    let ending = value.strip_prefix(')')?.trim_end();
    let ending = ending.strip_suffix(", done.").unwrap_or(ending).trim_end();
    let detail = if ending.is_empty() {
        None
    } else {
        let detail = ending.strip_prefix(',')?.trim();
        (!detail.is_empty() && detail != "done.").then(|| detail.to_owned())
    };
    let percent = percent
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite());
    Some((percent.map(|value| value.min(100.0) as u8), detail))
}

fn digits(value: &str) -> Option<(&str, &str)> {
    let end = value.bytes().take_while(u8::is_ascii_digit).count();
    (end > 0).then(|| value.split_at(end))
}

fn after_space(value: &str) -> Option<&str> {
    let rest = value.trim_start();
    (rest.len() != value.len()).then_some(rest)
}

pub(super) fn public_url(input: &str) -> String {
    let Ok(mut url) = Url::parse(input) else {
        return input.to_owned();
    };
    if url.username().is_empty() && url.password().is_none() && url.query().is_none() {
        return input.to_owned();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.into()
}

fn command_url(input: &str) -> String {
    let Some((scheme, rest)) = input.split_once("://") else {
        return input.to_owned();
    };
    if let Some((host, query)) = rest.split_once('?')
        && !host.contains(['/', '#'])
    {
        return format!("{scheme}://{host}/?{query}");
    }
    input.to_owned()
}

pub(super) async fn run(
    remote: &str,
    destination: &Path,
    mut cancellation: watch::Receiver<bool>,
    mut on_update: impl FnMut(Update),
) -> Outcome {
    if *cancellation.borrow() {
        return Outcome::Cancelled;
    }
    let (Some(parent), Some(directory)) = (destination.parent(), destination.file_name()) else {
        return Outcome::Failed("Choose a destination path before cloning.".into());
    };
    let command_remote = command_url(remote);
    let Ok(mut child) = Command::new("git")
        .args(["clone", "--progress", "--", &command_remote])
        .arg(directory)
        .current_dir(parent)
        .env("GIT_PROGRESS_DELAY", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LANGUAGE", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    else {
        return Outcome::Failed("The repository could not be cloned.".into());
    };
    let mut stderr = child.stderr.take().expect("git stderr is piped");
    let mut bytes = [0; 8192];
    let mut output = CloneOutput::new(remote, &command_remote);
    let mut stall_at = Some(Instant::now() + STALL_AFTER);
    let mut lfs = interval(LFS_POLL);
    let mut last_partial_bytes = 0;
    loop {
        tokio::select! {
            _ = cancellation.changed() => {
                let _ = child.kill().await;
                return Outcome::Cancelled;
            }
            read = stderr.read(&mut bytes) => {
                let Ok(count) = read else { break };
                if count == 0 { break }
                output.push(&bytes[..count], |update| {
                    stall_at = Some(Instant::now() + STALL_AFTER);
                    on_update(Update::Progress(update));
                });
            }
            _ = sleep_until(stall_at.unwrap_or_else(Instant::now)), if stall_at.is_some() => {
                stall_at = None;
                on_update(Update::Stalled);
            }
            _ = lfs.tick() => {
                let partial_bytes = lfs_partial_bytes(destination).unwrap_or_default();
                if partial_bytes > 0 && partial_bytes != last_partial_bytes {
                    stall_at = Some(Instant::now() + STALL_AFTER);
                    on_update(Update::Progress(lfs_update(partial_bytes, output.filter_percent)));
                }
                last_partial_bytes = partial_bytes;
            }
        }
    }
    output.push(b"\n", |update| on_update(Update::Progress(update)));
    tokio::select! {
        status = child.wait() => {
            if status.is_ok_and(|status| status.success()) {
                Outcome::Done
            } else {
                Outcome::Failed(output.failure())
            }
        }
        _ = cancellation.changed() => {
            let _ = child.kill().await;
            Outcome::Cancelled
        }
    }
}

struct CloneOutput<'a> {
    remote: &'a str,
    command_remote: &'a str,
    public_remote: String,
    line: Vec<u8>,
    overflowed: bool,
    tail: VecDeque<String>,
    filter_percent: Option<u8>,
}

impl<'a> CloneOutput<'a> {
    fn new(remote: &'a str, command_remote: &'a str) -> Self {
        Self {
            remote,
            command_remote,
            public_remote: public_url(remote),
            line: Vec::new(),
            overflowed: false,
            tail: VecDeque::new(),
            filter_percent: None,
        }
    }

    fn push(&mut self, bytes: &[u8], mut progress: impl FnMut(Progress)) {
        for byte in bytes {
            if matches!(byte, b'\r' | b'\n') {
                if !self.overflowed && !self.line.is_empty() {
                    let raw = String::from_utf8_lossy(&self.line);
                    let line = raw.trim();
                    if let Some(mut update) = Progress::parse(line) {
                        if line.starts_with("Filtering content") {
                            self.filter_percent = update.percent;
                        }
                        if let Some(detail) = &mut update.detail {
                            *detail = self.redact(detail);
                        }
                        progress(update);
                    } else if !line.is_empty() && !line.starts_with("Cloning into") {
                        let detail = self.redact(line);
                        if self.tail.len() == 4 {
                            self.tail.pop_front();
                        }
                        self.tail.push_back(detail);
                    }
                }
                self.line.clear();
                self.overflowed = false;
            } else if self.line.len() < MAX_LINE_BYTES {
                self.line.push(*byte);
            } else {
                self.overflowed = true;
            }
        }
    }

    fn redact(&self, line: &str) -> String {
        let line = line.replace(self.command_remote, &self.public_remote);
        let line = line.replace(self.remote, &self.public_remote);
        let mut sanitized = String::with_capacity(line.len());
        for segment in line.split_inclusive(char::is_whitespace) {
            if let Some((scheme, rest)) = segment.split_once("://") {
                let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                let skip = rest[..authority_end]
                    .rfind('@')
                    .map_or(0, |index| index + 1);
                sanitized.push_str(scheme);
                sanitized.push_str("://");
                let rest = &rest[skip..];
                if let Some((path, _)) = rest.split_once('?') {
                    sanitized.push_str(path);
                    sanitized.push_str(&segment[segment.trim_end().len()..]);
                } else {
                    sanitized.push_str(rest);
                }
            } else {
                sanitized.push_str(segment);
            }
        }
        sanitized
    }

    fn failure(self) -> String {
        if self.tail.is_empty() {
            return "The repository could not be cloned.".into();
        }
        self.tail.into_iter().collect::<Vec<_>>().join(" ")
    }
}

fn lfs_partial_bytes(destination: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for file in destination.join(".git/lfs/incomplete").read_dir()? {
        total = total.saturating_add(file?.metadata()?.len());
    }
    Ok(total)
}

fn lfs_update(bytes: u64, percent: Option<u8>) -> Progress {
    let size = if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / 1024_f64.powi(3))
    } else {
        format!("{:.1} MiB", bytes as f64 / 1024_f64.powi(2))
    };
    Progress {
        stage: Stage::Checkout,
        percent,
        detail: Some(format!("Downloading Git LFS objects: {size} in progress")),
    }
}
