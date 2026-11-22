mod ancestry;
mod diff;
mod git;
mod objects;
mod sparse;

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest};

#[cfg(test)]
pub(super) use self::diff::CheckpointFile;
pub(super) use self::diff::{DiffInput, summarize};
use self::git::{Git, Output};
use self::objects::Scratch;

const REFS_PREFIX: &str = "refs/t3/orchestration-v2/checkpoints/";
const NESTED_REPOSITORY_LIMIT: usize = 64;
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct ReadOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

pub(super) async fn read_command(cwd: &Path, args: &[&str]) -> Result<ReadOutput, CheckpointError> {
    Ok(Git::new(cwd).run(args, None).await?.into())
}

impl From<Output> for ReadOutput {
    fn from(output: Output) -> Self {
        Self {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

pub(super) struct StatusIndex {
    scratch: Scratch,
    _directory: tempfile::TempDir,
}

impl StatusIndex {
    pub(super) fn copy(common: &Path, source: &Path) -> io::Result<Self> {
        let directory = tempfile::tempdir()?;
        let scratch = Scratch::isolated(directory.path(), common)?;
        match std::fs::copy(source, scratch.index()) {
            Ok(_) => {
                let modified = source.metadata()?.modified()?;
                let seconds = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let timestamp = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(scratch.index())?
                    .set_modified(timestamp)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        Ok(Self {
            scratch,
            _directory: directory,
        })
    }

    pub(super) async fn run(
        &self,
        cwd: &Path,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<ReadOutput, CheckpointError> {
        Ok(Git::new(cwd)
            .scratch(&self.scratch)
            .run(args, input)
            .await?
            .into())
    }
}

pub(super) struct CaptureInput<'a> {
    pub(super) cwd: &'a Path,
    pub(super) checkpoint_ref: &'a str,
    pub(super) previous_checkpoint_ref: Option<&'a str>,
    pub(super) exclude_paths: &'a [&'a str],
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CapturedCheckpoint {
    Created,
    Existing,
}

#[derive(Debug)]
pub(super) enum CheckpointError {
    Io(io::Error),
    Git {
        operation: String,
        status: ExitStatus,
        message: String,
    },
    Timeout(String),
    InvalidReference,
    InvalidOutput(&'static str),
    InvalidExclusion,
    UnsupportedSparseCheckout,
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "Checkpoint storage failed: {error}"),
            Self::Git {
                operation,
                status,
                message,
            } => write!(formatter, "git {operation} failed ({status}): {message}"),
            Self::Timeout(operation) => write!(
                formatter,
                "git {operation} timed out while capturing a checkpoint."
            ),
            Self::InvalidReference => formatter.write_str("Invalid T3 checkpoint ref."),
            Self::InvalidOutput(message) => {
                write!(formatter, "Invalid Git checkpoint data: {message}")
            }
            Self::InvalidExclusion => {
                formatter.write_str("Checkpoint exclusions must be relative workspace paths.")
            }
            Self::UnsupportedSparseCheckout => formatter
                .write_str("Cannot rebuild a checkpoint index for non-cone sparse checkout."),
        }
    }
}

impl Error for CheckpointError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for CheckpointError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(super) fn checkpoint_ref(scope_id: &str, ordinal: u64) -> String {
    let hash = digest(&SHA256, scope_id.as_bytes());
    let mut scope_key = String::with_capacity(32);
    for byte in &hash.as_ref()[..16] {
        write!(scope_key, "{byte:02x}").expect("writing a string cannot fail");
    }
    format!(
        "{REFS_PREFIX}{}/ordinal/{ordinal}",
        URL_SAFE_NO_PAD.encode(scope_key)
    )
}

pub(super) async fn is_workspace(cwd: &Path) -> Result<bool, CheckpointError> {
    let probe = Git::new(cwd)
        .run(["rev-parse", "--is-inside-work-tree"], None)
        .await?;
    if !probe.status.success() && probe.stderr.starts_with(b"fatal: not a git repository") {
        return Ok(false);
    }
    Ok(probe.success()?.trim_ascii() == b"true")
}

pub(super) async fn has_ref(cwd: &Path, checkpoint_ref: &str) -> Result<bool, CheckpointError> {
    if !is_workspace(cwd).await? {
        return Ok(false);
    }
    let git = Git::new(cwd);
    validate_ref(git, checkpoint_ref).await?;
    let commit = format!("{checkpoint_ref}^{{commit}}");
    Ok(revision(git, &commit).await?.is_some())
}

pub(super) async fn capture(
    input: CaptureInput<'_>,
) -> Result<Option<CapturedCheckpoint>, CheckpointError> {
    if !is_workspace(input.cwd).await? {
        return Ok(None);
    }
    let git = Git::new(input.cwd);
    validate_ref(git, input.checkpoint_ref).await?;
    if let Some(previous) = input.previous_checkpoint_ref {
        validate_ref(git, previous).await?;
    }
    if let Some(previous) = existing_checkpoint(git, input.checkpoint_ref).await? {
        return Ok(Some(previous));
    }

    let head = revision(git, "HEAD^{commit}").await?;
    let history = ancestry::HeadHistory::read(git, head.as_deref()).await;
    let git_common = git.command(["rev-parse", "--git-common-dir"]).await?;
    let git_common = PathBuf::from(git_path(git_common)?);
    let git_common = if git_common.is_absolute() {
        git_common
    } else {
        input.cwd.canonicalize()?.join(git_common)
    };
    let scratch = Arc::new(Scratch::new(&git_common)?);
    let staging = git.scratch(&scratch);
    let sparse = config_bool(git, "core.sparseCheckout").await?;
    if let Some(head) = head.as_deref() {
        if sparse {
            sparse::prepare_index(git, &scratch, input.cwd, head).await?;
        } else {
            staging.command(["read-tree", head]).await?;
        }
    } else {
        staging.command(["read-tree", "--empty"]).await?;
    }

    let exclusions = exclusion_pathspecs(git, input.exclude_paths).await?;
    let prefix = git.command(["rev-parse", "--show-prefix"]).await?;
    let prefix = git_path(prefix)?;
    let ignored_workspace = if let Some(folder) = Path::new(&prefix).file_name() {
        let relative = Path::new("..").join(folder);
        !ignored(git, &[relative.as_os_str()]).await?.is_empty()
    } else {
        false
    };
    if ignored_workspace && !seed_ignored_workspace(git, staging).await? {
        return Ok(None);
    }
    stage(input.cwd, staging, exclusions, sparse, ignored_workspace).await?;
    let tree = required_oid(staging.command(["write-tree"]).await?)?;

    let turn_start = match input.previous_checkpoint_ref {
        Some(previous) => history
            .turn_start(git, staging, previous, input.checkpoint_ref)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let message = history.checkpoint_message(input.checkpoint_ref);
    let commit = commit_tree(
        staging,
        &tree,
        head.as_deref(),
        turn_start.as_deref(),
        &message,
    )
    .await?;
    let objects = Arc::clone(&scratch);
    tokio::task::spawn_blocking(move || objects.publish())
        .await
        .map_err(|_| CheckpointError::InvalidOutput("object publication was interrupted"))??;

    let absent = "0".repeat(commit.len());
    let published = git
        .run(
            [
                "update-ref",
                "--no-deref",
                input.checkpoint_ref,
                &commit,
                &absent,
            ],
            None,
        )
        .await?;
    if published.status.success() {
        Ok(Some(CapturedCheckpoint::Created))
    } else if let Some(existing) = existing_checkpoint(git, input.checkpoint_ref).await? {
        Ok(Some(existing))
    } else {
        Err(published.error())
    }
}

async fn validate_ref(git: Git<'_>, reference: &str) -> Result<(), CheckpointError> {
    if !reference.starts_with(REFS_PREFIX)
        || !git
            .run(["check-ref-format", reference], None)
            .await?
            .status
            .success()
    {
        return Err(CheckpointError::InvalidReference);
    }
    Ok(())
}

async fn existing_checkpoint(
    git: Git<'_>,
    reference: &str,
) -> Result<Option<CapturedCheckpoint>, CheckpointError> {
    let symbolic = git
        .run(["symbolic-ref", "--quiet", "--no-recurse", reference], None)
        .await?;
    if symbolic.status.success() {
        return Err(CheckpointError::InvalidReference);
    }
    let Some(commit) = revision(git, reference).await? else {
        return Ok(None);
    };
    if git.command(["cat-file", "-t", &commit]).await?.trim_ascii() != b"commit" {
        return Err(CheckpointError::InvalidOutput(
            "checkpoint ref must point to a commit",
        ));
    }
    required_oid(
        git.command(["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
            .await?,
    )?;
    Ok(Some(CapturedCheckpoint::Existing))
}

async fn revision(git: Git<'_>, revision: &str) -> Result<Option<String>, CheckpointError> {
    let result = git
        .run(["rev-parse", "--verify", "--quiet", revision], None)
        .await?;
    if result.status.code() == Some(1) && result.stderr.is_empty() {
        return Ok(None);
    }
    required_oid(result.success()?).map(Some)
}

async fn config_bool(git: Git<'_>, name: &str) -> Result<bool, CheckpointError> {
    let result = git.run(["config", "--bool", name], None).await?;
    if result.status.code() == Some(1) && result.stderr.is_empty() {
        return Ok(false);
    }
    Ok(result.success()?.trim_ascii() == b"true")
}

fn required_oid(output: Vec<u8>) -> Result<String, CheckpointError> {
    let value = String::from_utf8(output)
        .map_err(|_| CheckpointError::InvalidOutput("expected an object ID"))?;
    let oid = value.trim();
    if !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(CheckpointError::InvalidOutput("expected an object ID"));
    }
    Ok(oid.to_owned())
}

fn git_path(mut output: Vec<u8>) -> Result<OsString, CheckpointError> {
    if output.pop() != Some(b'\n') {
        return Err(CheckpointError::InvalidOutput("expected a Git path"));
    }
    #[cfg(windows)]
    if output.last() == Some(&b'\r') {
        output.pop();
    }
    path_bytes(output)
}

#[cfg(unix)]
fn path_bytes(bytes: Vec<u8>) -> Result<OsString, CheckpointError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_bytes(bytes: Vec<u8>) -> Result<OsString, CheckpointError> {
    String::from_utf8(bytes)
        .map(OsString::from)
        .map_err(|_| CheckpointError::InvalidOutput("expected a UTF-8 Git path"))
}

fn exclude_spec(path: &OsStr) -> OsString {
    let mut spec = OsString::from(":(exclude,literal)");
    spec.push(path);
    spec
}

async fn exclusion_pathspecs(
    git: Git<'_>,
    paths: &[&str],
) -> Result<Vec<OsString>, CheckpointError> {
    if paths.iter().any(|path| {
        path.is_empty()
            || path.contains('\0')
            || !Path::new(path)
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
    }) {
        return Err(CheckpointError::InvalidExclusion);
    }
    let paths: Vec<_> = paths.iter().map(OsStr::new).collect();
    let ignored = ignored(git, &paths).await?;
    Ok(paths
        .into_iter()
        .filter(|path| {
            !ignored
                .split(|byte| *byte == 0)
                .any(|entry| entry == path.as_encoded_bytes())
        })
        .map(exclude_spec)
        .collect())
}

async fn ignored(git: Git<'_>, paths: &[&OsStr]) -> Result<Vec<u8>, CheckpointError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_encoded_bytes());
        input.push(0);
    }
    let output = git
        .run(
            ["check-ignore", "--no-index", "-z", "--stdin"],
            Some(&input),
        )
        .await?;
    if output.status.code() == Some(1) {
        Ok(Vec::new())
    } else {
        output.success()
    }
}

async fn seed_ignored_workspace(git: Git<'_>, staging: Git<'_>) -> Result<bool, CheckpointError> {
    let raw = git
        .command([
            "diff",
            "--cached",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            "--no-relative",
            "--ignore-submodules=none",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ita-visible-in-index",
            "--diff-filter=A",
            "-z",
            "--",
            ".",
        ])
        .await?;
    add_index_entries(staging, &raw, false).await?;
    let tracked = staging
        .command(["ls-files", "--cached", "-z", "--", "."])
        .await?;
    Ok(!tracked.is_empty())
}

async fn add_index_entries(
    staging: Git<'_>,
    raw: &[u8],
    skip_gitlinks: bool,
) -> Result<(), CheckpointError> {
    let mut records = raw.split(|byte| *byte == 0);
    let mut input = Vec::new();
    while let Some(header) = records.next().filter(|header| !header.is_empty()) {
        let path = records
            .next()
            .ok_or(CheckpointError::InvalidOutput("missing diff path"))?;
        let fields: Vec<_> = header.split(|byte| *byte == b' ').collect();
        let [old_mode, new_mode, _, new_id, _] = fields[..] else {
            return Err(CheckpointError::InvalidOutput("invalid raw diff record"));
        };
        if skip_gitlinks && (old_mode == b":160000" || new_mode == b"160000") {
            continue;
        }
        input.extend_from_slice(new_mode);
        input.push(b' ');
        input.extend_from_slice(new_id);
        input.push(b'\t');
        input.extend_from_slice(path);
        input.push(0);
    }
    if !input.is_empty() {
        staging
            .run(["update-index", "-z", "--index-info"], Some(&input))
            .await?
            .success()?;
    }
    Ok(())
}

async fn stage(
    cwd: &Path,
    staging: Git<'_>,
    exclusions: Vec<OsString>,
    sparse: bool,
    workspace_ignored: bool,
) -> Result<(), CheckpointError> {
    let attempt = add(staging, &exclusions, sparse, workspace_ignored, false).await?;
    if attempt.status.success() {
        return Ok(());
    }
    match tokio::time::timeout(
        RECOVERY_TIMEOUT,
        recover_stage(cwd, staging, exclusions, sparse, workspace_ignored),
    )
    .await
    {
        Ok(Ok(true)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Ok(Ok(false)) | Err(_) => Err(attempt.error()),
    }
}

async fn recover_stage(
    cwd: &Path,
    staging: Git<'_>,
    mut exclusions: Vec<OsString>,
    sparse: bool,
    workspace_ignored: bool,
) -> Result<bool, CheckpointError> {
    let untracked = staging
        .command([
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .await?;
    let candidates: Vec<_> = untracked
        .split(|byte| *byte == 0)
        .filter(|entry| entry.ends_with(b"/"))
        .collect();
    if candidates.len() > NESTED_REPOSITORY_LIMIT {
        return Ok(false);
    }
    for entry in candidates {
        let path = path_bytes(entry.to_vec())?;
        let nested = cwd.join(&path);
        if nested.join(".git").exists()
            && revision(Git::new(&nested), "HEAD^{commit}")
                .await?
                .is_none()
        {
            exclusions.push(exclude_spec(&path));
        }
    }
    let recovered = add(staging, &exclusions, sparse, workspace_ignored, true).await?;
    if recovered.status.code() == Some(1) {
        Ok(true)
    } else {
        recovered.success().map(|_| true)
    }
}

async fn add(
    staging: Git<'_>,
    exclusions: &[OsString],
    sparse: bool,
    workspace_ignored: bool,
    ignore_errors: bool,
) -> Result<Output, CheckpointError> {
    let mut args = vec![OsStr::new("add")];
    if ignore_errors {
        args.push(OsStr::new("--ignore-errors"));
    }
    if sparse {
        args.push(OsStr::new("--sparse"));
    }
    args.extend([
        OsStr::new(if workspace_ignored { "-u" } else { "-A" }),
        OsStr::new("--"),
        OsStr::new("."),
    ]);
    args.extend(exclusions.iter().map(OsString::as_os_str));
    staging.run(args, None).await
}

async fn commit_tree(
    git: Git<'_>,
    tree: &str,
    first_parent: Option<&str>,
    second_parent: Option<&str>,
    message: &str,
) -> Result<String, CheckpointError> {
    let mut args = vec!["commit-tree", tree];
    for parent in [first_parent, second_parent].into_iter().flatten() {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    required_oid(git.command(args).await?)
}

#[cfg(test)]
mod tests;
