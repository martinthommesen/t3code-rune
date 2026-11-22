mod create;
mod diagnostic;
mod git;
mod hidden_changes;
mod idle;
mod measure_worktree;
mod remove_worktree;
mod restore_worktree;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::thread_commands::trim_js;
pub(super) use create::{CreateRefInput, create_ref};
use diagnostic::FailureReason;
pub(super) use diagnostic::GitCommandError;
pub(super) use measure_worktree::measure_worktree;
pub(super) use remove_worktree::{
    AutomaticInspectionError, RemoveWorktreeInput, inspect_automatic, remove_automatic,
    remove_worktree,
};
pub(super) use restore_worktree::{restore_existing_branch, worktree_can_restore};

const SERVICE: &str = "GitWorkflowService.switchRef";
const DRIVER: &str = "GitVcsDriver.switchRef";
const CHECKOUT: &str = "GitVcsDriver.switchRef.checkout";
const CREATE_TRACKING: &str = "GitVcsDriver.switchRef.createTrackingBranch";
const SET_UPSTREAM: &str = "GitVcsDriver.switchRef.trackRemote";
const CLEAN_UNTRACKED: &str = "GitVcsDriver.switchRef.removeUntrackedBranch";
const HEADS: &str = "refs/heads/";
const REMOTES: &str = "refs/remotes/";
type Leases = Mutex<BTreeMap<PathBuf, Weak<AsyncMutex<()>>>>;
static CHECKOUT_LEASES: Leases = Mutex::new(BTreeMap::new());
static CONFIG_LEASES: Leases = Mutex::new(BTreeMap::new());

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SwitchRefInput {
    pub(super) cwd: String,
    ref_name: String,
}

impl SwitchRefInput {
    pub(super) fn parse(payload: Value) -> Result<Self, String> {
        let mut input: Self = serde_json::from_value(payload)
            .map_err(|_| "A Git working directory and reference name are required.".to_owned())?;
        input.cwd = trim_js(&input.cwd).to_owned();
        input.ref_name = trim_js(&input.ref_name).to_owned();
        if input.cwd.is_empty() || input.ref_name.is_empty() {
            return Err("A Git working directory and reference name are required.".into());
        }
        Ok(input)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SwitchRefResult {
    pub(super) ref_name: Option<String>,
}

pub(super) async fn switch_ref(
    database: &Path,
    input: SwitchRefInput,
) -> Result<SwitchRefResult, GitCommandError> {
    let database = database.to_owned();
    let cwd = input.cwd.clone();
    // The checkout must finish if its WebSocket request is dropped.
    tokio::spawn(async move { switch(&database, input).await })
        .await
        .map_err(|_| {
            GitCommandError::new(SERVICE, "git checkout", &cwd, "Checkout task stopped.")
        })?
}

async fn switch(
    database: &Path,
    input: SwitchRefInput,
) -> Result<SwitchRefResult, GitCommandError> {
    let cwd = input.cwd.as_str();
    let root = git::repository_root(Path::new(cwd))
        .await
        .ok_or_else(|| GitCommandError::not_a_repository(SERVICE, cwd))?;
    let _checkout = checkout_lease(&root).await;
    idle::assert_idle(SERVICE, database, cwd, &root).await?;
    switch_in_checkout(cwd, &input.ref_name).await
}

async fn switch_in_checkout(cwd: &str, ref_name: &str) -> Result<SwitchRefResult, GitCommandError> {
    let git = git::Git::new(cwd);
    let target = Target::resolve(git, ref_name).await?;
    let name = target.name();
    if name != "-" && name.starts_with('-') {
        return Err(GitCommandError::new(
            DRIVER,
            "git",
            cwd,
            "Ref names cannot start with '-'.",
        ));
    }
    if let Target::NewTracking { local, upstream } = &target {
        create_tracking(git, local, upstream).await?;
    }
    let args = match target {
        Target::ConflictingRemote(_) => vec!["checkout", "--track", name, "--"],
        _ => vec!["checkout", name, "--"],
    };
    let _config = if matches!(target, Target::ConflictingRemote(_) | Target::Revision(_)) {
        git.config_lease().await
    } else {
        None
    };
    let checkout = git.checkout(&args).await?;
    if !checkout.status.success() {
        return Err(diagnostic::checkout_failure(cwd, &args, &checkout));
    }
    let output = git
        .checked(
            "GitVcsDriver.switchRef.currentBranch",
            &["branch", "--show-current"],
            None,
        )
        .await?;
    let ref_name = trim_js(&output.stdout);
    Ok(SwitchRefResult {
        ref_name: (!ref_name.is_empty()).then(|| ref_name.to_owned()),
    })
}

enum Target<'a> {
    Local(String),
    NewTracking { local: &'a str, upstream: String },
    ConflictingRemote(&'a str),
    Revision(&'a str),
}

impl<'a> Target<'a> {
    async fn resolve(git: git::Git<'_>, requested: &'a str) -> Result<Self, GitCommandError> {
        let local_ref = format!("{HEADS}{requested}");
        let upstream = format!("{REMOTES}{requested}");
        let (local, remote) = tokio::try_join!(
            git.has_ref("GitVcsDriver.switchRef.localInputExists", &local_ref),
            git.has_ref("GitVcsDriver.switchRef.remoteExists", &upstream),
        )?;
        if local {
            return Ok(Self::Local(requested.to_owned()));
        }
        if !remote {
            return Ok(Self::Revision(requested));
        }

        let tracking = git
            .probe(
                "GitVcsDriver.switchRef.localTrackingBranch",
                &[
                    "for-each-ref",
                    "--format=%(refname:lstrip=2)%09%(upstream)",
                    "refs/heads",
                ],
            )
            .await?;
        if tracking.status.success()
            && let Some(local) = tracking.stdout.lines().find_map(|line| {
                let (name, tracked) = line.split_once('\t')?;
                (tracked == upstream && !name.is_empty()).then_some(name)
            })
        {
            return Ok(Self::Local(local.to_owned()));
        }
        if let Some((remote, local)) = requested.split_once('/')
            && !remote.is_empty()
            && !local.is_empty()
            && !git
                .has_ref(
                    "GitVcsDriver.switchRef.localTrackedBranchTargetExists",
                    &format!("{HEADS}{local}"),
                )
                .await?
        {
            return Ok(Self::NewTracking { local, upstream });
        }
        Ok(Self::ConflictingRemote(requested))
    }

    fn name(&self) -> &str {
        match self {
            Self::Local(name) => name,
            Self::NewTracking { local, .. } => local,
            Self::ConflictingRemote(name) | Self::Revision(name) => name,
        }
    }
}

async fn create_tracking(
    git: git::Git<'_>,
    local: &str,
    upstream: &str,
) -> Result<(), GitCommandError> {
    let known_tip = git
        .checked(CREATE_TRACKING, &["rev-parse", "--verify", upstream], None)
        .await?;
    git.checked(
        CREATE_TRACKING,
        &["branch", "--no-track", "--", local, upstream],
        Some("git branch create failed"),
    )
    .await?;
    let _config = git.config_lease().await;
    let setting = format!("--set-upstream-to={upstream}");
    let args = ["branch", &setting, "--", local];
    let mut retries = 0;
    loop {
        match git.checked(SET_UPSTREAM, &args, None).await {
            Ok(_) => return Ok(()),
            Err(error) if error.reason() == Some(FailureReason::ConfigLocked) && retries < 10 => {
                retries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                let _ = git
                    .probe(
                        CLEAN_UNTRACKED,
                        &[
                            "update-ref",
                            "-d",
                            &format!("{HEADS}{local}"),
                            trim_js(&known_tip.stdout),
                        ],
                    )
                    .await;
                return Err(error);
            }
        }
    }
}

async fn lease(registry: &Leases, path: &Path) -> OwnedMutexGuard<()> {
    let lock = {
        let mut locks = registry.lock().unwrap();
        locks.retain(|_, value| value.strong_count() != 0);
        if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(AsyncMutex::new(()));
            locks.insert(path.to_owned(), Arc::downgrade(&lock));
            lock
        }
    };
    lock.lock_owned().await
}

pub(super) async fn checkout_lease(root: &Path) -> OwnedMutexGuard<()> {
    let lease_root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    lease(&CHECKOUT_LEASES, &lease_root).await
}

#[cfg(test)]
mod tests;
