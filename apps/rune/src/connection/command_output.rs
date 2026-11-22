use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::projection::CommandOutputTarget;
use super::{Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop};
use crate::server::TerminalOutput;

const RETAIN_COMPLETED: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandInspection {
    pub(crate) owner_thread_id: String,
    pub(crate) target: CommandOutputTarget,
    pub(crate) attachment: Uuid,
    pub(crate) fallback_revision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandOutput {
    Pending,
    Inline,
    Output {
        text: String,
        truncated: bool,
        running: bool,
    },
    Failed(String),
    Missing,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Frame {
    Replace {
        text: String,
        truncated: bool,
        running: bool,
    },
    Append {
        text: String,
        truncated: bool,
        running: bool,
    },
}

impl CommandOutput {
    fn apply(&mut self, terminal: &mut TerminalOutput, frame: Frame) -> Result<(), &'static str> {
        let (text, truncated, running) = match frame {
            Frame::Replace {
                text,
                truncated,
                running,
            } => {
                *terminal = TerminalOutput::default();
                (text, truncated, running)
            }
            Frame::Append {
                text,
                truncated,
                running,
            } => {
                let Self::Output {
                    truncated: before, ..
                } = self
                else {
                    return Err("The command output stream did not start with a snapshot.");
                };
                (text, *before || truncated, running)
            }
        };
        terminal.append(&text);
        *self = Self::Output {
            text: terminal.text().into(),
            truncated: truncated || terminal.truncated(),
            running,
        };
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Method {
    Stream,
    Detail,
    Inline,
}

pub(super) enum Command {
    Open(CommandInspection),
    Close(Uuid),
}

#[derive(Default)]
pub(super) struct Subscriptions {
    active: HashMap<Uuid, Subscription>,
    cached: HashMap<(CommandOutputTarget, Option<String>), (Instant, CommandOutput)>,
}

struct Subscription {
    intent: CommandInspection,
    method: Method,
    request_id: Option<String>,
    output: CommandOutput,
    terminal: TerminalOutput,
    opened_at: Instant,
}

impl Subscriptions {
    pub(super) fn inspections(&self) -> Vec<CommandInspection> {
        self.active
            .values()
            .map(|subscription| subscription.intent.clone())
            .collect()
    }

    pub(super) fn matches(&self, request_id: &str) -> bool {
        self.active
            .values()
            .any(|subscription| subscription.request_id.as_deref() == Some(request_id))
    }

    pub(super) fn chunk(
        &mut self,
        request_id: &str,
        values: Vec<Value>,
        events: &EventSender<impl Fn()>,
    ) {
        let Some(subscription) = self.request(request_id) else {
            return;
        };
        for value in values {
            let result = serde_json::from_value::<Frame>(value)
                .map_err(|_| "The environment returned invalid command output.")
                .and_then(|frame| subscription.output.apply(&mut subscription.terminal, frame));
            if let Err(error) = result {
                subscription.output = CommandOutput::Failed(error.into());
                break;
            }
        }
        subscription.publish(events);
    }

    pub(super) fn exit(
        &mut self,
        request_id: &str,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        let Some(subscription) = self.request(request_id) else {
            return;
        };
        subscription.request_id = None;
        match result {
            Ok(value) if subscription.method == Method::Detail => {
                subscription.output = detail_output(value, &subscription.intent.target);
            }
            Ok(_) if matches!(subscription.output, CommandOutput::Pending) => {
                subscription.output = CommandOutput::Missing;
            }
            Err(message) if !matches!(&subscription.output, CommandOutput::Output { text, .. } if !text.is_empty()) => {
                subscription.output = CommandOutput::Failed(message)
            }
            Ok(_) | Err(_) => {}
        }
        subscription.publish(events);
    }

    fn request(&mut self, id: &str) -> Option<&mut Subscription> {
        self.active
            .values_mut()
            .find(|subscription| subscription.request_id.as_deref() == Some(id))
    }

    fn remove(&mut self, id: Uuid) -> Option<String> {
        let subscription = self.active.remove(&id)?;
        if matches!(
            subscription.output,
            CommandOutput::Output { running: false, .. }
        ) {
            let revision = (subscription.method == Method::Detail)
                .then_some(subscription.intent.fallback_revision)
                .flatten();
            self.cached.insert(
                (subscription.intent.target, revision),
                (Instant::now(), subscription.output),
            );
        }
        subscription.request_id
    }
}

impl Subscription {
    fn publish(&self, events: &EventSender<impl Fn()>) {
        events.send(Event::CommandOutput {
            thread_id: self.intent.owner_thread_id.clone(),
            attachment: self.intent.attachment,
            output: self.output.clone(),
        });
    }
}

fn detail_output(value: Value, target: &CommandOutputTarget) -> CommandOutput {
    use super::projection::CommandEntry;

    if value.get("item").is_some_and(Value::is_null) {
        return CommandOutput::Missing;
    }
    let Some(item) = value
        .get("item")
        .filter(|item| item["type"] == "command_execution")
    else {
        return CommandOutput::Failed("The environment returned invalid command details.".into());
    };
    let Ok(item) = CommandEntry::deserialize(item) else {
        return CommandOutput::Failed("The environment returned invalid command details.".into());
    };
    if item.target() != *target {
        return CommandOutput::Failed("The environment returned a different command.".into());
    }
    let mut terminal = TerminalOutput::default();
    terminal.append(item.output.as_deref().unwrap_or_default());
    CommandOutput::Output {
        text: terminal.text().into(),
        truncated: terminal.truncated(),
        running: item.status.is_active(),
    }
}

impl Connection {
    pub(crate) fn inspect_command_output(&self, intent: CommandInspection) {
        let _ = self
            .commands
            .send(super::Command::ObserveOutput(Command::Open(intent)));
    }

    pub(crate) fn close_command_output(&self, attachment: Uuid) {
        let _ = self
            .commands
            .send(super::Command::ObserveOutput(Command::Close(attachment)));
    }
}

impl Session {
    pub(super) async fn command_output(
        &mut self,
        command: Command,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let intent = match command {
            Command::Open(intent) => intent,
            Command::Close(attachment) => return self.close_command_output(attachment).await,
        };
        if self
            .thread
            .as_ref()
            .is_none_or(|thread| thread.thread_id != intent.owner_thread_id)
        {
            return Ok(());
        }
        let method = if self.command_output_streaming {
            Method::Stream
        } else if intent.fallback_revision.is_some() {
            Method::Detail
        } else {
            Method::Inline
        };
        if self
            .command_outputs
            .active
            .get(&intent.attachment)
            .is_some_and(|existing| {
                existing.intent.target == intent.target
                    && existing.method == method
                    && (method != Method::Detail
                        || existing.intent.fallback_revision == intent.fallback_revision)
            })
        {
            return Ok(());
        }
        self.close_command_output(intent.attachment).await?;
        let rpc = match method {
            Method::Stream => Some("orchestration.subscribeCommandOutput"),
            Method::Detail => Some("orchestration.getTurnItem"),
            Method::Inline => None,
        };
        let cached = self
            .command_outputs
            .cached
            .get(&(
                intent.target.clone(),
                if method == Method::Detail {
                    intent.fallback_revision.clone()
                } else {
                    None
                },
            ))
            .filter(|(at, _)| at.elapsed() < RETAIN_COMPLETED)
            .map(|(_, output)| output.clone());
        let request_id = if let Some(rpc) = rpc.filter(|_| cached.is_none()) {
            let id = self.allocate_id();
            let mut payload =
                json!({"threadId":intent.target.thread_id,"itemId":intent.target.item_id});
            if method == Method::Detail {
                payload["revision"] = json!(intent.fallback_revision);
            }
            self.send_request(&id, rpc, payload).await?;
            Some(id)
        } else {
            None
        };
        let subscription = Subscription {
            intent,
            method,
            request_id,
            opened_at: Instant::now(),
            terminal: TerminalOutput::default(),
            output: cached.unwrap_or(if method == Method::Inline {
                CommandOutput::Inline
            } else {
                CommandOutput::Pending
            }),
        };
        subscription.publish(events);
        self.command_outputs
            .active
            .insert(subscription.intent.attachment, subscription);
        Ok(())
    }

    async fn close_command_output(&mut self, attachment: Uuid) -> Result<(), SessionStop> {
        if let Some(request_id) = self.command_outputs.remove(attachment) {
            self.interrupt(&request_id).await?;
        }
        Ok(())
    }

    pub(super) async fn close_command_outputs(&mut self) -> Result<(), SessionStop> {
        let attachments: Vec<_> = self.command_outputs.active.keys().copied().collect();
        for attachment in attachments {
            self.close_command_output(attachment).await?;
        }
        Ok(())
    }

    pub(super) async fn expire_command_outputs(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let mut expired = Vec::new();
        self.command_outputs
            .cached
            .retain(|_, (at, _)| at.elapsed() < RETAIN_COMPLETED);
        for subscription in self.command_outputs.active.values_mut() {
            if matches!(subscription.output, CommandOutput::Pending)
                && subscription.opened_at.elapsed() >= REQUEST_TIMEOUT
                && let Some(request_id) = subscription.request_id.take()
            {
                subscription.output = CommandOutput::Failed(
                    "The environment did not return command output in time.".into(),
                );
                subscription.publish(events);
                expired.push(request_id);
            }
        }
        for id in expired {
            self.interrupt(&id).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
