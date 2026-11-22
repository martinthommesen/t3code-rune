mod launch;
mod probe;
mod process;
mod usage;
mod wire;

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::timeout;
use uuid::Uuid;

use process::Process;
use wire::Incoming;

pub use launch::{Launch, PermissionMode, SessionTarget};
pub use probe::{Probe, ProbeAccount, ProbeLaunch, ProbeModel};
pub use usage::{ContextUsage, ProcessLedger, TurnUsage};
pub use wire::{
    AssistantBlock, AssistantMessage, AssistantSnapshot, CacheCreation, Initialization, Message,
    ModelUsage, OutputDetails, TurnResult, Usage,
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug)]
pub enum Error {
    Spawn(io::Error),
    Io(io::Error),
    Exited(Option<i32>),
    FrameTooLarge,
    InvalidFrame,
    InvalidSession,
    ActiveTurn,
    InitializationFailed,
    InitializeTimeout,
    WriteTimeout,
    UnexpectedControlResponse,
    TooManyEarlyMessages,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "Could not start Claude Code: {error}"),
            Self::Io(error) => write!(formatter, "Claude Code I/O failed: {error}"),
            Self::Exited(Some(code)) => write!(formatter, "Claude Code exited with code {code}"),
            Self::Exited(None) => formatter.write_str("Claude Code closed its message stream"),
            Self::FrameTooLarge => formatter.write_str("Claude Code frame exceeded the size limit"),
            Self::InvalidFrame => formatter.write_str("Claude Code sent an invalid message"),
            Self::InvalidSession => {
                formatter.write_str("Claude Code sent a message for a different session")
            }
            Self::ActiveTurn => formatter.write_str("Claude Code already has an active prompt"),
            Self::InitializationFailed => formatter.write_str("Claude Code refused initialization"),
            Self::InitializeTimeout => formatter.write_str("Claude Code initialization timed out"),
            Self::WriteTimeout => {
                formatter.write_str("Claude Code did not read the message in time")
            }
            Self::UnexpectedControlResponse => {
                formatter.write_str("Claude Code returned an unexpected control response")
            }
            Self::TooManyEarlyMessages => {
                formatter.write_str("Claude Code sent too many messages before initialization")
            }
        }
    }
}

impl std::error::Error for Error {}

pub struct Session {
    process: Process,
    early: VecDeque<Incoming>,
    session_id: Uuid,
    active_turn: bool,
}

impl Session {
    pub async fn open(launch: Launch) -> Result<Self, Error> {
        let mut session = Self {
            process: Process::spawn(launch.command())?,
            early: VecDeque::new(),
            session_id: launch.target.id(),
            active_turn: false,
        };
        timeout(INITIALIZE_TIMEOUT, session.initialize(&launch))
            .await
            .map_err(|_| Error::InitializeTimeout)??;
        Ok(session)
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    pub async fn offer_text(&mut self, text: &str, prompt_id: Option<Uuid>) -> Result<(), Error> {
        if self.active_turn {
            return Err(Error::ActiveTurn);
        }
        let mut prompt = json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "parent_tool_use_id": null,
        });
        if let Some(prompt_id) = prompt_id {
            prompt["uuid"] = Value::String(prompt_id.to_string());
        }
        self.process.write(prompt).await?;
        self.active_turn = true;
        Ok(())
    }

    pub async fn next_message(&mut self) -> Result<Message, Error> {
        let frame = match self.early.pop_front() {
            Some(frame) => frame,
            None => self.process.read().await?,
        };
        match frame {
            Incoming::Message(message) => {
                match &message {
                    Message::Initialized(initialization) => {
                        self.session_id = initialization.session_id
                    }
                    Message::Assistant(assistant) if assistant.session_id != self.session_id => {
                        return Err(Error::InvalidSession);
                    }
                    Message::Result(result) => {
                        if result.session_id != self.session_id {
                            return Err(Error::InvalidSession);
                        }
                        self.active_turn = false;
                    }
                    _ => {}
                }
                Ok(message)
            }
            Incoming::Request {
                id,
                subtype,
                tool_use_id,
            } => {
                let response = if subtype == "can_use_tool" {
                    let mut content =
                        json!({"behavior":"deny", "message":"Tool approval is unavailable."});
                    if let Some(tool_use_id) = tool_use_id {
                        content["toolUseID"] = Value::String(tool_use_id);
                    }
                    json!({"subtype":"success", "request_id":id, "response":content})
                } else {
                    json!({"subtype":"error", "request_id":id,
                        "error":"This Claude Code control request is unavailable."})
                };
                self.process
                    .write(json!({"type":"control_response", "response":response}))
                    .await?;
                Ok(Message::RejectedControl { subtype })
            }
            Incoming::Response { .. } => Err(Error::UnexpectedControlResponse),
        }
    }

    pub async fn shutdown(self) -> Result<(), Error> {
        self.process.shutdown().await
    }

    async fn initialize(&mut self, launch: &Launch) -> Result<(), Error> {
        let mut request =
            json!({"subtype":"initialize", "appendSystemPrompt":launch.append_system_prompt});
        if let Some(title) = &launch.title {
            request["title"] = Value::String(title.clone());
        }
        self.process
            .initialize(request, &mut self.early)
            .await
            .map(|_| ())
    }
}

#[cfg(all(test, unix))]
mod tests;
