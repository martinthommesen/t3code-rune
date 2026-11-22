use std::fmt::Write;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use tokio::sync::watch;

use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

use super::setup_progress::{Progress, StageId, Status};
use super::{LaunchError, LaunchInput, WorkspaceStrategy};

mod base;
mod generation;
mod git;
mod naming;
mod rename;
pub(super) mod rename_events;

pub(super) use generation::{ResultName, name as generate_name};

struct CheckoutConfig {
    workers: String,
    sparse: Option<Vec<u8>>,
    submodules_disabled_by: Option<&'static str>,
}

impl CheckoutConfig {
    async fn read(
        input: &LaunchInput,
        settings_path: Option<&Path>,
        repository: &Path,
        commit: &str,
    ) -> io::Result<Self> {
        let settings = super::workspace::read_settings(settings_path).map_err(io::Error::other)?;
        let file = git::metadata(repository, &["show", &format!("{commit}:t3.json")]).await?;
        let file = file
            .status
            .success()
            .then_some(file.stdout)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|contents| super::super::parse_settings_json(&contents))
            .unwrap_or_default();
        let configured = scoped_setting(&settings, &input.project_id.0, "worktreeSubmodules");
        let submodules = configured
            .as_str()
            .map(|mode| (mode, "settings"))
            .or_else(|| {
                file["worktreeSubmodules"]
                    .as_str()
                    .map(|mode| (mode, "t3.json"))
            });
        let has_submodules = git::metadata(
            repository,
            &["cat-file", "-e", &format!("{commit}:.gitmodules")],
        )
        .await?
        .status
        .success();
        let submodules_disabled_by = submodules
            .filter(|(mode, _)| *mode == "none" && has_submodules)
            .map(|(_, source)| source);
        if has_submodules && submodules_disabled_by.is_none() {
            return Err(io::Error::other(
                "This worktree requires submodule preparation.",
            ));
        }
        let sparse = if let Some(patterns) = file["worktreeSparsePatternsPath"].as_str() {
            let patterns = patterns.trim_start_matches("./");
            if patterns.split('/').any(|part| part == "..") || patterns.starts_with('/') {
                return Err(io::Error::other(
                    "The worktree sparse-checkout pattern path is invalid.",
                ));
            }
            let output = git::metadata(
                repository,
                &["cat-file", "blob", &format!("{commit}:{patterns}")],
            )
            .await?;
            if !output.status.success() {
                return Err(io::Error::other(
                    "The worktree sparse-checkout pattern file is missing.",
                ));
            }
            Some(output.stdout)
        } else {
            None
        };
        let workers = git::metadata(repository, &["config", "--get", "checkout.workers"]).await?;
        let workers = if workers.status.success() {
            String::from_utf8(workers.stdout)
                .map_err(io::Error::other)?
                .trim_end_matches(['\r', '\n'])
                .to_owned()
        } else {
            "0".to_owned()
        };
        Ok(Self {
            workers,
            sparse,
            submodules_disabled_by,
        })
    }
}

fn scoped_setting<'a>(
    settings: &'a serde_json::Value,
    project: &str,
    key: &str,
) -> &'a serde_json::Value {
    settings["projectSettingsOverrides"][project]
        .get(key)
        .unwrap_or(&settings[key])
}

async fn generated_branch(
    input: &LaunchInput,
    settings_path: Option<&Path>,
    repo: &Path,
) -> Result<String, LaunchError> {
    let fail = |error| input.fail("provision-worktree", error);
    let settings = super::workspace::read_settings(settings_path).map_err(fail)?;
    let project = &input.project_id.0;
    let naming = naming::Naming::from_settings(&settings, project);
    // The command identity keeps the temporary branch stable when checkout is retried.
    let token = hash_prefix(&format!("{project}\0{}", input.command_id.0), 4);
    let mut branch = naming.temporary(&token);
    for (position, _) in branch.match_indices('/') {
        let candidate = format!("refs/heads/{}", &branch[..position]);
        if git::metadata(repo, &["show-ref", "--verify", "--quiet", &candidate])
            .await
            .map_err(|_| fail("Local Git branches could not be read."))?
            .status
            .success()
        {
            branch = branch.replace('/', "-");
            break;
        }
    }
    Ok(branch)
}

pub(super) fn auto_naming(strategy: &WorkspaceStrategy) -> bool {
    let WorkspaceStrategy::Worktree { branch, .. } = strategy else {
        return false;
    };
    branch.as_ref().is_none_or(|branch| {
        let branch = branch.0.to_ascii_lowercase();
        if let Some(value) = branch
            .strip_prefix("t3/")
            .or_else(|| branch.strip_prefix("t3-"))
            .or_else(|| branch.strip_prefix("t3code-"))
        {
            return hex_token(value);
        }
        branch.strip_prefix("t3code/").is_some_and(|value| {
            hex_token(value)
                || (value.len() == 36
                    && uuid::Uuid::parse_str(value).is_ok_and(|value| {
                        value.get_version_num() == 4
                            && value.get_variant() == uuid::Variant::RFC4122
                    }))
        })
    })
}

fn hex_token(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) struct LocalWorktree {
    lease: Lease,
    base: base::Base,
}

struct Lease {
    repository: PathBuf,
    path: PathBuf,
    branch: String,
    owner: String,
}

struct Provision<'a> {
    source: base::Source<'a>,
    branch: &'a str,
    settings_path: Option<&'a Path>,
    progress: Option<&'a Progress>,
}

#[derive(Deserialize, Serialize)]
struct FinishedCheckout {
    owner: String,
    base: base::Base,
}

pub(super) async fn preflight(
    input: &LaunchInput,
    strategy: &WorkspaceStrategy,
    repository: &Path,
    title: &str,
    replay: bool,
) -> Result<(), LaunchError> {
    let Some((source, branch)) = single_checkout(input, strategy)? else {
        return Ok(());
    };
    if replay {
        return Ok(());
    }
    let failure = |detail: &str| input.fail("provision-worktree", detail);
    let inside = git::metadata(repository, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map_err(|_| failure("Git workspace could not be checked."))?;
    if !inside.status.success() || inside.stdout.trim_ascii() != b"true" {
        return Err(input.preflight_error(
            "ThreadLaunchNotRepositoryError",
            format!(
                "Project \"{title}\" ({}) is not a Git repository.",
                repository.display()
            ),
        ));
    }
    if !base::can_resolve(repository, source).await.unwrap_or(true) {
        return Err(input.preflight_error(
            "ThreadLaunchBaseRefError",
            format!(
                "Base ref \"{}\" does not resolve to a commit in project \"{title}\" ({}).",
                source.reference,
                repository.display()
            ),
        ));
    }
    let Some(branch) = branch else {
        return Ok(());
    };
    let format = git::metadata(repository, &["check-ref-format", "--branch", branch]).await;
    if !format.is_ok_and(|result| result.status.success()) {
        return Err(failure("The worktree branch name is invalid."));
    }
    let branches = git::text(
        repository,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    )
    .await
    .map_err(|_| failure("Local Git branches could not be read."))?;
    let branches = branches
        .lines()
        .filter_map(|line| line.strip_prefix("refs/heads/"))
        .collect::<Vec<_>>();
    if branches.contains(&branch) {
        return Err(input.preflight_error(
            "ThreadLaunchBranchExistsError",
            format!("Branch \"{branch}\" already exists in project \"{title}\"."),
        ));
    }
    if let Some(conflict) = branches.into_iter().find(|current| {
        current.starts_with(&format!("{branch}/")) || branch.starts_with(&format!("{current}/"))
    }) {
        return Err(input.preflight_error("ThreadLaunchBranchConflictError", format!("Branch \"{branch}\" cannot be created in project \"{title}\": branch \"{conflict}\" exists.")));
    }
    Ok(())
}

fn single_checkout<'a>(
    input: &LaunchInput,
    strategy: &'a WorkspaceStrategy,
) -> Result<Option<(base::Source<'a>, Option<&'a str>)>, LaunchError> {
    let WorkspaceStrategy::Worktree {
        base_ref,
        branch,
        start_from_origin,
        repositories,
    } = strategy
    else {
        return Ok(None);
    };
    if repositories.as_ref().is_some_and(|items| !items.is_empty()) {
        return Err(input.fail(
            "provision-worktree",
            "This worktree requires task-folder preparation.",
        ));
    }
    Ok(Some((
        base::Source {
            reference: &base_ref.0,
            fetch: *start_from_origin == Some(true),
        },
        branch.as_ref().map(|branch| branch.0.as_str()),
    )))
}

impl LocalWorktree {
    pub(super) async fn acquire(
        input: &LaunchInput,
        strategy: &WorkspaceStrategy,
        repository: &Path,
        parent: &Path,
        settings_path: Option<&Path>,
        cancelled: &mut watch::Receiver<bool>,
        progress: Option<&Progress>,
    ) -> Result<Self, LaunchError> {
        let (source, branch) = single_checkout(input, strategy)?.ok_or_else(|| {
            input.fail("provision-worktree", "A local worktree was not requested.")
        })?;
        let branch = match branch {
            Some(branch) => branch.to_owned(),
            None => generated_branch(input, settings_path, repository).await?,
        };
        Self::prepare(
            input,
            repository,
            parent,
            Provision {
                source,
                branch: &branch,
                settings_path,
                progress,
            },
            cancelled,
        )
        .await
        .map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                input.cancelled()
            } else {
                input.fail("provision-worktree", error)
            }
        })
    }

    async fn prepare(
        input: &LaunchInput,
        repository: &Path,
        parent: &Path,
        provision: Provision<'_>,
        cancelled: &mut watch::Receiver<bool>,
    ) -> io::Result<Self> {
        let Provision {
            source,
            branch,
            settings_path,
            progress,
        } = provision;
        let identity = branch_identity(repository, branch).await?;
        let digest_suffix = hash_prefix(&identity, 8);
        let label: String = {
            let mut result = String::new();
            for character in branch.chars() {
                if result.len() + character.len_utf8() > 23 {
                    break;
                }
                result.push(if character == '/' { '-' } else { character });
            }
            result
        };
        let name = format!("{label}-{digest_suffix}");
        let repository_name = repository
            .file_name()
            .ok_or_else(|| io::Error::other("The Git repository needs a folder name."))?;
        let parent = parent.join(repository_name);
        fs::create_dir_all(&parent)?;
        for counter in 1.. {
            let candidate = if counter == 1 {
                parent.join(&name)
            } else {
                parent.join(format!("{name}-{counter}"))
            };
            let owner = registration_owner(&identity, &input.command_id.0, &candidate);
            let lease = Lease {
                path: candidate,
                owner,
                repository: repository.to_owned(),
                branch: branch.to_owned(),
            };
            if lease.owns().await? {
                let base = lease.finished_base().await?.ok_or_else(|| {
                    io::Error::other("The reserved worktree checkout did not finish.")
                })?;
                let worktree = Self { lease, base };
                worktree.verify_commit().await?;
                if let Some(progress) = progress {
                    progress.stage(StageId::Fetch, Status::Skipped, None);
                    progress.stage(StageId::Checkout, Status::Running, None);
                }
                return Ok(worktree);
            }
            if lease.path.try_exists()? {
                continue;
            }
            let base = base::resolve(repository, source, cancelled, progress).await?;
            if let Some(progress) = progress {
                progress.stage(StageId::Checkout, Status::Running, None);
            }
            let checkout =
                CheckoutConfig::read(input, settings_path, repository, &base.commit).await?;
            let worktree = Self { lease, base };
            if let Err(error) = worktree.git_checkout(&checkout, cancelled).await {
                if worktree.lease.owns().await.unwrap_or(false) {
                    let _ = worktree.abort().await;
                }
                return Err(error);
            }
            if let Some((progress, source)) = progress.zip(checkout.submodules_disabled_by) {
                progress.stage(
                    StageId::Submodules,
                    Status::Skipped,
                    Some(&format!("disabled in {source}")),
                );
            }
            return Ok(worktree);
        }
        unreachable!()
    }

    async fn git_checkout(
        &self,
        config: &CheckoutConfig,
        cancelled: &mut watch::Receiver<bool>,
    ) -> io::Result<()> {
        let workers = format!("checkout.workers={}", config.workers);
        let mut args = vec![
            "-c",
            &workers,
            "worktree",
            "add",
            "--lock",
            "--reason",
            &self.lease.owner,
        ];
        if config.sparse.is_some() {
            args.push("--no-checkout");
        }
        args.extend([
            "-b",
            &self.lease.branch,
            "--",
            path_text(&self.lease.path)?,
            &self.base.commit,
        ]);
        git::checkout(&self.lease.repository, &args, None, cancelled).await?;
        if !self.lease.owns().await? {
            return Err(io::Error::other(
                "The worktree registration did not retain launch ownership.",
            ));
        }
        if let Some(patterns) = &config.sparse {
            let config_lock = git::lock_config(&self.lease.repository).await?;
            git::checkout(
                &self.lease.path,
                &["sparse-checkout", "set", "--no-cone", "--stdin"],
                Some(patterns),
                cancelled,
            )
            .await?;
            drop(config_lock);
            git::checkout(
                &self.lease.path,
                &["-c", &workers, "checkout"],
                None,
                cancelled,
            )
            .await?;
        }
        // A clean refreshed index avoids racing setup and the provider's first Git command.
        let _ = git::metadata(&self.lease.path, &["update-index", "--refresh"]).await;
        self.mark_checkout_finished().await?;
        Ok(())
    }

    pub(super) fn branch(&self) -> &str {
        &self.lease.branch
    }

    pub(super) async fn rename_branch(&mut self, branch: &str, exact: bool) -> io::Result<()> {
        let branch = rename::branch(&self.lease.path, &self.lease.branch, branch, exact).await?;
        self.lease.branch = branch;
        Ok(())
    }

    pub(super) fn path(&self) -> &Path {
        &self.lease.path
    }

    pub(super) async fn commit(self) -> io::Result<()> {
        if !self.lease.owns().await? {
            return Err(io::Error::other(
                "The worktree registration is no longer owned by the launch.",
            ));
        }
        {
            let _lock = git::lock_config(&self.lease.repository).await?;
            if let Some(upstream) = &self.base.upstream {
                let tracking = format!("--set-upstream-to={upstream}");
                let _ = git::text(
                    &self.lease.repository,
                    &["branch", &tracking, "--", &self.lease.branch],
                )
                .await;
            }
            if let Some(comparison) = &self.base.comparison {
                let key = format!("branch.{}.t3-merge-base", self.lease.branch);
                git::text(
                    &self.lease.repository,
                    &["config", "--local", &key, &comparison.branch],
                )
                .await?;
                if let Some(remote_ref) = &comparison.remote_ref {
                    let key = format!("branch.{}.t3-merge-base-ref", self.lease.branch);
                    git::text(
                        &self.lease.repository,
                        &["config", "--local", &key, remote_ref],
                    )
                    .await?;
                }
            }
        }
        git::text(
            &self.lease.repository,
            &["worktree", "unlock", "--", path_text(&self.lease.path)?],
        )
        .await?;
        if let Ok(Some(_)) = self.lease.finished_base().await
            && let Ok(marker) = self.lease.ready_marker().await
        {
            let _ = fs::remove_file(marker);
        }
        Ok(())
    }

    pub(super) async fn abort(self) -> io::Result<()> {
        if !self.lease.owns().await? {
            return Err(io::Error::other(
                "The worktree registration is no longer owned by the launch.",
            ));
        }
        self.require_clean().await?;
        let path = path_text(&self.lease.path)?;
        git::text(&self.lease.repository, &["worktree", "unlock", "--", path]).await?;
        if let Err(error) =
            git::text(&self.lease.repository, &["worktree", "remove", "--", path]).await
        {
            let _ = git::text(
                &self.lease.repository,
                &[
                    "worktree",
                    "lock",
                    "--reason",
                    &self.lease.owner,
                    "--",
                    path,
                ],
            )
            .await;
            return Err(error);
        }
        git::text(
            &self.lease.repository,
            &[
                "update-ref",
                "-d",
                &format!("refs/heads/{}", self.lease.branch),
                &self.base.commit,
            ],
        )
        .await?;
        Ok(())
    }

    async fn require_clean(&self) -> io::Result<()> {
        let status = git::text(
            &self.lease.path,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )
        .await?;
        if !status.is_empty() {
            return Err(io::Error::other(
                "The reserved checkout has changed since launch began.",
            ));
        }
        self.verify_commit().await
    }

    async fn verify_commit(&self) -> io::Result<()> {
        let head = git::text(&self.lease.path, &["rev-parse", "--verify", "HEAD"]).await?;
        if head.trim_end() != self.base.commit {
            return Err(io::Error::other(
                "The reserved checkout has moved to a different commit.",
            ));
        }
        Ok(())
    }

    async fn mark_checkout_finished(&self) -> io::Result<()> {
        use std::io::Write as _;
        let path = self.lease.ready_marker().await?;
        let contents = serde_json::to_vec(&FinishedCheckout {
            owner: self.lease.owner.clone(),
            base: self.base.clone(),
        })?;
        let mut marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        marker.write_all(&contents)?;
        marker.sync_all()
    }
}

impl Lease {
    async fn owns(&self) -> io::Result<bool> {
        let registrations =
            git::text(&self.repository, &["worktree", "list", "--porcelain", "-z"]).await?;
        let path = format!("worktree {}", path_text(&self.path)?);
        let canonical = self
            .path
            .canonicalize()
            .ok()
            .and_then(|path| path.to_str().map(|path| format!("worktree {path}")));
        let branch = format!("branch refs/heads/{}", self.branch);
        let owner = format!("locked {}", self.owner);
        Ok(registrations.split("\0\0").any(|entry| {
            let mut fields = entry.split('\0');
            fields.next().is_some_and(|registered| {
                registered == path || canonical.as_deref() == Some(registered)
            }) && fields.clone().any(|field| field == branch)
                && fields.any(|field| field == owner)
        }))
    }

    async fn ready_marker(&self) -> io::Result<PathBuf> {
        let linked_git_dir = git::text(&self.path, &["rev-parse", "--absolute-git-dir"]).await?;
        let common = git::common_directory(&self.repository).await?;
        let directory = PathBuf::from(linked_git_dir.trim_end_matches('\n')).canonicalize()?;
        if directory.parent() != Some(common.join("worktrees").as_path()) {
            return Err(io::Error::other(
                "The worktree registration is outside this Git repository.",
            ));
        }
        Ok(directory.join("t3-launch-checkout-ready"))
    }

    async fn finished_base(&self) -> io::Result<Option<base::Base>> {
        const MAX_MARKER: u64 = 16 * 1024;
        let path = self.ready_marker().await?;
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(MAX_MARKER + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MARKER {
            return Ok(None);
        }
        let Ok(record) = serde_json::from_slice::<FinishedCheckout>(&bytes) else {
            return Ok(None);
        };
        Ok((record.owner == self.owner).then_some(record.base))
    }
}

pub(super) async fn recorded_binding(
    input: &LaunchInput,
    requested: &WorkspaceStrategy,
    repository: &Path,
    checkout: &Path,
    original_branch: &str,
) -> Result<Option<LocalWorktree>, LaunchError> {
    if single_checkout(input, requested)?.is_none() {
        return Ok(None);
    }
    let failure = |error| input.fail("update-thread", error);
    let identity = branch_identity(repository, original_branch)
        .await
        .map_err(failure)?;
    let symbolic = git::text(checkout, &["symbolic-ref", "--quiet", "HEAD"])
        .await
        .map_err(failure)?;
    let current = symbolic
        .trim_end_matches('\n')
        .strip_prefix("refs/heads/")
        .ok_or_else(|| {
            input.fail(
                "update-thread",
                "The worktree no longer has its recorded branch.",
            )
        })?;
    let lease = Lease {
        repository: repository.to_owned(),
        path: checkout.to_owned(),
        branch: current.to_owned(),
        owner: registration_owner(&identity, &input.command_id.0, checkout),
    };
    if !lease.owns().await.map_err(failure)? {
        return Ok(None);
    }
    let base = lease
        .finished_base()
        .await
        .map_err(failure)?
        .ok_or_else(|| {
            input.fail(
                "update-thread",
                "The recorded checkout could not be recovered.",
            )
        })?;
    let worktree = LocalWorktree { lease, base };
    worktree.verify_commit().await.map_err(failure)?;
    Ok(Some(worktree))
}

async fn branch_identity(repository: &Path, branch: &str) -> io::Result<String> {
    let common = git::common_directory(repository).await?;
    Ok(format!("{}\0{branch}", path_text(&common)?))
}

fn registration_owner(identity: &str, command_id: &str, checkout: &Path) -> String {
    format!(
        "t3-create-{}",
        hash_prefix(
            &format!("{identity}\0{command_id}\0{}", checkout.display()),
            16
        )
    )
}

fn path_text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("The Git worktree path is not Unicode."))
}

fn hash_prefix(identity: &str, length: usize) -> String {
    let hash = digest(&SHA256, identity.as_bytes());
    let mut result = String::with_capacity(length * 2);
    for byte in &hash.as_ref()[..length] {
        write!(result, "{byte:02x}").unwrap();
    }
    result
}
