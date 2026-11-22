use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Event, EventSender, REQUEST_TIMEOUT, RpcCause, RpcExit, Session, SessionStop};

const MAX_HISTORY_BYTES: usize = 512 * 1024;
const MAX_WRITE_UNITS: usize = 65_536;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalTarget {
    pub(crate) thread_id: String,
    pub(crate) terminal_id: String,
}

#[derive(Clone)]
pub(crate) struct TerminalAttach {
    pub(crate) target: TerminalTarget,
    pub(crate) cwd: Option<String>,
    pub(crate) project_root: String,
    pub(crate) worktree_path: Option<String>,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) resume: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TerminalStatus {
    Starting,
    Running,
    Exited,
    Error,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalSnapshot {
    #[serde(flatten)]
    target: TerminalTarget,
    sequence: Option<u64>,
    pub(crate) cwd: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) history: String,
    pub(crate) label: String,
    pub(crate) status: TerminalStatus,
    pub(crate) cols: Option<u16>,
    pub(crate) rows: Option<u16>,
    pub(crate) error: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) exit_signal: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum TerminalUpdate {
    Snapshot {
        snapshot: TerminalSnapshot,
    },
    Restarted {
        snapshot: TerminalSnapshot,
    },
    Output {
        data: String,
    },
    Activity {
        cols: Option<u16>,
        rows: Option<u16>,
        label: String,
        has_running_subprocess: bool,
    },
    Exited {
        exit_code: Option<i64>,
        exit_signal: Option<i32>,
    },
    Cleared,
    Closed,
    Error {
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireUpdate {
    thread_id: Option<String>,
    terminal_id: Option<String>,
    sequence: Option<u64>,
    #[serde(flatten)]
    update: TerminalUpdate,
}

pub(super) enum Command {
    Attach(Intent),
    Detach(u64),
    Write {
        attachment: u64,
        data: String,
        binary: bool,
        reply: bool,
    },
    WriteIdle {
        request: Uuid,
        session: Uuid,
        thread_id: String,
        terminal_ids: Vec<String>,
        data: String,
    },
    Resize {
        attachment: u64,
        cols: u16,
        rows: u16,
    },
    Close {
        target: TerminalTarget,
        delete_history: bool,
    },
}

#[derive(Clone)]
pub(super) struct Intent {
    pub(super) attachment: u64,
    pub(super) attach: TerminalAttach,
    resume: bool,
}

#[derive(Default)]
pub(super) struct Selection {
    current: Mutex<HashMap<u64, TerminalTarget>>,
    write_session: Mutex<Option<Uuid>>,
}

impl Selection {
    pub(super) fn write_session(&self) -> Option<Uuid> {
        *self.write_session.lock().expect("terminal write session")
    }

    pub(super) fn set_write_session(&self, session: Option<Uuid>) {
        *self.write_session.lock().expect("terminal write session") = session;
    }

    pub(super) fn attach(&self, intent: &Intent) {
        let mut current = self.current.lock().expect("terminal selection lock");
        current.retain(|_, target| target != &intent.attach.target);
        current.insert(intent.attachment, intent.attach.target.clone());
    }

    pub(super) fn detach(&self, attachment: u64) {
        self.current
            .lock()
            .expect("terminal selection lock")
            .remove(&attachment);
    }

    pub(super) fn keep_thread(&self, thread: Option<&str>) {
        self.current
            .lock()
            .expect("terminal selection lock")
            .retain(|_, target| Some(target.thread_id.as_str()) == thread);
    }

    pub(super) fn includes(&self, attachment: u64) -> bool {
        self.current
            .lock()
            .expect("terminal selection lock")
            .contains_key(&attachment)
    }
}

impl Intent {
    pub(super) fn new(attachment: u64, attach: TerminalAttach) -> Self {
        Self {
            attachment,
            resume: attach.resume,
            attach,
        }
    }

    pub(super) fn payload(&self) -> Value {
        let mut payload = json!({
            "threadId": self.attach.target.thread_id,
            "terminalId": self.attach.target.terminal_id,
            "worktreePath": self.attach.worktree_path,
            "cols": self.attach.cols,
            "rows": self.attach.rows,
            "restartIfNotRunning": !self.resume,
            "maxHistoryBytes": MAX_HISTORY_BYTES,
        });
        if let Some(cwd) = &self.attach.cwd {
            payload["cwd"] = json!(cwd);
            let mut env = json!({"T3CODE_PROJECT_ROOT": self.attach.project_root});
            if let Some(worktree) = &self.attach.worktree_path {
                env["T3CODE_WORKTREE_PATH"] = json!(worktree);
            }
            payload["env"] = env;
        }
        if self.resume {
            payload["resume"] = json!(true);
        }
        payload
    }
}

enum Phase {
    Waiting {
        since: Instant,
        requested_grid: (u16, u16),
    },
    Ready {
        sequence: Option<u64>,
        status: TerminalStatus,
    },
}

pub(super) struct Subscription {
    pub(super) request_id: String,
    pub(super) intent: Intent,
    phase: Phase,
    closing: bool,
}

impl Subscription {
    pub(super) fn new(request_id: String, intent: Intent) -> Self {
        let requested_grid = (intent.attach.cols, intent.attach.rows);
        Self {
            request_id,
            intent,
            phase: Phase::Waiting {
                since: Instant::now(),
                requested_grid,
            },
            closing: false,
        }
    }

    pub(super) fn timed_out(&self, timeout: std::time::Duration) -> bool {
        matches!(self.phase, Phase::Waiting { since, .. } if since.elapsed() >= timeout)
    }

    pub(super) fn accepts_input(&self) -> bool {
        !self.closing
            && matches!(
                self.phase,
                Phase::Ready {
                    status: TerminalStatus::Running,
                    ..
                }
            )
    }

    pub(super) fn resize(&mut self, cols: u16, rows: u16) -> Option<Value> {
        if (cols, rows) == (self.intent.attach.cols, self.intent.attach.rows) {
            return None;
        }
        self.intent.attach.cols = cols;
        self.intent.attach.rows = rows;
        self.accepts_input().then(|| self.resize_payload())
    }

    pub(super) fn resize_payload(&self) -> Value {
        json!({
            "threadId": self.intent.attach.target.thread_id,
            "terminalId": self.intent.attach.target.terminal_id,
            "cols": self.intent.attach.cols,
            "rows": self.intent.attach.rows,
        })
    }

    pub(super) fn inputs(&self, data: &str, binary: bool, reply: bool) -> Vec<Value> {
        let mut inputs = Vec::new();
        let mut start = 0;
        let mut units = 0;
        for (index, character) in data.char_indices() {
            if units + character.len_utf16() > MAX_WRITE_UNITS {
                inputs.push(self.write_payload(&data[start..index], binary, reply));
                start = index;
                units = 0;
            }
            units += character.len_utf16();
        }
        if start < data.len() {
            inputs.push(self.write_payload(&data[start..], binary, reply));
        }
        inputs
    }

    fn write_payload(&self, data: &str, binary: bool, reply: bool) -> Value {
        json!({
            "threadId": self.intent.attach.target.thread_id,
            "terminalId": self.intent.attach.target.terminal_id,
            "data": data,
            "binary": binary,
            "reply": reply,
        })
    }

    pub(super) fn receive(
        &mut self,
        value: Value,
    ) -> Result<Option<(TerminalUpdate, bool)>, String> {
        let frame: WireUpdate = serde_json::from_value(value)
            .map_err(|error| format!("The environment returned invalid terminal data: {error}"))?;
        let snapshot = match &frame.update {
            TerminalUpdate::Snapshot { snapshot } | TerminalUpdate::Restarted { snapshot } => {
                Some(snapshot)
            }
            _ => None,
        };
        let target = &self.intent.attach.target;
        let outer_target = frame.thread_id.as_deref().zip(frame.terminal_id.as_deref());
        if snapshot.is_some_and(|snapshot| &snapshot.target != target)
            || frame
                .thread_id
                .as_deref()
                .is_some_and(|thread| thread != target.thread_id)
            || frame
                .terminal_id
                .as_deref()
                .is_some_and(|terminal| terminal != target.terminal_id)
            || (outer_target.is_none() && snapshot.is_none())
        {
            return Err("The environment returned output for a different terminal.".to_owned());
        }

        let sequence = frame
            .sequence
            .or_else(|| snapshot.and_then(|snapshot| snapshot.sequence));
        let resets = matches!(frame.update, TerminalUpdate::Snapshot { .. });
        let resize = match self.phase {
            Phase::Waiting { requested_grid, .. } => {
                let Some(snapshot) = snapshot else {
                    return if matches!(frame.update, TerminalUpdate::Closed) {
                        Ok(Some((frame.update, false)))
                    } else {
                        Err("The environment sent terminal output before its snapshot.".to_owned())
                    };
                };
                self.intent.resume = true;
                self.phase = Phase::Ready {
                    sequence,
                    status: snapshot.status,
                };
                self.accepts_input()
                    && requested_grid != (self.intent.attach.cols, self.intent.attach.rows)
            }
            Phase::Ready {
                sequence: previous,
                status,
            } => {
                if !resets
                    && previous
                        .zip(sequence)
                        .is_some_and(|(previous, next)| next <= previous)
                {
                    return Ok(None);
                }
                let status = match &frame.update {
                    TerminalUpdate::Exited { .. } | TerminalUpdate::Closed => {
                        TerminalStatus::Exited
                    }
                    TerminalUpdate::Error { .. } => TerminalStatus::Error,
                    _ => snapshot.map_or(status, |snapshot| snapshot.status),
                };
                self.phase = Phase::Ready {
                    sequence: if resets {
                        sequence
                    } else {
                        sequence.or(previous)
                    },
                    status,
                };
                false
            }
        };
        Ok(Some((frame.update, resize)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Write,
    Reply,
    Resize,
}

impl Action {
    fn method(self) -> &'static str {
        match self {
            Self::Write | Self::Reply => "terminal.write",
            Self::Resize => "terminal.resize",
        }
    }

    fn unconfirmed(self) -> &'static str {
        match self {
            Self::Write | Self::Reply => "Terminal input delivery could not be confirmed.",
            Self::Resize => "The environment did not confirm the terminal size.",
        }
    }
}

pub(super) struct PendingOperation {
    kind: Operation,
    started: Instant,
}

enum Operation {
    Active {
        attachment: u64,
        action: Action,
    },
    IdleWrite {
        request: Uuid,
        terminal_ids: Vec<String>,
    },
    Close {
        target: TerminalTarget,
        attachment: Option<u64>,
    },
}

impl Operation {
    fn is_for_attachment(&self, id: u64) -> bool {
        matches!(self, Self::Active { attachment, .. } if *attachment == id)
    }
}

const CLOSE_UNCONFIRMED: &str = "The environment did not confirm that the terminal closed.";
const IDLE_WRITE_UNCONFIRMED: &str =
    "The environment did not confirm that the project script was sent.";

impl Session {
    pub(super) async fn start_terminal(&mut self, intent: Intent) -> Result<(), SessionStop> {
        if let Some(replaced) = self.terminals.values().find(|terminal| {
            terminal.intent.attachment == intent.attachment
                || terminal.intent.attach.target == intent.attach.target
        }) {
            self.drop_terminal(replaced.intent.attachment).await?;
        }
        let request = self.allocate_id();
        let payload = intent.payload();
        self.terminals.insert(
            intent.attachment,
            Subscription::new(request.clone(), intent),
        );
        self.send_request(&request, "terminal.attach", payload)
            .await
    }

    async fn drop_terminal(&mut self, attachment: u64) -> Result<(), SessionStop> {
        if let Some(terminal) = self.terminals.remove(&attachment) {
            self.pending_terminal
                .retain(|_, pending| !pending.kind.is_for_attachment(attachment));
            self.interrupt(&terminal.request_id).await?;
        }
        Ok(())
    }

    pub(super) async fn drop_terminals(&mut self) -> Result<(), SessionStop> {
        let attachments: Vec<_> = self.terminals.keys().copied().collect();
        for attachment in attachments {
            self.drop_terminal(attachment).await?;
        }
        Ok(())
    }

    pub(super) async fn terminal_command(
        &mut self,
        command: Command,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if let Command::Close {
            target,
            delete_history,
        } = command
        {
            return self
                .issue_terminal_close(target, delete_history, events)
                .await;
        }
        if let Command::Attach(intent) = command {
            if !self.terminals_enabled {
                terminal_error(
                    events,
                    intent.attachment,
                    "This environment does not allow terminal access.",
                );
            } else if !self
                .thread
                .as_ref()
                .is_some_and(|thread| thread.thread_id == intent.attach.target.thread_id)
            {
                terminal_error(
                    events,
                    intent.attachment,
                    "Open the thread before attaching its terminal.",
                );
            } else {
                self.start_terminal(intent).await?;
            }
            return Ok(());
        }
        match command {
            Command::Detach(attachment) => self.drop_terminal(attachment).await?,
            Command::Write {
                attachment,
                data,
                binary,
                reply,
            } => {
                let Some(terminal) = self.terminals.get(&attachment) else {
                    return Ok(());
                };
                let action = if reply { Action::Reply } else { Action::Write };
                if terminal.accepts_input() {
                    let inputs = terminal.inputs(&data, binary, reply);
                    for payload in inputs {
                        self.issue_terminal_request(attachment, action, payload)
                            .await?;
                    }
                } else if !reply && !data.is_empty() {
                    terminal_error(events, attachment, "The terminal is not ready for input.");
                }
            }
            Command::WriteIdle {
                request,
                session,
                thread_id,
                terminal_ids,
                data,
            } => {
                if session != self.terminal_write_session {
                    idle_write_settled(events, request, Err(IDLE_WRITE_UNCONFIRMED.to_owned()));
                } else if !self.terminals_enabled
                    || !self
                        .thread
                        .as_ref()
                        .is_some_and(|thread| thread.thread_id == thread_id)
                {
                    idle_write_settled(
                        events,
                        request,
                        Err("This thread terminal is unavailable.".to_owned()),
                    );
                } else if !self.terminal_idle_write || !idle_write_fits(&data) {
                    idle_write_settled(events, request, Ok(None));
                } else {
                    let wire_id = self.allocate_id();
                    let payload =
                        json!({"threadId": thread_id, "terminalIds": terminal_ids, "data": data});
                    self.pending_terminal.insert(
                        wire_id.clone(),
                        PendingOperation {
                            kind: Operation::IdleWrite {
                                request,
                                terminal_ids,
                            },
                            started: Instant::now(),
                        },
                    );
                    self.send_request(&wire_id, "t3.terminalWriteIdle", payload)
                        .await?;
                }
            }
            Command::Resize {
                attachment,
                cols,
                rows,
            } => {
                if let Some(payload) = self
                    .terminals
                    .get_mut(&attachment)
                    .and_then(|terminal| terminal.resize(cols, rows))
                {
                    self.issue_terminal_request(attachment, Action::Resize, payload)
                        .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn issue_terminal_request(
        &mut self,
        attachment: u64,
        action: Action,
        payload: Value,
    ) -> Result<(), SessionStop> {
        let request = self.allocate_id();
        if action != Action::Reply {
            self.pending_terminal.insert(
                request.clone(),
                PendingOperation {
                    kind: Operation::Active { attachment, action },
                    started: Instant::now(),
                },
            );
        }
        self.send_request(&request, action.method(), payload).await
    }

    fn allows_terminal_target(&self, target: &TerminalTarget) -> bool {
        self.terminals_enabled
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| thread.thread_id == target.thread_id)
    }

    async fn issue_terminal_close(
        &mut self,
        target: TerminalTarget,
        delete_history: bool,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if !self.allows_terminal_target(&target) {
            let message = if self.terminals_enabled {
                "Open the thread before closing its terminal."
            } else {
                "This environment does not allow terminal access."
            };
            terminal_close_settled(events, target, Err(message.to_owned()));
            return Ok(());
        }
        if self.pending_terminal.values().any(|pending| matches!(&pending.kind, Operation::Close { target: closing, .. } if closing == &target)) {
            return Ok(());
        }
        let attachment = self
            .terminals
            .values_mut()
            .find(|terminal| terminal.intent.attach.target == target)
            .map(|terminal| {
                terminal.closing = true;
                terminal.intent.attachment
            });
        let request = self.allocate_id();
        let payload = json!({
            "threadId": target.thread_id,
            "terminalId": target.terminal_id,
            "deleteHistory": delete_history,
        });
        self.pending_terminal.insert(
            request.clone(),
            PendingOperation {
                kind: Operation::Close { target, attachment },
                started: Instant::now(),
            },
        );
        self.send_request(&request, "terminal.close", payload).await
    }

    pub(super) async fn terminal_chunk(
        &mut self,
        attachment: u64,
        values: Vec<Value>,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        for value in values {
            let Some(terminal) = self.terminals.get_mut(&attachment) else {
                break;
            };
            match terminal.receive(value) {
                Ok(Some((update, resize))) => {
                    let closed = matches!(update, TerminalUpdate::Closed);
                    events.send(Event::Terminal { attachment, update });
                    if closed {
                        self.drop_terminal(attachment).await?;
                        break;
                    }
                    if resize {
                        let payload = terminal.resize_payload();
                        self.issue_terminal_request(attachment, Action::Resize, payload)
                            .await?;
                    }
                }
                Ok(None) => {}
                Err(message) => {
                    terminal_error(events, attachment, &message);
                    self.drop_terminal(attachment).await?;
                    break;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn terminal_exit(
        &mut self,
        request_id: &str,
        exit: RpcExit,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if let Some(pending) = self.pending_terminal.remove(request_id) {
            match pending.kind {
                Operation::Close { target, attachment } => {
                    self.finish_terminal_close(target, attachment, exit, events)
                        .await?;
                }
                Operation::IdleWrite {
                    request,
                    terminal_ids,
                } => {
                    let result =
                        exit.into_result()
                            .and_then(|value| match value.get("terminalId") {
                                Some(Value::Null) => Ok(None),
                                Some(Value::String(id)) if terminal_ids.contains(id) => {
                                    Ok(Some(id.to_owned()))
                                }
                                _ => Err(IDLE_WRITE_UNCONFIRMED.to_owned()),
                            });
                    idle_write_settled(events, request, result);
                }
                Operation::Active { attachment, .. } => {
                    if let Err(message) = exit.into_result() {
                        terminal_error(events, attachment, &message);
                    }
                }
            }
        } else if let Some(attachment) = self.terminal_attachment(request_id) {
            self.terminals.remove(&attachment);
            let message = exit
                .into_result()
                .err()
                .unwrap_or_else(|| "The terminal stream ended.".to_owned());
            self.pending_terminal
                .retain(|_, operation| !operation.kind.is_for_attachment(attachment));
            terminal_error(events, attachment, &message);
        }
        Ok(())
    }

    async fn finish_terminal_close(
        &mut self,
        target: TerminalTarget,
        attachment: Option<u64>,
        exit: RpcExit,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let can_fallback = matches!(&exit, RpcExit::Failure { cause } if cause.iter().any(|cause| matches!(cause, RpcCause::Fail { .. })));
        let result = exit.into_result().map(|_| ());
        let terminal = attachment.and_then(|attachment| self.terminals.get_mut(&attachment));
        let closed = attachment.filter(|_| result.is_ok() && terminal.is_some());
        if let Some(terminal) = terminal {
            let attachment = terminal.intent.attachment;
            match &result {
                Err(message) => {
                    terminal.closing = false;
                    terminal_error(events, attachment, message);
                }
                Ok(()) => events.send(Event::Terminal {
                    attachment,
                    update: TerminalUpdate::Closed,
                }),
            }
        }
        terminal_close_settled(events, target.clone(), result);
        if let Some(attachment) = closed {
            self.drop_terminal(attachment).await?;
        } else if can_fallback && self.allows_terminal_target(&target) {
            let request = self.allocate_id();
            self.send_request(
                &request,
                "terminal.write",
                json!({
                    "threadId": target.thread_id,
                    "terminalId": target.terminal_id,
                    "data": "exit\n",
                }),
            )
            .await?;
        }
        Ok(())
    }

    pub(super) fn terminal_attachment(&self, request_id: &str) -> Option<u64> {
        self.terminals.iter().find_map(|(&attachment, terminal)| {
            (terminal.request_id == request_id).then_some(attachment)
        })
    }

    pub(super) async fn expire_terminal(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let expired: Vec<_> = self
            .terminals
            .values()
            .filter(|terminal| terminal.timed_out(REQUEST_TIMEOUT))
            .map(|terminal| terminal.intent.attachment)
            .collect();
        for attachment in expired {
            terminal_error(
                events,
                attachment,
                "The environment did not attach the terminal in time.",
            );
            self.drop_terminal(attachment).await?;
        }
        let active = &mut self.terminals;
        self.pending_terminal.retain(|_, operation| {
            if operation.started.elapsed() < REQUEST_TIMEOUT {
                return true;
            }
            match &operation.kind {
                Operation::Active { attachment, action } => {
                    terminal_error(events, *attachment, action.unconfirmed())
                }
                Operation::IdleWrite { request, .. } => {
                    idle_write_settled(events, *request, Err(IDLE_WRITE_UNCONFIRMED.to_owned()));
                }
                Operation::Close { target, attachment } => {
                    terminal_close_settled(
                        events,
                        target.clone(),
                        Err(CLOSE_UNCONFIRMED.to_owned()),
                    );
                    if let Some(terminal) = attachment.and_then(|id| active.get_mut(&id)) {
                        terminal.closing = false;
                        terminal_error(events, terminal.intent.attachment, CLOSE_UNCONFIRMED);
                    }
                }
            }
            false
        });
        Ok(())
    }

    pub(super) fn terminal_after_disconnect(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Vec<Intent> {
        let operations = std::mem::take(&mut self.pending_terminal);
        for operation in operations.values() {
            match &operation.kind {
                Operation::IdleWrite { request, .. } => {
                    idle_write_settled(events, *request, Err(IDLE_WRITE_UNCONFIRMED.to_owned()));
                }
                Operation::Close { target, .. } => {
                    terminal_close_settled(
                        events,
                        target.clone(),
                        Err(CLOSE_UNCONFIRMED.to_owned()),
                    );
                }
                Operation::Active { .. } => {}
            }
        }
        std::mem::take(&mut self.terminals)
            .into_values()
            .filter_map(|terminal| {
                let attachment_id = terminal.intent.attachment;
                if operations.values().any(|operation| {
                    matches!(operation.kind, Operation::Active { attachment, action: Action::Write }
                        if attachment == attachment_id)
                }) {
                    terminal_error(events, attachment_id, Action::Write.unconfirmed());
                }
                if terminal.closing {
                    terminal_error(events, attachment_id, CLOSE_UNCONFIRMED);
                }
                (!terminal.closing).then_some(terminal.intent)
            })
            .collect()
    }
}

fn idle_write_fits(data: &str) -> bool {
    data.encode_utf16().take(MAX_WRITE_UNITS + 1).count() <= MAX_WRITE_UNITS
}

fn idle_write_settled(
    events: &EventSender<impl Fn()>,
    request: Uuid,
    result: Result<Option<String>, String>,
) {
    events.send(Event::TerminalIdleWriteSettled { request, result });
}

fn terminal_close_settled(
    events: &EventSender<impl Fn()>,
    target: TerminalTarget,
    result: Result<(), String>,
) {
    events.send(Event::TerminalCloseSettled { target, result });
}

fn terminal_error(events: &EventSender<impl Fn()>, attachment: u64, message: &str) {
    events.send(Event::TerminalFailed {
        attachment,
        message: message.to_owned(),
    });
}

#[cfg(test)]
mod tests;
