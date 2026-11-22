use std::path::Path;

use serde::Serialize;
use serde_json::Value;

mod git;
mod local;
mod remote;
mod stream;
mod totals;

pub(super) use stream::{Service, Subscription};

pub(super) struct Input {
    pub(super) cwd: String,
    pub(super) include_remote: bool,
}

impl Input {
    pub(super) fn parse(payload: Value) -> Result<Self, String> {
        Ok(Self {
            cwd: parse_cwd(&payload)?,
            include_remote: parse_flag(&payload, "includeRemote", true)?,
        })
    }
}

pub(super) struct RefreshInput {
    pub(super) cwd: String,
    pub(super) local_only: bool,
}

impl RefreshInput {
    pub(super) fn parse(payload: Value) -> Result<Self, String> {
        Ok(Self {
            cwd: parse_cwd(&payload)?,
            local_only: parse_flag(&payload, "localOnly", false)?,
        })
    }
}

fn parse_cwd(payload: &Value) -> Result<String, String> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "A Git working directory is required.".into())
}

fn parse_flag(payload: &Value, key: &str, default: bool) -> Result<bool, String> {
    match payload.get(key) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::Null) | None => Ok(default),
        _ => Err(format!("Invalid {key}.")),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalStatus {
    pub(super) is_repo: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_control_provider: Option<HostingProvider>,
    has_primary_remote: bool,
    is_default_ref: bool,
    pub(super) ref_name: Option<String>,
    has_working_tree_changes: bool,
    working_tree: WorkingTree,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_changes: Option<totals::BranchChanges>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostingProvider {
    kind: &'static str,
    name: String,
    base_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemoteStatus {
    has_upstream: bool,
    ahead_count: u64,
    behind_count: u64,
    ahead_of_default_count: u64,
    pr: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkingTree {
    #[serde(skip_serializing_if = "Option::is_none")]
    root_path: Option<String>,
    files: Vec<WorkingFile>,
    insertions: u64,
    deletions: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct WorkingFile {
    path: String,
    insertions: u64,
    deletions: u64,
}

struct StatusParts {
    local: LocalStatus,
    remote: Option<RemoteStatus>,
}

#[derive(Default)]
struct ReadOptions {
    tracking: bool,
    branch_changes: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RefreshStatus {
    #[serde(flatten)]
    local: LocalStatus,
    #[serde(flatten)]
    remote: RemoteStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    refs_digest: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GitManagerError {
    #[serde(rename = "_tag")]
    tag: &'static str,
    operation: &'static str,
    cwd: String,
    detail: &'static str,
}

impl GitManagerError {
    fn local(cwd: &Path, detail: &'static str) -> Self {
        Self {
            tag: "GitManagerError",
            operation: "GitWorkflowService.localStatus",
            cwd: cwd.to_string_lossy().into_owned(),
            detail,
        }
    }
}

/// Read local porcelain and, on demand, divergence against already-present tracking refs.
async fn read(
    cwd: &Path,
    options: ReadOptions,
) -> Result<(StatusParts, Option<git::Checkout>), GitManagerError> {
    let Some(checkout) = git::checkout(cwd).await? else {
        return Ok((empty(), None));
    };
    let Some(mut status) = local::read(cwd, &checkout).await? else {
        return Ok((empty(), None));
    };
    let remote = if options.tracking {
        Some(remote::tracking(cwd, &status).await)
    } else {
        None
    };
    if options.branch_changes {
        status.local.branch_changes = totals::read(&checkout, &status).await;
    }
    Ok((
        StatusParts {
            local: status.local,
            remote,
        },
        Some(checkout),
    ))
}

fn empty() -> StatusParts {
    StatusParts {
        local: LocalStatus::default(),
        remote: None,
    }
}

pub(super) fn snapshot(local: LocalStatus, remote: Option<RemoteStatus>) -> Value {
    serde_json::json!({"_tag":"snapshot","local":local,"remote":remote})
}

fn trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
    })
}
