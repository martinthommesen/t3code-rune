use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::OwnedMutexGuard;

use super::diagnostic::GitCommandError;
use super::{CHECKOUT, CONFIG_LEASES, lease, trim_js};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 1_000_000;

pub(super) struct Output {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

#[derive(Clone, Copy)]
pub(super) struct Git<'a> {
    cwd: &'a str,
}

impl<'a> Git<'a> {
    pub(super) fn new(cwd: &'a str) -> Self {
        Self { cwd }
    }

    pub(super) async fn has_ref(
        self,
        operation: &'static str,
        reference: &str,
    ) -> Result<bool, GitCommandError> {
        self.probe(operation, &["show-ref", "--verify", "--quiet", reference])
            .await
            .map(|result| result.status.success())
    }

    pub(super) async fn probe(
        self,
        operation: &'static str,
        args: &[&str],
    ) -> Result<Output, GitCommandError> {
        self.run(operation, args, Some(PROBE_TIMEOUT)).await
    }

    pub(super) async fn checkout(self, args: &[&str]) -> Result<Output, GitCommandError> {
        self.run(CHECKOUT, args, None).await
    }

    pub(super) async fn checked(
        self,
        operation: &'static str,
        args: &[&str],
        detail: Option<&str>,
    ) -> Result<Output, GitCommandError> {
        self.checked_with_timeout(operation, args, detail, COMMAND_TIMEOUT)
            .await
    }

    pub(super) async fn checked_with_timeout(
        self,
        operation: &'static str,
        args: &[&str],
        detail: Option<&str>,
        timeout: Duration,
    ) -> Result<Output, GitCommandError> {
        let output = self.run(operation, args, Some(timeout)).await?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(GitCommandError::failed(
                operation, self.cwd, args, &output, detail,
            ))
        }
    }

    pub(super) async fn config_lease(self) -> Option<OwnedMutexGuard<()>> {
        let common = self
            .probe(
                "GitVcsDriver.resolveGitCommonDir",
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
            .ok()?;
        if !common.status.success() {
            return None;
        }
        let directory = PathBuf::from(trim_js(&common.stdout));
        let directory = directory.canonicalize().unwrap_or(directory);
        Some(lease(&CONFIG_LEASES, &directory).await)
    }

    async fn run(
        self,
        operation: &'static str,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<Output, GitCommandError> {
        run(Path::new(self.cwd), args, timeout)
            .await
            .map_err(|error| {
                let detail = match error.kind() {
                    io::ErrorKind::TimedOut => "Git command timed out.",
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
                        "Failed to spawn Git process."
                    }
                    _ => "Failed to read Git process output.",
                };
                GitCommandError::process(operation, self.cwd, args, detail)
            })
    }
}

pub(super) async fn repository_root(cwd: &Path) -> Option<PathBuf> {
    let root = run(cwd, &["rev-parse", "--show-toplevel"], Some(PROBE_TIMEOUT))
        .await
        .ok()?;
    if !root.status.success() {
        return None;
    }
    let root = root.stdout.trim_end_matches(['\r', '\n']);
    (!root.is_empty()).then(|| PathBuf::from(root))
}

async fn run(cwd: &Path, args: &[&str], timeout: Option<Duration>) -> io::Result<Output> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(timeout.is_some())
        .env("LANGUAGE", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
        .args(["-c", "safe.bareRepository=explicit"]);
    #[cfg(windows)]
    command.args(["-c", "core.longpaths=true"]);
    command.args(args);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("Git stdout is piped");
    let stderr = child.stderr.take().expect("Git stderr is piped");
    let wait = async {
        let (status, stdout, stderr) = tokio::try_join!(child.wait(), read(stdout), read(stderr))?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    };
    if let Some(timeout) = timeout {
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| io::ErrorKind::TimedOut)?
    } else {
        wait.await
    }
}

async fn read(mut output: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut buffer = vec![0; 8192];
    let mut bytes = Vec::new();
    loop {
        let received = output.read(&mut buffer).await?;
        if received == 0 {
            break;
        }
        let retained = received.min(OUTPUT_LIMIT - bytes.len());
        bytes.extend_from_slice(&buffer[..retained]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
