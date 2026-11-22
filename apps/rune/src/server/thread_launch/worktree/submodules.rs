use std::io;
use std::path::{Component, Path};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::watch;

use super::git;
use crate::server::thread_launch::setup_progress::{Progress, StageId, Status};

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Recursive,
    TopLevel,
    Skip,
}

impl Mode {
    pub(super) fn from_settings(settings: &Value, project_id: &str) -> Option<Self> {
        Self::from_value(&settings["projectSettingsOverrides"][project_id]["worktreeSubmodules"])
            .or_else(|| Self::from_value(&settings["worktreeSubmodules"]))
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "recursive" => Some(Self::Recursive),
            "top-level" => Some(Self::TopLevel),
            "none" => Some(Self::Skip),
            _ => None,
        }
    }
}

pub(super) async fn in_head(path: &Path) -> bool {
    git::metadata(path, &["cat-file", "-e", "HEAD:.gitmodules"])
        .await
        .is_ok_and(|output| output.status.success())
}

pub(super) async fn prepare(
    path: &Path,
    configured: Option<Mode>,
    cancelled: &mut watch::Receiver<bool>,
    progress: Option<&Progress>,
) -> io::Result<()> {
    // Sparse-checkout may exclude both metadata files from the working tree.
    if !in_head(path).await {
        return Ok(());
    }

    let (mode, source) = if let Some(mode) = configured {
        (mode, "settings")
    } else {
        (file_mode(path).await.unwrap_or(Mode::Recursive), "t3.json")
    };
    if matches!(mode, Mode::Skip) {
        if let Some(progress) = progress {
            progress.stage(
                StageId::Submodules,
                Status::Skipped,
                Some(&format!("disabled in {source}")),
            );
        }
        return Ok(());
    }

    let last_clone_progress = Mutex::new(None);
    let observe = |line: &str| {
        if let Some(progress) = progress {
            report_line(progress, &last_clone_progress, line);
        }
    };
    let lines = progress.map(|_| &observe as &git::LineObserver<'_>);
    if let Some(progress) = progress {
        progress.stage(StageId::Checkout, Status::Done, None);
        progress.stage(StageId::Submodules, Status::Running, None);
    }

    let result = match init(path, cancelled).await {
        Ok(()) => update(path, mode, &[], cancelled, lines).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => {
            if let Some(progress) = progress {
                progress.stage(StageId::Submodules, Status::Done, None);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
        Err(error) => {
            eprintln!("Worktree submodule checkout failed: {error}");
            // A failed clone can stop Git before checking out the modules it already cloned.
            // Force checkout only for registered paths that actually contain a submodule.
            if let Err(retry_error) = retry_cloned(path, mode, cancelled, lines).await {
                if retry_error.kind() == io::ErrorKind::Interrupted {
                    return Err(retry_error);
                }
                eprintln!("Could not check out already cloned submodules: {retry_error}");
            }
            if let Some(progress) = progress {
                progress.stage(
                    StageId::Submodules,
                    Status::Warning,
                    Some(&error.to_string()),
                );
            }
        }
    }
    Ok(())
}

async fn file_mode(path: &Path) -> Option<Mode> {
    let output = git::metadata(path, &["show", "HEAD:t3.json"]).await.ok()?;
    if !output.status.success() {
        return None;
    }
    let source = String::from_utf8_lossy(&output.stdout);
    let file = super::super::super::project_file::parse(&source);
    let Some(file) = file else {
        eprintln!(
            "Invalid t3.json in {}; initializing submodules recursively",
            path.display()
        );
        return None;
    };
    file.get("worktreeSubmodules").and_then(Mode::from_value)
}

async fn init(path: &Path, cancelled: &mut watch::Receiver<bool>) -> io::Result<()> {
    // Only URL registration mutates the repository's shared Git config.
    let _lock = tokio::select! {
        biased;
        _ = cancelled.wait_for(|cancelled| *cancelled) => return Err(io::ErrorKind::Interrupted.into()),
        lock = git::lock_config(path) => lock?,
    };
    run(
        path,
        "initSubmodules",
        &["submodule", "init"],
        cancelled,
        None,
    )
    .await
    .map(|_| ())
}

async fn update(
    path: &Path,
    mode: Mode,
    cloned: &[&str],
    cancelled: &mut watch::Receiver<bool>,
    lines: Option<&git::LineObserver<'_>>,
) -> io::Result<()> {
    let mut args = vec!["submodule", "update", "--init"];
    if matches!(mode, Mode::Recursive) {
        args.push("--recursive");
    }
    if lines.is_some() {
        args.push("--progress");
    }
    if !cloned.is_empty() {
        args.extend(["--force", "--"]);
        args.extend_from_slice(cloned);
    }
    run(path, "updateSubmodules", &args, cancelled, lines)
        .await
        .map(|_| ())
}

async fn retry_cloned(
    path: &Path,
    mode: Mode,
    cancelled: &mut watch::Receiver<bool>,
    lines: Option<&git::LineObserver<'_>>,
) -> io::Result<()> {
    let result = run(
        path,
        "listSubmodules",
        &[
            "config",
            "-z",
            "--blob",
            "HEAD:.gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
        cancelled,
        None,
    )
    .await?;
    let records = String::from_utf8_lossy(&result.stdout);
    let root = path.canonicalize()?;
    let cloned: Vec<_> = records
        .split_terminator('\0')
        .filter_map(|entry| entry.split_once('\n').map(|(_, module_path)| module_path))
        .filter(|module_path| {
            let relative = Path::new(module_path);
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                && root
                    .join(relative)
                    .canonicalize()
                    .is_ok_and(|dir| dir.starts_with(&root) && dir.join(".git").exists())
        })
        .collect();
    if !cloned.is_empty() {
        update(path, mode, &cloned, cancelled, lines).await?;
    }
    Ok(())
}

async fn run(
    path: &Path,
    operation: &str,
    args: &[&str],
    cancelled: &mut watch::Receiver<bool>,
    lines: Option<&git::LineObserver<'_>>,
) -> io::Result<git::Output> {
    let output = git::submodule_output(path, args, cancelled, lines).await?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (detail, reason) = failure_detail(&stderr);
    Err(io::Error::other(format!(
        "Git command failed in GitVcsDriver.createWorktree.{operation} ({}): {detail}{reason}",
        path.display()
    )))
}

fn failure_detail(stderr: &str) -> (&'static str, &'static str) {
    const GENERIC: &str = "Git command exited with a non-zero status.";
    // Git diagnostics can include URL credentials. Use them only to classify fixed reasons.
    let mut reasons = [false; 4];
    for line in stderr.lines().filter(|line| !line.starts_with("remote:")) {
        let line = line.to_ascii_lowercase();
        reasons[0] |= line.starts_with("error: could not lock config file ")
            && line.ends_with(": file exists");
        reasons[1] |= [
            "authentication failed",
            "could not read username",
            "could not read password",
            "permission denied (publickey",
            "permission denied (password",
            "permission denied (keyboard-interactive",
            "permission denied, please try again",
        ]
        .iter()
        .any(|refusal| line.contains(refusal));
        reasons[2] |= line.contains("host key verification failed");
        reasons[3] |= line.starts_with("fatal:")
            && (line.contains("does not appear to be a git repository")
                || line.contains("could not read from remote repository")
                || (line.contains("repository ") && line.contains(" not found")));
    }
    match reasons.iter().position(|present| *present) {
        Some(0) => (
            "Another Git process kept the repository's config locked. Wait for it to finish, then retry.",
            " (config_locked)",
        ),
        Some(1) => (GENERIC, " (authentication_failed)"),
        Some(2) => (GENERIC, " (host_key_unverified)"),
        Some(3) => (GENERIC, " (remote_unreachable)"),
        _ => (GENERIC, ""),
    }
}

fn report_line(progress: &Progress, last_clone_progress: &Mutex<Option<Instant>>, line: &str) {
    let line = line.trim();
    if let Some((module_path, _)) = line
        .strip_prefix("Submodule path '")
        .and_then(|line| line.rsplit_once("': "))
        && !module_path.is_empty()
    {
        progress.submodule_detail(module_path);
        return;
    }

    const CLONE_PREFIXES: &[&str] = &[
        "remote: Enumerating objects",
        "remote: Counting objects",
        "remote: Compressing objects",
        "Receiving objects",
        "Resolving deltas",
        "Updating files",
        "Checking out files",
        "Filtering content",
    ];
    if CLONE_PREFIXES.iter().any(|prefix| line.starts_with(prefix)) {
        let mut last = last_clone_progress.lock().unwrap();
        let now = Instant::now();
        if last.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1)) {
            *last = Some(now);
            progress.submodule_detail(&line.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
}
