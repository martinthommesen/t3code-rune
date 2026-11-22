pub mod claude;
pub mod codex;
mod ndjson;
mod process;
mod protocol;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const CLIENT_NAME: &str = "T3 Code";
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const QUEUE_CAPACITY: usize = 32;
const STARTUP_EVENT_CAPACITY: usize = 128;

pub struct CodexLaunch {
    pub executable: PathBuf,
    pub working_directory: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: HashMap<OsString, OsString>,
    pub client_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInitialization {
    pub user_agent: String,
    pub codex_home: PathBuf,
    pub platform_family: String,
    pub platform_os: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(serde_json::Number),
    Text(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug)]
pub enum CodexEvent {
    Notification {
        method: String,
        params: Option<Value>,
    },
    Request {
        id: RequestId,
        method: String,
        params: Option<Value>,
    },
}

#[derive(Clone, Debug)]
pub enum CodexError {
    Spawn(Arc<io::Error>),
    Io(Arc<io::Error>),
    InvalidFrame,
    InvalidInitialization,
    FrameTooLarge,
    Overloaded,
    Closed,
    Timeout(String),
    Exited {
        code: Option<i32>,
        diagnostic: Option<String>,
    },
    Remote {
        method: String,
        error: Box<RpcError>,
    },
    RequestAlreadyResolved,
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "Could not start Codex app-server: {error}"),
            Self::Io(error) => write!(f, "Codex app-server I/O failed: {error}"),
            Self::InvalidFrame => f.write_str("Codex app-server sent an invalid JSON-RPC frame"),
            Self::InvalidInitialization => {
                f.write_str("Codex app-server returned invalid initialization metadata")
            }
            Self::FrameTooLarge => {
                f.write_str("Codex app-server JSON-RPC frame exceeded the size limit")
            }
            Self::Overloaded => f.write_str("Codex app-server transport is overloaded"),
            Self::Closed => f.write_str("Codex app-server transport closed"),
            Self::Timeout(method) => write!(f, "Codex app-server timed out during {method}"),
            Self::Exited { code, diagnostic } => {
                match code {
                    Some(code) => write!(f, "Codex app-server exited with code {code}")?,
                    None => f.write_str("Codex app-server terminated")?,
                }
                if let Some(diagnostic) = diagnostic {
                    write!(f, ": {diagnostic}")?;
                }
                Ok(())
            }
            Self::Remote { method, error } => write!(
                f,
                "Codex {method} failed: {} ({})",
                error.message, error.code
            ),
            Self::RequestAlreadyResolved => f.write_str("Codex server request already resolved"),
        }
    }
}

impl std::error::Error for CodexError {}

type Reply<T> = oneshot::Sender<Result<T, CodexError>>;

enum Command {
    Request {
        method: String,
        params: Option<Value>,
        reply: Reply<Value>,
    },
    Notify {
        method: String,
        params: Option<Value>,
        reply: Reply<()>,
    },
    Respond {
        id: RequestId,
        result: Result<Value, RpcError>,
        reply: Reply<()>,
    },
}

#[derive(Clone)]
pub struct CodexClient {
    commands: mpsc::Sender<Command>,
    terminal_error: watch::Receiver<Option<CodexError>>,
}

impl CodexClient {
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value, CodexError> {
        self.submit(method, deadline, |reply| Command::Request {
            method: method.to_owned(),
            params,
            reply,
        })
        .await
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), CodexError> {
        self.submit(method, WRITE_TIMEOUT, |reply| Command::Notify {
            method: method.to_owned(),
            params,
            reply,
        })
        .await
    }

    pub async fn respond(
        &self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> Result<(), CodexError> {
        self.submit(
            "sending a server request response",
            WRITE_TIMEOUT,
            |reply| Command::Respond { id, result, reply },
        )
        .await
    }

    async fn submit<T>(
        &self,
        method: &str,
        deadline: Duration,
        command: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, CodexError> {
        timeout(deadline, async {
            let (send, receive) = oneshot::channel();
            self.commands
                .send(command(send))
                .await
                .map_err(|_| self.closed())?;
            receive.await.map_err(|_| self.closed())?
        })
        .await
        .map_err(|_| CodexError::Timeout(method.to_owned()))?
    }

    fn closed(&self) -> CodexError {
        self.terminal_error
            .borrow()
            .clone()
            .unwrap_or(CodexError::Closed)
    }
}

pub struct CodexAppServer {
    client: CodexClient,
    startup_events: VecDeque<CodexEvent>,
    events: mpsc::Receiver<CodexEvent>,
    stop: oneshot::Sender<()>,
    supervisor: JoinHandle<Result<(), CodexError>>,
    pub initialization: CodexInitialization,
}

impl CodexAppServer {
    pub async fn launch(launch: CodexLaunch) -> Result<Self, CodexError> {
        let payload = json!({
            "clientInfo": {"name": CLIENT_NAME, "title": CLIENT_NAME, "version": launch.client_version},
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": ["turn/diff/updated"],
                "extensions": {"io.modelcontextprotocol/ui": {"mimeTypes": ["text/html;profile=mcp-app"]}}
            }
        });
        let (command_tx, command_rx) = mpsc::channel(QUEUE_CAPACITY);
        let (event_tx, mut events) = mpsc::channel(QUEUE_CAPACITY);
        let (stop, stop_rx) = oneshot::channel();
        let (error_tx, terminal_error) = watch::channel(None);
        let supervisor = process::spawn(launch, command_rx, event_tx, stop_rx, error_tx)?;
        let client = CodexClient {
            commands: command_tx,
            terminal_error,
        };
        let mut startup_events = VecDeque::new();
        let handshake = initialize(&client, payload, &mut events, &mut startup_events).await;
        match handshake {
            Ok(initialization) => Ok(Self {
                client,
                startup_events,
                events,
                stop,
                supervisor,
                initialization,
            }),
            Err(error) => {
                let _ = stop.send(());
                let _ = supervisor.await;
                Err(error)
            }
        }
    }

    pub fn client(&self) -> CodexClient {
        self.client.clone()
    }

    /// Drain this stream while requests are in flight. Server requests require an explicit response.
    pub async fn next_event(&mut self) -> Result<CodexEvent, CodexError> {
        match self.startup_events.pop_front() {
            Some(event) => Ok(event),
            None => self.events.recv().await.ok_or_else(|| self.client.closed()),
        }
    }

    pub async fn shutdown(self) -> Result<(), CodexError> {
        let _ = self.stop.send(());
        self.supervisor.await.map_err(|_| CodexError::Closed)?
    }
}

async fn initialize(
    client: &CodexClient,
    payload: Value,
    events: &mut mpsc::Receiver<CodexEvent>,
    startup_events: &mut VecDeque<CodexEvent>,
) -> Result<CodexInitialization, CodexError> {
    let handshake = async {
        let result = client
            .request("initialize", Some(payload), INITIALIZE_TIMEOUT)
            .await?;
        let initialization =
            serde_json::from_value(result).map_err(|_| CodexError::InvalidInitialization)?;
        client.notify("initialized", None).await?;
        Ok(initialization)
    };
    tokio::pin!(handshake);
    loop {
        tokio::select! {
            biased;
            result = &mut handshake => return result,
            event = events.recv() => {
                let event = event.ok_or_else(|| client.closed())?;
                if startup_events.len() == STARTUP_EVENT_CAPACITY {
                    return Err(CodexError::Overloaded);
                }
                startup_events.push_back(event);
            }
        }
    }
}

#[cfg(test)]
mod tests;
