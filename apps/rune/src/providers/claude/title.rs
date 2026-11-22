use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use super::launch;

const OUTPUT_LIMIT: u64 = 1024 * 1024;
const STDERR_LIMIT: u64 = 64 * 1024;
const NO_SESSION_PERSISTENCE: &str = "--no-session-persistence";
const NO_SESSION_PERSISTENCE_REJECTED: &str = "error: unknown option '--no-session-persistence'";

pub struct TitleRequest<'a> {
    pub prompt: &'a str,
    pub schema: &'a Value,
    pub model: &'a str,
}

pub async fn run_title(
    executable: &Path,
    environment: &HashMap<OsString, OsString>,
    request: TitleRequest<'_>,
    deadline: Duration,
) -> io::Result<Value> {
    timeout(deadline, execute(executable, environment, request))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude Code title request timed out.",
            )
        })?
}

async fn execute(
    executable: &Path,
    environment: &HashMap<OsString, OsString>,
    request: TitleRequest<'_>,
) -> io::Result<Value> {
    let directory = tempfile::Builder::new()
        .prefix("t3code-claude-title-")
        .tempdir()?;
    let schema = serde_json::to_string(request.schema)?;
    let args = TitleCommand {
        executable,
        cwd: directory.path(),
        environment,
        prompt: request.prompt,
        schema: &schema,
        model: request.model,
    };
    let mut output = args.run(true).await?;
    if !output.status.success()
        && String::from_utf8_lossy(&output.stderr).contains(NO_SESSION_PERSISTENCE_REJECTED)
    {
        output = args.run(false).await?;
    }
    if !output.status.success() {
        let detail = output.status.code().map_or_else(
            || "Claude Code closed before generating a title.".to_owned(),
            |code| format!("Claude Code title request exited with code {code}."),
        );
        return Err(io::Error::other(detail));
    }
    if !output.prompt_written {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "Claude Code closed its input before reading the title prompt.",
        ));
    }
    parse_structured_output(&output.stdout)
}

struct TitleCommand<'a> {
    executable: &'a Path,
    cwd: &'a Path,
    environment: &'a HashMap<OsString, OsString>,
    prompt: &'a str,
    schema: &'a str,
    model: &'a str,
}

struct Capture {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    prompt_written: bool,
}

impl TitleCommand<'_> {
    async fn run(&self, no_session_persistence: bool) -> io::Result<Capture> {
        let mut command = launch::command(self.executable, self.cwd, self.environment);
        command.args([
            "-p",
            "--output-format",
            "json",
            "--json-schema",
            self.schema,
            "--model",
            self.model,
            "--effort",
            "high",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--tools",
            "",
            "--disable-slash-commands",
        ]);
        if no_session_persistence {
            command.arg(NO_SESSION_PERSISTENCE);
        }
        let mut child = command
            .args(["--strict-mcp-config", "--permission-mode", "dontAsk"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped title input");
        let stdout = child.stdout.take().expect("piped title output");
        let stderr = child.stderr.take().expect("piped title diagnostics");
        let send_prompt = async move {
            let result = async {
                stdin.write_all(self.prompt.as_bytes()).await?;
                stdin.shutdown().await
            }
            .await;
            drop(stdin);
            match result {
                // Older Claude versions can reject an argument before reading stdin.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
                Err(error) => Err(error),
                Ok(()) => Ok(true),
            }
        };
        let (prompt_written, stdout, stderr, status) = tokio::try_join!(
            send_prompt,
            bounded_output(stdout, OUTPUT_LIMIT),
            bounded_output(stderr, STDERR_LIMIT),
            child.wait(),
        )?;
        Ok(Capture {
            status,
            stdout,
            stderr,
            prompt_written,
        })
    }
}

async fn bounded_output(reader: impl AsyncRead + Unpin, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude Code title output exceeded its size limit.",
        ));
    }
    Ok(bytes)
}

fn parse_structured_output(bytes: &[u8]) -> io::Result<Value> {
    let output: Value = serde_json::from_slice(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let envelope = match &output {
        Value::Object(_) => Some(&output),
        Value::Array(messages) if messages.iter().all(|message| message["type"].is_string()) => {
            messages.iter().rfind(|message| message["type"] == "result")
        }
        _ => None,
    };
    envelope
        .and_then(|envelope| envelope.get("structured_output"))
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude Code did not return a structured title result.",
            )
        })
}

#[cfg(all(test, unix))]
mod tests;
