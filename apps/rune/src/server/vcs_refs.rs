use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use super::git_checkpoint::{CheckpointError, ReadOutput, read_command};
use super::locale::path_collator;

const HEADS: &str = "refs/heads/";
const REMOTES: &str = "refs/remotes/";
const ORIGIN: &str = "origin";
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Input {
    cwd: String,
    query: Option<String>,
    cursor: u64,
    limit: usize,
    kind: RefKind,
    include_matching_remote_refs: bool,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RefKind {
    #[default]
    All,
    Local,
    Remote,
}

impl Input {
    pub(super) fn parse(payload: Value) -> Result<Self, String> {
        let fields = payload
            .as_object()
            .ok_or("A Git reference request must be an object.")?;
        let cwd = read_text(fields, "cwd")?.ok_or("A Git working directory is required.")?;
        let query = read_text(fields, "query")?;
        if query
            .as_ref()
            .is_some_and(|value| value.encode_utf16().count() > 256)
        {
            return Err("The Git reference query must be at most 256 characters.".into());
        }
        let cursor = read_integer(fields, "cursor")?.unwrap_or(0);
        let limit = read_integer(fields, "limit")?.unwrap_or(100);
        if !(1..=200).contains(&limit) {
            return Err("The Git reference limit must be between 1 and 200.".into());
        }
        // A reference read always probes the repository's current contents.
        field::<bool>(fields, "refresh")?;
        Ok(Self {
            cwd,
            query,
            cursor,
            limit: limit as usize,
            kind: field(fields, "refKind")?.unwrap_or_default(),
            include_matching_remote_refs: field(fields, "includeMatchingRemoteRefs")?
                .unwrap_or_default(),
        })
    }
}

fn field<T: DeserializeOwned>(
    fields: &Map<String, Value>,
    name: &str,
) -> Result<Option<T>, String> {
    // The RPC JSON codec also accepts null for omitted optional fields.
    fields
        .get(name)
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()).map_err(|_| format!("Invalid {name}.")))
        .transpose()
}

fn read_text(fields: &Map<String, Value>, name: &str) -> Result<Option<String>, String> {
    field::<String>(fields, name)?
        .map(|value| {
            let value = trim_text(&value);
            if value.is_empty() {
                Err(format!("Invalid {name}."))
            } else {
                Ok(value.to_owned())
            }
        })
        .transpose()
}

fn trim_text(value: &str) -> &str {
    value.trim_matches(|character: char| {
        (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
    })
}

fn read_integer(fields: &Map<String, Value>, name: &str) -> Result<Option<u64>, String> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    field::<f64>(fields, name)?
        .map(|value| {
            if value.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER).contains(&value) {
                Ok(value as u64)
            } else {
                Err(format!("Invalid {name}."))
            }
        })
        .transpose()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListRefs {
    refs: Vec<GitRef>,
    is_repo: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_at_repository_root: Option<bool>,
    has_primary_remote: bool,
    next_cursor: Option<usize>,
    total_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GitRef {
    name: String,
    current: bool,
    is_remote: bool,
    is_default: bool,
    worktree_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_ref: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct GitCommandError(Box<CommandDiagnostic>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandDiagnostic {
    #[serde(rename = "_tag")]
    tag: &'static str,
    operation: &'static str,
    command: &'static str,
    cwd: String,
    argument_count: usize,
    detail: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_length: Option<usize>,
    #[serde(skip)]
    executable_missing: bool,
}

struct Checkout {
    common_dir: PathBuf,
    root: PathBuf,
    at_root: Option<bool>,
}

pub(super) async fn list(input: Input) -> Result<ListRefs, GitCommandError> {
    let cwd = Path::new(&input.cwd);
    let Some(checkout) = checkout(cwd).await? else {
        return Ok(ListRefs {
            refs: Vec::new(),
            is_repo: false,
            is_at_repository_root: None,
            has_primary_remote: false,
            next_cursor: None,
            total_count: 0,
        });
    };
    let (snapshot, current) = tokio::join!(snapshot(&checkout.common_dir), current_branch(cwd));
    Ok(snapshot?.page(input, checkout, current.as_deref()))
}

async fn checkout(cwd: &Path) -> Result<Option<Checkout>, GitCommandError> {
    match cwd.metadata() {
        Ok(metadata) if !metadata.is_dir() => return Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        _ => (),
    }
    let operation = "GitVcsDriver.detectRepository";
    let args = [
        "rev-parse",
        "--is-inside-work-tree",
        "--show-toplevel",
        "--git-common-dir",
    ];
    let layout = match git(operation, cwd, &args, SHORT_TIMEOUT).await {
        Err(error) if error.0.executable_missing => return Ok(None),
        result => result?,
    };
    if !layout.status.success() {
        return Ok(None);
    }
    let mut lines = strip_line_feed(&layout.stdout).split('\n');
    if lines.next().map(trim_text) != Some("true") {
        return Ok(None);
    }
    let (common, root) = match (lines.next(), lines.next(), lines.next()) {
        (Some(root), Some(common), None) if !common.is_empty() && !root.is_empty() => {
            (PathBuf::from(common), PathBuf::from(root))
        }
        _ => tokio::try_join!(
            git_path(
                cwd,
                "--git-common-dir",
                "GitVcsDriver.resolveRepositoryPaths.commonDir"
            ),
            git_path(
                cwd,
                "--show-toplevel",
                "GitVcsDriver.resolveRepositoryPaths.worktreeRoot"
            ),
        )?,
    };
    let physical_cwd = cwd.canonicalize().ok().map(strip_windows_prefix);
    let common_dir = physical_cwd.as_deref().unwrap_or(cwd).join(common);
    let common_dir = common_dir
        .canonicalize()
        .map(strip_windows_prefix)
        .unwrap_or(common_dir);
    let root: PathBuf = cwd.join(root).components().collect();
    let at_root = physical_cwd.as_ref().map(|cwd| &root == cwd);
    Ok(Some(Checkout {
        common_dir,
        root,
        at_root,
    }))
}

fn strip_windows_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{share}"));
        }
        if let Some(path) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(path);
        }
    }
    path
}

fn strip_line_feed(value: &str) -> &str {
    value.strip_suffix('\n').unwrap_or(value)
}

async fn git_path(
    cwd: &Path,
    flag: &str,
    operation: &'static str,
) -> Result<PathBuf, GitCommandError> {
    let args = ["rev-parse", flag];
    let output = git(operation, cwd, &args, SHORT_TIMEOUT).await?;
    let path = strip_line_feed(&output.stdout);
    if output.status.success() && !path.is_empty() {
        Ok(PathBuf::from(path))
    } else {
        Err(command_error(
            operation,
            cwd,
            args.len(),
            "Failed to resolve the Git repository path.",
            Some(&output),
        ))
    }
}

async fn current_branch(cwd: &Path) -> Option<String> {
    git(
        "GitVcsDriver.listRefs.currentBranch",
        cwd,
        &["symbolic-ref", "--quiet", "HEAD"],
        SHORT_TIMEOUT,
    )
    .await
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| {
        trim_text(&output.stdout)
            .strip_prefix(HEADS)
            .map(str::to_owned)
    })
}

struct Snapshot {
    refs: Vec<GitRef>,
    has_primary_remote: bool,
}

async fn snapshot(common: &Path) -> Result<Snapshot, GitCommandError> {
    let cwd = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().unwrap_or(common)
    } else {
        common
    };
    let dir = common.to_string_lossy();
    let worktree_args = ["--git-dir", &dir, "worktree", "list", "--porcelain", "-z"];
    let mut worktrees = git(
        "GitVcsDriver.readWorktreeBranchPaths",
        cwd,
        &worktree_args,
        LIST_TIMEOUT,
    )
    .await?;
    if !worktrees.status.success() {
        worktrees = git(
            "GitVcsDriver.readWorktreeBranchPaths",
            cwd,
            &worktree_args[..worktree_args.len() - 1],
            LIST_TIMEOUT,
        )
        .await?;
        worktrees.stdout = worktrees.stdout.replace('\n', "\0");
    }
    let worktrees = if worktrees.status.success() {
        worktree_branches(&worktrees.stdout, common)
    } else {
        HashMap::new()
    };
    let ref_operation = "GitVcsDriver.listRefs.snapshotRefs";
    let ref_args = [
        "--git-dir",
        &dir,
        "for-each-ref",
        "--format=%(refname)%09%(committerdate:unix)%09%(symref)%09%(upstream)",
        "refs/heads",
        "refs/remotes",
    ];
    let default_args = [
        "--git-dir",
        &dir,
        "rev-parse",
        "--verify",
        "--quiet",
        "--symbolic-full-name",
        "refs/remotes/origin/HEAD",
    ];
    let remote_args = ["--git-dir", &dir, "remote"];
    let (refs, default, remotes) = tokio::try_join!(
        git(ref_operation, cwd, &ref_args, LIST_TIMEOUT),
        git(
            "GitVcsDriver.listRefs.defaultRef",
            cwd,
            &default_args,
            SHORT_TIMEOUT
        ),
        git(
            "GitVcsDriver.listRefs.remoteNames",
            cwd,
            &remote_args,
            SHORT_TIMEOUT
        ),
    )?;
    if !refs.status.success() {
        return Err(command_error(
            ref_operation,
            cwd,
            ref_args.len(),
            "Git ref snapshot enumeration failed.",
            Some(&refs),
        ));
    }
    let default = default
        .status
        .success()
        .then(|| trim_text(&default.stdout))
        .and_then(|reference| reference.strip_prefix("refs/remotes/origin/"))
        .map(trim_text)
        .filter(|name| !name.is_empty() && *name != "HEAD");
    let mut remote_names: Vec<_> = if remotes.status.success() {
        remotes
            .stdout
            .split('\n')
            .map(trim_text)
            .filter(|name| !name.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    remote_names.sort_by_cached_key(|name| std::cmp::Reverse(name.encode_utf16().count()));
    Ok(Snapshot {
        refs: parse_refs(&refs.stdout, default, &remote_names, &worktrees),
        has_primary_remote: !remote_names.is_empty(),
    })
}

pub(super) fn worktree_branches(output: &str, common: &Path) -> HashMap<String, String> {
    let mut paths = HashMap::new();
    for entry in output.split("\0\0") {
        let (mut branch, mut path) = (None, None);
        for field in entry.split('\0') {
            if field == "prunable" || field.starts_with("prunable ") {
                branch = None;
                break;
            }
            if let Some(value) = field.strip_prefix("worktree ") {
                path = Some(value);
            } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
                branch = Some(value);
            }
        }
        if let Some((branch, path)) = branch.zip(path) {
            let path: PathBuf = Path::new(path).components().collect();
            if path != common && path.exists() {
                paths.insert(branch.to_owned(), path.to_string_lossy().into_owned());
            }
        }
    }
    paths
}

fn parse_refs(
    output: &str,
    default: Option<&str>,
    remotes: &[&str],
    worktrees: &HashMap<String, String>,
) -> Vec<GitRef> {
    let mut refs = Vec::new();
    for line in output.split('\n') {
        let mut columns = line.split('\t');
        let (Some(full), Some(date), Some(""), Some(upstream)) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            continue;
        };
        let (name, remote) = if let Some(name) = full.strip_prefix(HEADS) {
            (name, None)
        } else if let Some(name) = full.strip_prefix(REMOTES) {
            (
                name,
                Some(remotes.iter().find_map(|remote| {
                    let branch = trim_text(name).strip_prefix(remote)?.strip_prefix('/')?;
                    (!trim_text(branch).is_empty()).then_some((*remote, trim_text(branch)))
                })),
            )
        } else {
            continue;
        };
        let is_remote = remote.is_some();
        let (remote_name, remote_branch) = remote.flatten().unzip();
        refs.push((
            GitRef {
                name: name.to_owned(),
                current: false,
                is_remote,
                is_default: if is_remote {
                    remote_name == Some(ORIGIN) && remote_branch == default
                } else {
                    default == Some(name)
                },
                worktree_path: if is_remote {
                    None
                } else {
                    worktrees.get(name).cloned()
                },
                remote_name: remote_name.map(str::to_owned),
                upstream_ref: (!is_remote && !upstream.is_empty()).then(|| upstream.to_owned()),
            },
            date.parse::<i64>().unwrap_or_default(),
        ));
    }
    let collator = path_collator();
    refs.sort_by(|(left, left_date), (right, right_date)| {
        left.is_remote
            .cmp(&right.is_remote)
            .then_with(|| right_date.cmp(left_date))
            .then_with(|| {
                collator.as_ref().map_or_else(
                    || left.name.cmp(&right.name),
                    |collator| collator.compare(&left.name, &right.name),
                )
            })
    });
    refs.into_iter().map(|(reference, _)| reference).collect()
}

impl Snapshot {
    fn page(mut self, input: Input, checkout: Checkout, current: Option<&str>) -> ListRefs {
        let root = checkout.root.to_string_lossy();
        for reference in self
            .refs
            .iter_mut()
            .filter(|reference| !reference.is_remote)
        {
            reference.current = current == Some(reference.name.as_str());
            if reference.current {
                reference.worktree_path = Some(root.to_string());
            } else if reference.worktree_path.as_deref() == Some(root.as_ref()) {
                reference.worktree_path = None;
            }
        }
        if !input.include_matching_remote_refs {
            let local: HashSet<_> = self
                .refs
                .iter()
                .filter(|reference| !reference.is_remote)
                .map(|reference| reference.name.clone())
                .collect();
            self.refs.retain(|reference| {
                reference.remote_name.as_deref() != Some(ORIGIN)
                    || !reference
                        .name
                        .split_once('/')
                        .is_some_and(|(_, branch)| local.contains(branch))
            });
        }
        self.refs.sort_by_key(|reference| {
            if reference.current {
                0
            } else if reference.is_default {
                1
            } else {
                2
            }
        });
        let query = input.query.map(|query| query.to_lowercase());
        let refs: Vec<_> = self
            .refs
            .into_iter()
            .filter(|reference| {
                (match input.kind {
                    RefKind::All => true,
                    RefKind::Local => !reference.is_remote,
                    RefKind::Remote => reference.is_remote,
                }) && query
                    .as_deref()
                    .is_none_or(|query| reference.name.to_lowercase().contains(query))
            })
            .collect();
        let total_count = refs.len();
        let cursor = usize::try_from(input.cursor).unwrap_or(usize::MAX);
        let refs: Vec<_> = refs
            .into_iter()
            .skip(cursor)
            .take(input.limit)
            .map(|mut reference| {
                reference.name = trim_text(&reference.name).to_owned();
                reference.worktree_path = reference
                    .worktree_path
                    .map(|path| trim_text(&path).to_owned());
                reference.upstream_ref = reference
                    .upstream_ref
                    .map(|upstream| trim_text(&upstream).to_owned());
                reference
            })
            .collect();
        let end = cursor + refs.len();
        ListRefs {
            refs,
            is_repo: true,
            is_at_repository_root: checkout.at_root,
            has_primary_remote: self.has_primary_remote,
            next_cursor: (end < total_count).then_some(end),
            total_count,
        }
    }
}

async fn git(
    operation: &'static str,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<ReadOutput, GitCommandError> {
    match tokio::time::timeout(timeout, read_command(cwd, args)).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            let mut failure = command_error(
                operation,
                cwd,
                args.len(),
                match &error {
                    CheckpointError::Timeout(_) => "Git command timed out.",
                    CheckpointError::Io(io) if io.kind() == io::ErrorKind::InvalidData => {
                        "Git command output exceeded its limit."
                    }
                    _ => "Failed to run Git process.",
                },
                None,
            );
            failure.0.executable_missing =
                matches!(error, CheckpointError::Io(io) if io.kind() == io::ErrorKind::NotFound);
            Err(failure)
        }
        Err(_) => Err(command_error(
            operation,
            cwd,
            args.len(),
            "Git command timed out.",
            None,
        )),
    }
}

fn command_error(
    operation: &'static str,
    cwd: &Path,
    argument_count: usize,
    detail: &'static str,
    output: Option<&ReadOutput>,
) -> GitCommandError {
    GitCommandError(Box::new(CommandDiagnostic {
        tag: "GitCommandError",
        operation,
        command: "git",
        cwd: cwd.to_string_lossy().into_owned(),
        argument_count,
        detail,
        exit_code: output.and_then(|output| output.status.code()),
        stdout_length: output.map(|output| output.stdout.encode_utf16().count()),
        stderr_length: output.map(|output| output.stderr.encode_utf16().count()),
        executable_missing: false,
    }))
}
