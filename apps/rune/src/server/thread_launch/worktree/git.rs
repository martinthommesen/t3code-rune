use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, watch};

const METADATA_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
static CONFIG_LOCKS: Mutex<BTreeMap<PathBuf, Weak<AsyncMutex<()>>>> = Mutex::new(BTreeMap::new());

pub(super) struct Output {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) async fn metadata(cwd: &Path, args: &[&str]) -> io::Result<Output> {
    run(cwd, args, Operation::Metadata).await
}

pub(super) async fn checkout(
    cwd: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    cancelled: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let output = run(cwd, args, Operation::Checkout { input, cancelled }).await?;
    if !output.status.success() {
        return Err(io::Error::other(
            "Git checkout did not finish successfully.",
        ));
    }
    Ok(())
}

pub(super) async fn fetch(
    cwd: &Path,
    args: &[&str],
    cancelled: &mut watch::Receiver<bool>,
) -> io::Result<Output> {
    run(cwd, args, Operation::Fetch(cancelled)).await
}

pub(super) async fn text(cwd: &Path, args: &[&str]) -> io::Result<String> {
    let output = metadata(cwd, args).await?;
    if !output.status.success() {
        return Err(io::Error::other("Git did not finish successfully."));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}

pub(super) async fn common_directory(cwd: &Path) -> io::Result<PathBuf> {
    let directory = text(cwd, &["rev-parse", "--git-common-dir"]).await?;
    cwd.join(directory.trim_end_matches('\n')).canonicalize()
}

pub(super) async fn lock_config(cwd: &Path) -> io::Result<OwnedMutexGuard<()>> {
    let common = common_directory(cwd).await?;
    let lock = {
        let mut locks = CONFIG_LOCKS.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&common).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(AsyncMutex::new(()));
            locks.insert(common, Arc::downgrade(&lock));
            lock
        }
    };
    Ok(lock.lock_owned().await)
}

enum Operation<'a> {
    Metadata,
    Checkout {
        input: Option<&'a [u8]>,
        cancelled: &'a mut watch::Receiver<bool>,
    },
    Fetch(&'a mut watch::Receiver<bool>),
}

async fn run(cwd: &Path, args: &[&str], operation: Operation<'_>) -> io::Result<Output> {
    let capture_stdout = matches!(operation, Operation::Metadata);
    let fetch = matches!(operation, Operation::Fetch(_));
    let (input, cancellation, timeout) = match operation {
        Operation::Metadata => (None, None, Some(METADATA_TIMEOUT)),
        Operation::Checkout { input, cancelled } => (input, Some(cancelled), None),
        Operation::Fetch(cancelled) => (None, Some(cancelled), Some(FETCH_TIMEOUT)),
    };
    if cancellation
        .as_ref()
        .is_some_and(|receiver| *receiver.borrow())
    {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if fetch { Stdio::piped() } else { Stdio::null() })
        .kill_on_drop(true)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_INDEX_FILE")
        .args(["-c", "safe.bareRepository=explicit"])
        .args(args);
    if fetch {
        command.envs([
            ("GCM_INTERACTIVE", "never"),
            ("GIT_ASKPASS", ""),
            ("SSH_ASKPASS", ""),
            ("SSH_ASKPASS_REQUIRE", "never"),
        ]);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let process_id = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();
    let wait = async {
        let feed = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(input).await?;
                stdin.shutdown().await?;
            }
            Ok(())
        };
        let (_, status, stdout, stderr) =
            tokio::try_join!(feed, child.wait(), read_output(stdout), read_output(stderr))?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    };
    let interrupted = async {
        if let Some(receiver) = cancellation {
            tokio::select! {
                biased;
                _ = receiver.wait_for(|cancelled| *cancelled) => Err(io::ErrorKind::Interrupted.into()),
                result = wait => result,
            }
        } else {
            wait.await
        }
    };
    let result = if let Some(timeout) = timeout {
        match tokio::time::timeout(timeout, interrupted).await {
            Ok(result) => result,
            Err(_) => Err(io::ErrorKind::TimedOut.into()),
        }
    } else {
        interrupted.await
    };
    if result.is_err() {
        terminate(&mut child, process_id).await;
    }
    result
}

async fn read_output(pipe: Option<impl AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
    let Some(mut pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let mut chunk = vec![0; 8192];
    let mut truncated = false;
    loop {
        let count = pipe.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let saved = count.min(MAX_OUTPUT - output.len());
        output.extend_from_slice(&chunk[..saved]);
        truncated |= saved < count;
    }
    if truncated {
        return Err(io::Error::other("Git output exceeded the supported size."));
    }
    Ok(output)
}

async fn terminate(child: &mut Child, process_id: Option<u32>) {
    if let Some(id) = process_id {
        #[cfg(unix)]
        let mut kill = {
            let mut command = Command::new("/bin/kill");
            command.args(["-KILL", "--", &format!("-{id}")]);
            command
        };
        #[cfg(windows)]
        let mut kill = {
            let mut command = Command::new("taskkill.exe");
            command.args(["/T", "/F", "/PID", &id.to_string()]);
            command
        };
        #[cfg(any(unix, windows))]
        {
            kill.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let _ = tokio::time::timeout(Duration::from_secs(2), kill.status()).await;
        }
    }
    let _ = child.kill().await;
}
