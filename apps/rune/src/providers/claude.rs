mod launch;
mod usage;
mod wire;

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use super::ndjson::{self, MAX_FRAME_BYTES, ReadError};
use wire::Incoming;

pub use launch::{Launch, PermissionMode, SessionTarget};
pub use usage::{ContextUsage, ProcessLedger, TurnUsage};
pub use wire::{
    AssistantBlock, AssistantMessage, AssistantSnapshot, CacheCreation, Initialization, Message,
    ModelUsage, OutputDetails, TurnResult, Usage,
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const EARLY_MESSAGES: usize = 128;

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
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: JoinHandle<io::Result<u64>>,
    buffer: Vec<u8>,
    early: VecDeque<Incoming>,
    session_id: Uuid,
    active_turn: bool,
}

impl Session {
    pub async fn open(launch: Launch) -> Result<Self, Error> {
        let mut child = launch
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(Error::Spawn)?;
        let stdin = child.stdin.take().ok_or_else(|| missing_pipe("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing_pipe("stdout"))?;
        let mut stderr = child.stderr.take().ok_or_else(|| missing_pipe("stderr"))?;
        let stderr =
            tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
        let mut session = Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr,
            buffer: Vec::new(),
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
        self.write(prompt).await?;
        self.active_turn = true;
        Ok(())
    }

    pub async fn next_message(&mut self) -> Result<Message, Error> {
        let frame = match self.early.pop_front() {
            Some(frame) => frame,
            None => self.read().await?,
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
                self.write(json!({"type":"control_response", "response":response}))
                    .await?;
                Ok(Message::RejectedControl { subtype })
            }
            Incoming::Response { .. } => Err(Error::UnexpectedControlResponse),
        }
    }

    pub async fn shutdown(mut self) -> Result<(), Error> {
        drop(self.stdin.take());
        match timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(result) => result.map(|_| ()).map_err(Error::Io),
            Err(_) => self.child.kill().await.map_err(Error::Io),
        }
    }

    async fn initialize(&mut self, launch: &Launch) -> Result<(), Error> {
        let id = Uuid::new_v4().to_string();
        let mut request =
            json!({"subtype":"initialize", "appendSystemPrompt":launch.append_system_prompt});
        if let Some(title) = &launch.title {
            request["title"] = Value::String(title.clone());
        }
        self.write(json!({"type":"control_request", "request_id":id, "request":request}))
            .await?;
        loop {
            match self.read().await? {
                Incoming::Response {
                    id: actual,
                    success,
                } if actual == id => {
                    return if success {
                        Ok(())
                    } else {
                        Err(Error::InitializationFailed)
                    };
                }
                Incoming::Response { .. } => return Err(Error::UnexpectedControlResponse),
                message if self.early.len() < EARLY_MESSAGES => self.early.push_back(message),
                _ => return Err(Error::TooManyEarlyMessages),
            }
        }
    }

    async fn read(&mut self) -> Result<Incoming, Error> {
        let found = ndjson::read_line(&mut self.stdout, &mut self.buffer, MAX_FRAME_BYTES)
            .await
            .map_err(|error| match error {
                ReadError::Io(error) => Error::Io(error),
                ReadError::TooLarge => Error::FrameTooLarge,
            })?;
        if !found {
            let exit = self.child.try_wait().map_err(Error::Io)?;
            return Err(Error::Exited(exit.and_then(|status| status.code())));
        }
        let frame = Incoming::decode(&self.buffer);
        self.buffer.clear();
        frame
    }

    async fn write(&mut self, value: Value) -> Result<(), Error> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Error::InvalidFrame)?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::FrameTooLarge);
        }
        bytes.push(b'\n');
        let stdin = self.stdin.as_mut().expect("live Claude Code stdin");
        let outcome = match timeout(WRITE_TIMEOUT, stdin.write_all(&bytes)).await {
            Ok(result) => result.map_err(Error::Io),
            Err(_) => Err(Error::WriteTimeout),
        };
        if outcome.is_err() {
            let _ = self.child.start_kill();
        }
        outcome
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stderr.abort();
    }
}

fn missing_pipe(name: &str) -> Error {
    Error::Spawn(io::Error::other(format!(
        "Claude Code {name} was not piped"
    )))
}

#[cfg(all(test, unix))]
mod tests;
