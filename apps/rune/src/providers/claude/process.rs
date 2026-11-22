use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use super::super::ndjson::{self, MAX_FRAME_BYTES, ReadError};
use super::{Error, wire::Incoming};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const EARLY_MESSAGES: usize = 128;

pub(super) struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: JoinHandle<io::Result<u64>>,
    buffer: Vec<u8>,
}

impl Process {
    pub(super) fn spawn(mut command: Command) -> Result<Self, Error> {
        let mut child = command
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
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr,
            buffer: Vec::new(),
        })
    }

    pub(super) async fn initialize(
        &mut self,
        request: Value,
        early: &mut VecDeque<Incoming>,
    ) -> Result<Value, Error> {
        let id = Uuid::new_v4().to_string();
        self.write(json!({"type":"control_request", "request_id":id, "request":request}))
            .await?;
        loop {
            match self.read().await? {
                Incoming::Response { id: actual, result } if actual == id => return result,
                Incoming::Response { .. } => return Err(Error::UnexpectedControlResponse),
                message if early.len() < EARLY_MESSAGES => early.push_back(message),
                _ => return Err(Error::TooManyEarlyMessages),
            }
        }
    }
    pub(super) async fn shutdown(mut self) -> Result<(), Error> {
        drop(self.stdin.take());
        match timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(result) => result.map(|_| ()).map_err(Error::Io),
            Err(_) => self.child.kill().await.map_err(Error::Io),
        }
    }

    pub(super) async fn read(&mut self) -> Result<Incoming, Error> {
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

    pub(super) async fn write(&mut self, value: Value) -> Result<(), Error> {
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

impl Drop for Process {
    fn drop(&mut self) {
        self.stderr.abort();
    }
}

fn missing_pipe(name: &str) -> Error {
    Error::Spawn(io::Error::other(format!(
        "Claude Code {name} was not piped"
    )))
}
