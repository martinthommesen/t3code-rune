use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::pin::pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

use super::protocol::{self, Frame};
use super::{
    CLIENT_NAME, CodexError, CodexEvent, CodexLaunch, Command, QUEUE_CAPACITY, Reply, RequestId,
    RpcError, WRITE_TIMEOUT,
};

const EXIT_STATUS_GRACE: Duration = Duration::from_millis(250);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const STDERR_TAIL_BYTES: usize = 8192;

pub(super) fn spawn(
    launch: CodexLaunch,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<CodexEvent>,
    stop: oneshot::Receiver<()>,
    error: watch::Sender<Option<CodexError>>,
) -> Result<JoinHandle<Result<(), CodexError>>, CodexError> {
    let mut child = tokio::process::Command::new(launch.executable)
        .args(["app-server", "-c", "thread_unload_delay_secs=0"])
        .args(launch.arguments)
        .current_dir(launch.working_directory)
        .env("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", CLIENT_NAME)
        .envs(launch.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| CodexError::Spawn(error.into()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| unavailable_pipe("stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| unavailable_pipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| unavailable_pipe("stderr"))?;
    let process = Process {
        child,
        stdin,
        stdout,
        stderr,
    };
    Ok(tokio::spawn(supervise(
        process, commands, events, stop, error,
    )))
}

fn unavailable_pipe(name: &str) -> CodexError {
    CodexError::Spawn(Arc::new(io::Error::other(format!(
        "Child process {name} was not piped"
    ))))
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

struct Session {
    writes: mpsc::Sender<Vec<u8>>,
    events: mpsc::Sender<CodexEvent>,
    pending: HashMap<u64, (String, Reply<Value>)>,
    incoming_requests: HashSet<RequestId>,
    next_id: u64,
}

impl Session {
    fn send(&self, frame: Value) -> Result<(), CodexError> {
        let mut frame = serde_json::to_vec(&frame).map_err(|_| CodexError::InvalidFrame)?;
        if frame.len() > protocol::MAX_FRAME_BYTES {
            return Err(CodexError::FrameTooLarge);
        }
        frame.push(b'\n');
        self.writes.try_send(frame).map_err(queue_error)
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Request {
                method,
                params,
                reply,
            } => {
                self.pending.retain(|_, (_, reply)| !reply.is_closed());
                if reply.is_closed() {
                    return;
                }
                if self.pending.len() == QUEUE_CAPACITY {
                    let _ = reply.send(Err(CodexError::Overloaded));
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                match self.send(protocol::method_frame(Some(id), &method, params)) {
                    Ok(()) => {
                        self.pending.insert(id, (method, reply));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Command::Notify {
                method,
                params,
                reply,
            } => {
                if !reply.is_closed() {
                    let _ = reply.send(self.send(protocol::method_frame(None, &method, params)));
                }
            }
            Command::Respond { id, result, reply } => {
                if reply.is_closed() {
                    return;
                }
                let result = if self.incoming_requests.contains(&id) {
                    self.send(protocol::response_frame(id.clone(), result))
                        .map(|()| {
                            self.incoming_requests.remove(&id);
                        })
                } else {
                    Err(CodexError::RequestAlreadyResolved)
                };
                let _ = reply.send(result);
            }
        }
    }

    fn receive(&mut self, frame: Frame) -> Result<Option<CodexEvent>, CodexError> {
        let event = match frame {
            Frame::Notification { method, params } => {
                if method == "serverRequest/resolved"
                    && let Some(value) = params.as_ref().and_then(|params| params.get("requestId"))
                    && let Ok(id) = protocol::decode_id(value.clone())
                {
                    self.incoming_requests.remove(&id);
                }
                CodexEvent::Notification { method, params }
            }
            Frame::Request { id, method, params } => {
                if self.incoming_requests.contains(&id) {
                    return Ok(None);
                }
                if self.incoming_requests.len() == QUEUE_CAPACITY {
                    self.send(protocol::response_frame(
                        id,
                        Err(RpcError {
                            code: -32001,
                            message: "Too many Codex requests are already active.".into(),
                            data: None,
                        }),
                    ))?;
                    return Ok(None);
                }
                self.incoming_requests.insert(id.clone());
                CodexEvent::Request { id, method, params }
            }
            Frame::Response { id, result } => {
                if let Some(id) = protocol::client_request_id(id)
                    && let Some((method, reply)) = self.pending.remove(&id)
                {
                    let _ = reply.send(result.map_err(|error| CodexError::Remote {
                        method,
                        error: Box::new(error),
                    }));
                }
                return Ok(None);
            }
        };
        Ok(Some(event))
    }
}

fn queue_error<T>(error: mpsc::error::TrySendError<T>) -> CodexError {
    match error {
        mpsc::error::TrySendError::Full(_) => CodexError::Overloaded,
        mpsc::error::TrySendError::Closed(_) => CodexError::Closed,
    }
}

enum End {
    Shutdown,
    Pipe(CodexError),
    Process(ExitStatus),
    Failed(CodexError),
}

async fn run_session(
    child: &mut Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    commands: &mut mpsc::Receiver<Command>,
    stop: oneshot::Receiver<()>,
    writes: mpsc::Receiver<Vec<u8>>,
    session: &mut Session,
) -> End {
    let mut stop = stop;
    let mut stdout = BufReader::new(stdout);
    let mut frame = Vec::new();
    let mut writer = pin!(write_frames(stdin, writes));
    let mut drain_deadline = pin!(sleep(Duration::ZERO));
    let mut exited = None;
    let events = session.events.clone();
    let mut waiting_event = None;
    loop {
        tokio::select! {
            _ = &mut stop => return End::Shutdown,
            status = child.wait(), if exited.is_none() => match status {
                Ok(status) => {
                    exited = Some(status);
                    drain_deadline.as_mut().reset(Instant::now() + EXIT_STATUS_GRACE);
                }
                Err(error) => return End::Failed(CodexError::Io(error.into())),
            },
            _ = &mut drain_deadline, if exited.is_some() && waiting_event.is_none() => return End::Process(exited.expect("draining an exited process")),
            permit = events.reserve(), if waiting_event.is_some() => match permit {
                Ok(permit) => {
                    permit.send(waiting_event.take().expect("event awaiting delivery"));
                    drain_deadline.as_mut().reset(Instant::now() + EXIT_STATUS_GRACE);
                }
                Err(_) => return End::Failed(CodexError::Closed),
            },
            incoming = protocol::read_frame(&mut stdout, &mut frame, protocol::MAX_FRAME_BYTES), if waiting_event.is_none() => match incoming {
                Ok(Some(frame)) => {
                    if exited.is_some() {
                        drain_deadline.as_mut().reset(Instant::now() + EXIT_STATUS_GRACE);
                    }
                    match session.receive(frame) {
                        Ok(Some(event)) => match events.try_send(event) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(event)) => waiting_event = Some(event),
                            Err(mpsc::error::TrySendError::Closed(_)) => return End::Failed(CodexError::Closed),
                        },
                        Ok(None) => {},
                        Err(error) => return End::Failed(error),
                    }
                }
                Ok(None) => return exited.map_or(End::Pipe(CodexError::Closed), End::Process),
                Err(error @ CodexError::Io(_)) => return exited.map_or(End::Pipe(error), End::Process),
                Err(error) => return End::Failed(error),
            },
            result = &mut writer, if exited.is_none() => return End::Pipe(result),
            command = commands.recv(), if exited.is_none() => match command {
                Some(command) => session.command(command),
                None => return End::Shutdown,
            }
        }
    }
}

async fn write_frames(mut stdin: ChildStdin, mut frames: mpsc::Receiver<Vec<u8>>) -> CodexError {
    while let Some(frame) = frames.recv().await {
        match timeout(WRITE_TIMEOUT, stdin.write_all(&frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return CodexError::Io(error.into()),
            Err(_) => return CodexError::Timeout("writing a frame".into()),
        }
    }
    CodexError::Closed
}

async fn supervise(
    process: Process,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<CodexEvent>,
    stop: oneshot::Receiver<()>,
    error: watch::Sender<Option<CodexError>>,
) -> Result<(), CodexError> {
    let Process {
        mut child,
        stdin,
        stdout,
        stderr,
    } = process;
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)));
    let mut drain = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));
    let (write, writes) = mpsc::channel(QUEUE_CAPACITY);
    let mut session = Session {
        writes: write,
        events,
        pending: HashMap::new(),
        incoming_requests: HashSet::new(),
        next_id: 1,
    };
    let reason = run_session(
        &mut child,
        stdin,
        stdout,
        &mut commands,
        stop,
        writes,
        &mut session,
    )
    .await;
    let failure = match reason {
        End::Shutdown => None,
        End::Failed(error) => Some(error),
        End::Process(status) => Some(exited(status, &mut drain, &stderr_tail).await),
        End::Pipe(fallback) => Some(match timeout(EXIT_STATUS_GRACE, child.wait()).await {
            Ok(Ok(status)) => exited(status, &mut drain, &stderr_tail).await,
            _ => fallback,
        }),
    };
    let terminal_error = failure.clone().unwrap_or(CodexError::Closed);
    error.send_replace(Some(terminal_error.clone()));
    drop(commands);
    let Session {
        pending, events, ..
    } = session;
    drop(events);
    for (_, (_, reply)) in pending {
        let _ = reply.send(Err(terminal_error.clone()));
    }
    let stopped = stop_child(&mut child).await;
    drain.abort();
    match failure {
        Some(error) => Err(error),
        None => stopped,
    }
}

async fn stop_child(child: &mut Child) -> Result<(), CodexError> {
    match timeout(SHUTDOWN_GRACE, child.wait()).await {
        Ok(status) => status
            .map(|_| ())
            .map_err(|error| CodexError::Io(error.into())),
        Err(_) => {
            child
                .start_kill()
                .map_err(|error| CodexError::Io(error.into()))?;
            timeout(SHUTDOWN_GRACE, child.wait())
                .await
                .map_err(|_| CodexError::Timeout("reaping the process".into()))?
                .map(|_| ())
                .map_err(|error| CodexError::Io(error.into()))
        }
    }
}

async fn drain_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<VecDeque<u8>>>) {
    let mut buffer = [0; 4096];
    while let Ok(count) = stderr.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        let mut tail = tail.lock().expect("stderr tail lock");
        let overflow = (tail.len() + count).saturating_sub(STDERR_TAIL_BYTES);
        tail.drain(..overflow);
        tail.extend(&buffer[..count]);
    }
}

async fn exited(
    status: ExitStatus,
    drain: &mut JoinHandle<()>,
    tail: &Mutex<VecDeque<u8>>,
) -> CodexError {
    let _ = timeout(EXIT_STATUS_GRACE, drain).await;
    let mut tail = tail.lock().expect("stderr tail lock");
    let text = String::from_utf8_lossy(tail.make_contiguous());
    let diagnostic = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_control())
                .take(512)
                .collect()
        });
    CodexError::Exited {
        code: status.code(),
        diagnostic,
    }
}
