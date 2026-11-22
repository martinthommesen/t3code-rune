mod terminal_output;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::{Notify, broadcast};
use tokio::time::{Instant, sleep_until};

use self::terminal_output::{TAIL_CHARS, TerminalOutput};
use super::super::{ProjectionChange, turn_commands::TurnCommands};

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const OVERFLOW_INTERVAL: Duration = Duration::from_millis(500);
const MAX_APPEND_CHARS: usize = 16 * 1024;
const MAX_FINAL_SOURCE_CHARS: usize = TAIL_CHARS * 4;
const MAX_LIVE_COMMANDS: usize = 64;
const MAX_FINISHED_COMMANDS: usize = 16;

#[derive(Clone)]
pub(super) struct CommandOutputHub {
    commands: TurnCommands,
    changes: broadcast::Sender<ProjectionChange>,
    cache: Arc<Mutex<Cache>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Target {
    thread_id: String,
    item_id: String,
}

impl Target {
    fn new(thread_id: &str, item_id: &str) -> Self {
        Self {
            thread_id: thread_id.to_owned(),
            item_id: item_id.to_owned(),
        }
    }
}

#[derive(Default)]
struct Cache {
    live: VecDeque<(Target, TerminalOutput)>,
    finished: VecDeque<(Target, TerminalOutput)>,
    subscribers: HashMap<Target, Vec<Weak<Inbox>>>,
}

#[derive(Default)]
struct Inbox {
    pending: Mutex<PendingOutput>,
    wake: Notify,
}

#[derive(Default)]
struct PendingOutput {
    append: String,
    units: usize,
    replace: bool,
    overflowed: bool,
}

impl PendingOutput {
    fn reset(&mut self) {
        self.append.clear();
        self.units = 0;
    }

    fn invalidate(&mut self) {
        self.replace = true;
        self.reset();
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct CommandOutputFrame {
    kind: FrameKind,
    text: String,
    truncated: bool,
    running: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum FrameKind {
    Replace,
    Append,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct CommandOutputError {
    #[serde(rename = "_tag")]
    tag: &'static str,
    thread_id: String,
    message: &'static str,
}

impl CommandOutputError {
    fn new(target: &Target, message: &'static str) -> Self {
        Self {
            tag: "OrchestrationV2CommandOutputError",
            thread_id: target.thread_id.clone(),
            message,
        }
    }
}

pub(in crate::server) struct CommandOutputSubscription {
    target: Target,
    commands: TurnCommands,
    changes: broadcast::Receiver<ProjectionChange>,
    cache: Arc<Mutex<Cache>>,
    inbox: Arc<Inbox>,
    item: ObservedItem,
    needs_refresh: bool,
    has_update: bool,
    next_frame_at: Instant,
    finished: bool,
}

struct ObservedItem {
    running: bool,
    item: CommandItem,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandItem {
    output: Option<String>,
    native_item_ref: Option<NativeRef>,
    started_at: Option<String>,
    updated_at: String,
}

#[derive(Deserialize)]
struct NativeRef {
    driver: String,
}

impl CommandOutputHub {
    pub(super) fn new(
        commands: TurnCommands,
        changes: broadcast::Sender<ProjectionChange>,
    ) -> Self {
        Self {
            commands,
            changes,
            cache: Arc::new(Mutex::new(Cache::default())),
        }
    }

    pub(super) fn append(&self, thread_id: &str, item_id: &str, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        let target = Target::new(thread_id, item_id);
        let mut cache = self.cache.lock().expect("command output cache");
        let mut tail = remove_tail(&mut cache.live, &target).unwrap_or_default();
        tail.append(chunk);
        cache.live.push_back((target.clone(), tail));
        if cache.live.len() > MAX_LIVE_COMMANDS
            && let Some((oldest, _)) = cache.live.pop_front()
        {
            cache.each_inbox(&oldest, |inbox| {
                inbox
                    .pending
                    .lock()
                    .expect("command output inbox")
                    .invalidate();
            });
        }

        let units = utf16_len(chunk);
        cache.each_inbox(&target, |inbox| {
            let mut pending = inbox.pending.lock().expect("command output inbox");
            if !pending.replace {
                if pending.units + units > MAX_APPEND_CHARS {
                    pending.overflowed = true;
                    pending.invalidate();
                } else {
                    pending.append.push_str(chunk);
                    pending.units += units;
                }
            }
            inbox.wake.notify_one();
        });
    }

    pub(super) fn finish(&self, thread_id: &str, item_id: &str) {
        let target = Target::new(thread_id, item_id);
        let mut cache = self.cache.lock().expect("command output cache");
        if let Some(tail) = remove_tail(&mut cache.live, &target) {
            remove_tail(&mut cache.finished, &target);
            cache.finished.push_back((target, tail));
            if cache.finished.len() > MAX_FINISHED_COMMANDS {
                cache.finished.pop_front();
            }
        }
    }

    pub(super) fn subscribe(
        &self,
        thread_id: &str,
        item_id: &str,
    ) -> Result<CommandOutputSubscription, CommandOutputError> {
        let target = Target::new(thread_id, item_id);
        let changes = self.changes.subscribe();
        let inbox = Arc::new(Inbox {
            pending: Mutex::new(PendingOutput {
                replace: true,
                ..PendingOutput::default()
            }),
            wake: Notify::new(),
        });
        self.cache
            .lock()
            .expect("command output cache")
            .subscribers
            .entry(target.clone())
            .or_default()
            .push(Arc::downgrade(&inbox));
        let item = match observe(&self.commands, &target) {
            Ok(item) => item,
            Err(error) => {
                remove_inbox(&self.cache, &target, &inbox);
                return Err(error);
            }
        };
        Ok(CommandOutputSubscription {
            target,
            commands: self.commands.clone(),
            changes,
            cache: self.cache.clone(),
            inbox,
            item,
            needs_refresh: false,
            has_update: true,
            next_frame_at: Instant::now(),
            finished: false,
        })
    }
}

impl Cache {
    fn each_inbox(&self, target: &Target, mut apply: impl FnMut(&Inbox)) {
        if let Some(subscribers) = self.subscribers.get(target) {
            for inbox in subscribers.iter().filter_map(Weak::upgrade) {
                apply(&inbox);
            }
        }
    }

    fn tail(&self, target: &Target) -> Option<&TerminalOutput> {
        self.live
            .iter()
            .chain(&self.finished)
            .find(|(key, _)| key == target)
            .map(|(_, tail)| tail)
    }
}

fn remove_tail(
    tails: &mut VecDeque<(Target, TerminalOutput)>,
    target: &Target,
) -> Option<TerminalOutput> {
    let index = tails.iter().position(|(key, _)| key == target)?;
    tails.remove(index).map(|(_, tail)| tail)
}

fn remove_inbox(cache: &Mutex<Cache>, target: &Target, inbox: &Arc<Inbox>) {
    let mut cache = cache.lock().expect("command output cache");
    if let Some(subscribers) = cache.subscribers.get_mut(target) {
        let removed = Arc::downgrade(inbox);
        subscribers.retain(|candidate| !candidate.ptr_eq(&removed));
        if subscribers.is_empty() {
            cache.subscribers.remove(target);
        }
    }
}

fn observe(commands: &TurnCommands, target: &Target) -> Result<ObservedItem, CommandOutputError> {
    let (running, payload) = commands
        .command_output_item(&target.thread_id, &target.item_id)
        .map_err(|_| CommandOutputError::new(target, "Failed to read command output."))?
        .ok_or_else(|| CommandOutputError::new(target, "No such command."))?;
    let item = serde_json::from_str(&payload)
        .map_err(|_| CommandOutputError::new(target, "Failed to read command output."))?;
    Ok(ObservedItem { running, item })
}

impl Drop for CommandOutputSubscription {
    fn drop(&mut self) {
        remove_inbox(&self.cache, &self.target, &self.inbox);
    }
}

impl CommandOutputSubscription {
    pub(in crate::server) async fn recv(
        &mut self,
    ) -> Result<Option<CommandOutputFrame>, CommandOutputError> {
        while !self.finished {
            while !self.has_update {
                tokio::select! {
                    () = self.inbox.wake.notified() => self.has_update = true,
                    change = self.changes.recv() => self.observe_change(change)?,
                }
            }
            sleep_until(self.next_frame_at).await;
            for _ in 0..self.changes.len() {
                use broadcast::error::{RecvError, TryRecvError};
                match self.changes.try_recv() {
                    Ok(change) => self.observe_change(Ok(change))?,
                    Err(TryRecvError::Lagged(count)) => {
                        self.observe_change(Err(RecvError::Lagged(count)))?;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => self.observe_change(Err(RecvError::Closed))?,
                }
                if self.needs_refresh {
                    break;
                }
            }
            if self.needs_refresh {
                let commands = self.commands.clone();
                let target = self.target.clone();
                let mut observed = tokio::task::spawn_blocking(move || observe(&commands, &target))
                    .await
                    .map_err(|_| {
                        CommandOutputError::new(&self.target, "Failed to read command output.")
                    })??;
                self.needs_refresh = false;
                if !observed.running
                    || (observed.item.output.is_some()
                        && self.item.item.output != observed.item.output)
                {
                    self.inbox
                        .pending
                        .lock()
                        .expect("command output inbox")
                        .invalidate();
                }
                if observed.running && observed.item.output.is_none() {
                    observed.item.output = self.item.item.output.take();
                }
                self.item = observed;
            }
            self.has_update = false;
            if let Some((frame, delay)) = self.read_frame() {
                self.next_frame_at = Instant::now() + delay;
                self.finished = !frame.running;
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    fn observe_change(
        &mut self,
        change: Result<ProjectionChange, broadcast::error::RecvError>,
    ) -> Result<(), CommandOutputError> {
        match change {
            Ok(change) => {
                if matches!(change, ProjectionChange::Thread(id) if id == self.target.thread_id) {
                    self.needs_refresh = true;
                    self.has_update = true;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                self.needs_refresh = true;
                self.has_update = true;
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Err(CommandOutputError::new(
                    &self.target,
                    "Lost the thread's event stream.",
                ));
            }
        }
        Ok(())
    }

    fn read_frame(&self) -> Option<(CommandOutputFrame, Duration)> {
        let cache = self.cache.lock().expect("command output cache");
        let tail = cache.tail(&self.target);
        let mut pending = self.inbox.pending.lock().expect("command output inbox");
        let running = self.item.running;
        if !running || pending.replace {
            let delay = if running && pending.overflowed {
                OVERFLOW_INTERVAL
            } else {
                FRAME_INTERVAL
            };
            *pending = PendingOutput::default();
            let cached = match tail {
                Some(tail) if running => Some((tail.resume_text(), tail.truncated())),
                Some(tail) if self.item.item.output.is_none() => {
                    Some((tail.text().to_owned(), tail.truncated()))
                }
                _ => None,
            };
            drop(pending);
            drop(cache);
            let (text, truncated) = cached.unwrap_or_else(|| {
                let item = &self.item.item;
                if running {
                    persisted_tail(item.output.as_deref())
                } else {
                    persisted_tail(item.display_output().as_deref())
                }
            });
            return Some((
                CommandOutputFrame {
                    kind: FrameKind::Replace,
                    text,
                    truncated,
                    running,
                },
                delay,
            ));
        }
        let tail = tail?;
        if pending.append.is_empty() {
            return None;
        }
        pending.units = 0;
        Some((
            CommandOutputFrame {
                kind: FrameKind::Append,
                text: std::mem::take(&mut pending.append),
                truncated: tail.truncated(),
                running: true,
            },
            FRAME_INTERVAL,
        ))
    }
}

impl CommandItem {
    fn display_output(&self) -> Option<std::borrow::Cow<'_, str>> {
        let output = self.output.as_deref()?;
        let cutoff = OffsetDateTime::parse("2026-10-08T00:00:00Z", &Rfc3339)
            .expect("valid Claude output cutoff");
        if self
            .native_item_ref
            .as_ref()
            .is_some_and(|native| native.driver == "claudeAgent")
            && OffsetDateTime::parse(
                self.started_at.as_deref().unwrap_or(&self.updated_at),
                &Rfc3339,
            )
            .is_ok_and(|date| date < cutoff)
            && output
                .trim_start_matches(js_whitespace)
                .starts_with("{\"stdout\"")
            && let Ok(BashEnvelope { stdout, stderr, .. }) = serde_json::from_str(output)
        {
            let stdout = nonblank(&stdout);
            let stderr = nonblank(&stderr);
            let separator = if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            return Some(format!("{stdout}{separator}{stderr}").into());
        }
        Some(output.into())
    }
}

#[derive(Deserialize)]
struct BashEnvelope {
    stdout: String,
    stderr: String,
    #[serde(rename = "interrupted")]
    _interrupted: bool,
}

fn nonblank(text: &str) -> &str {
    if text.trim_matches(js_whitespace).is_empty() {
        ""
    } else {
        text
    }
}

fn js_whitespace(character: char) -> bool {
    character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
}

fn persisted_tail(output: Option<&str>) -> (String, bool) {
    let Some(output) = output else {
        return (String::new(), false);
    };
    let source = if let Some(cut) = suffix_start(output, MAX_FINAL_SOURCE_CHARS) {
        let suffix = &output[cut..];
        suffix
            .split_once('\n')
            .map(|(_, rest)| rest)
            .filter(|rest| !rest.is_empty())
            .unwrap_or(suffix)
    } else {
        output
    };
    let mut terminal = TerminalOutput::default();
    terminal.append(source);
    (
        terminal.text().to_owned(),
        terminal.truncated() || source.len() != output.len(),
    )
}

fn utf16_len(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }
}

fn suffix_start(text: &str, limit: usize) -> Option<usize> {
    let mut remaining = limit;
    for (offset, character) in text.char_indices().rev() {
        let len = character.len_utf16();
        if len > remaining {
            return Some(offset + character.len_utf8());
        }
        remaining -= len;
    }
    None
}
