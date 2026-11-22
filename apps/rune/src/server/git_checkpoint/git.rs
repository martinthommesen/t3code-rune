use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use super::CheckpointError;
use super::objects::Scratch;

const MAX_STDOUT: u64 = 16 * 1024 * 1024;
const MAX_STDERR: u64 = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(super) struct Git<'a> {
    cwd: &'a Path,
    scratch: Option<&'a Scratch>,
    committer_date: Option<&'a str>,
    sparse_index: bool,
    stdout_limit: u64,
}

impl<'a> Git<'a> {
    pub(super) fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            scratch: None,
            committer_date: None,
            sparse_index: false,
            stdout_limit: MAX_STDOUT,
        }
    }

    pub(super) fn scratch(self, scratch: &'a Scratch) -> Self {
        Self {
            scratch: Some(scratch),
            ..self
        }
    }

    pub(super) fn date(self, committer_date: &'a str) -> Self {
        Self {
            committer_date: Some(committer_date),
            ..self
        }
    }

    pub(super) fn sparse_index(self) -> Self {
        Self {
            sparse_index: true,
            ..self
        }
    }

    pub(super) fn with_stdout_limit(self, stdout_limit: u64) -> Self {
        Self {
            stdout_limit,
            ..self
        }
    }

    pub(super) async fn command<I, S>(self, args: I) -> Result<Vec<u8>, CheckpointError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(args, None).await?.success()
    }

    pub(super) async fn run<I, S>(
        self,
        args: I,
        input: Option<&[u8]>,
    ) -> Result<Output, CheckpointError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut args = args.into_iter();
        let action = args
            .next()
            .ok_or(CheckpointError::InvalidOutput("empty Git command"))?;
        let operation = action.as_ref().to_string_lossy().into_owned();
        let mut command = Command::new("git");
        command
            .current_dir(self.cwd)
            .kill_on_drop(true)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_QUARANTINE_PATH")
            .env_remove("GIT_NAMESPACE")
            .args([
                "-c",
                "safe.bareRepository=explicit",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.fsync=objects,reference",
                "-c",
                "core.fsyncMethod=fsync",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "log.showSignature=false",
                "-c",
                "diff.relative=false",
                "-c",
                "diff.suppressBlankEmpty=false",
            ]);
        #[cfg(windows)]
        command.args(["-c", "core.longpaths=true"]);
        if let Some(scratch) = self.scratch {
            command
                .env("GIT_INDEX_FILE", scratch.index())
                .env("GIT_OBJECT_DIRECTORY", scratch.objects())
                .env("GIT_QUARANTINE_PATH", scratch.objects())
                .env(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    quote_alternate(scratch.shared_objects()),
                )
                .env("GIT_AUTHOR_NAME", "T3 Code")
                .env("GIT_AUTHOR_EMAIL", "t3code@users.noreply.github.com")
                .env("GIT_COMMITTER_NAME", "T3 Code")
                .env("GIT_COMMITTER_EMAIL", "t3code@users.noreply.github.com")
                .args([
                    "-c",
                    "sparse.expectFilesOutsideOfPatterns=false",
                    "-c",
                    "core.splitIndex=false",
                    "-c",
                    "splitIndex.sharedIndexExpire=never",
                    "-c",
                    if self.sparse_index {
                        "index.sparse=true"
                    } else {
                        "index.sparse=false"
                    },
                    "-c",
                    "core.safecrlf=false",
                    "-c",
                    "core.ignorestat=false",
                ]);
        }
        if let Some(date) = self.committer_date {
            command.env("GIT_COMMITTER_DATE", date);
        }
        let mut child = command.arg(action).args(args).spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or(CheckpointError::InvalidOutput("missing Git stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(CheckpointError::InvalidOutput("missing Git stderr"))?;
        let stdin = child.stdin.take();
        let write_input = async move {
            if let (Some(mut stream), Some(input)) = (stdin, input)
                && let Err(error) = stream.write_all(input).await
                && error.kind() != io::ErrorKind::BrokenPipe
            {
                return Err(error);
            }
            Ok(())
        };
        let (status, stdout, stderr, ()) = timeout(COMMAND_TIMEOUT, async {
            tokio::try_join!(
                child.wait(),
                bounded(stdout, self.stdout_limit),
                bounded(stderr, MAX_STDERR),
                write_input
            )
        })
        .await
        .map_err(|_| CheckpointError::Timeout(operation.clone()))??;
        Ok(Output {
            operation,
            status,
            stdout,
            stderr,
        })
    }
}

pub(super) struct Output {
    operation: String,
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

impl Output {
    pub(super) fn success(self) -> Result<Vec<u8>, CheckpointError> {
        if self.status.success() {
            Ok(self.stdout)
        } else {
            Err(self.error())
        }
    }

    pub(super) fn error(self) -> CheckpointError {
        CheckpointError::Git {
            operation: self.operation,
            status: self.status,
            message: String::from_utf8_lossy(&self.stderr).trim().to_owned(),
        }
    }
}

async fn bounded(reader: impl AsyncRead + Unpin, limit: u64) -> io::Result<Vec<u8>> {
    let mut collected = Vec::new();
    reader.take(limit + 1).read_to_end(&mut collected).await?;
    if collected.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git output exceeded the checkpoint limit",
        ));
    }
    Ok(collected)
}

fn quote_alternate(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for byte in path.as_os_str().as_encoded_bytes() {
        match *byte {
            b'"' | b'\\' => {
                quoted.push('\\');
                quoted.push(char::from(*byte));
            }
            b' '..=b'~' => quoted.push(char::from(*byte)),
            _ => write!(quoted, "\\{byte:03o}").expect("writing a string cannot fail"),
        }
    }
    quoted.push('"');
    quoted
}
