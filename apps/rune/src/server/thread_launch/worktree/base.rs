use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::super::setup_progress::{Progress, StageId, Status};
use super::git;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Base {
    pub(super) commit: String,
    pub(super) comparison: Option<Comparison>,
    pub(super) upstream: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Comparison {
    pub(super) branch: String,
    pub(super) remote_ref: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) struct Source<'a> {
    pub(super) reference: &'a str,
    pub(super) fetch: bool,
}

struct Remote {
    name: String,
    branch: String,
}

impl Remote {
    fn reference(&self) -> String {
        format!("refs/remotes/{}/{}", self.name, self.branch)
    }

    fn comparison(&self) -> Comparison {
        Comparison {
            branch: self.branch.clone(),
            remote_ref: Some(self.reference()),
        }
    }
}

pub(super) async fn can_resolve(cwd: &Path, source: Source<'_>) -> io::Result<bool> {
    if source.reference.starts_with(':') || commit(cwd, source.reference).await?.is_some() {
        return Ok(true);
    }
    if source.fetch {
        let names = remotes(cwd).await?;
        if let Some(remote) = fetch_target(cwd, source.reference, &names).await? {
            let (_live, mut cancelled) = watch::channel(false);
            if fetch(cwd, &remote, &mut cancelled).await?.is_some() {
                return Ok(true);
            }
        }
    }
    // A revision may still resolve through Git DWIM handling, and unborn repos fail at checkout.
    if symbolic_ref(cwd, source.reference).await?.is_some() {
        return Ok(true);
    }
    Ok(commit(cwd, "HEAD").await?.is_none())
}

pub(super) async fn resolve(
    cwd: &Path,
    source: Source<'_>,
    cancelled: &mut watch::Receiver<bool>,
    progress: Option<&Progress>,
) -> io::Result<Base> {
    let names = remotes(cwd).await?;
    let target = if source.fetch {
        fetch_target(cwd, source.reference, &names).await?
    } else {
        None
    };
    if let Some(progress) = progress {
        progress.stage(
            StageId::Fetch,
            if target.is_some() {
                Status::Running
            } else {
                Status::Skipped
            },
            None,
        );
    }
    if let Some(remote) = &target {
        let fetched = fetch(cwd, remote, cancelled).await?;
        if let Some(progress) = progress {
            progress.stage(StageId::Fetch, Status::Done, None);
        }
        if let Some(base) = fetched {
            return Ok(base);
        }
    }
    if let Some(base) = existing(cwd, source.reference, &names).await? {
        return Ok(base);
    }
    let primary = target.map(|target| target.name).or_else(|| {
        names
            .iter()
            .find(|name| *name == "origin")
            .or_else(|| names.first())
            .cloned()
    });
    if let Some(name) = primary {
        let head = format!("refs/remotes/{name}/HEAD");
        if let Some(reference) = symbolic_ref(cwd, &head).await?
            && let Some(branch) = reference.strip_prefix(&format!("refs/remotes/{name}/"))
            && branch != "HEAD"
        {
            let target = Remote {
                name,
                branch: branch.to_owned(),
            };
            match fetch(cwd, &target, cancelled).await {
                Ok(Some(base)) => {
                    if let Some(progress) = progress {
                        progress.fallback(&target.reference());
                    }
                    return Ok(base);
                }
                Ok(None) => (),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                Err(_) => {
                    if let Some(base) = existing(cwd, &target.reference(), &names).await? {
                        if let Some(progress) = progress {
                            progress.fallback(&target.reference());
                        }
                        return Ok(base);
                    }
                }
            }
        }
    }
    let base = existing(cwd, "HEAD", &names)
        .await?
        .ok_or_else(|| io::Error::other("No Git commit is available for the worktree base."))?;
    if let Some(progress) = progress {
        progress.fallback(
            base.comparison
                .as_ref()
                .map_or("HEAD", |comparison| &comparison.branch),
        );
    }
    Ok(base)
}

async fn remotes(cwd: &Path) -> io::Result<Vec<String>> {
    Ok(git::text(cwd, &["remote"])
        .await?
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

async fn fetch_target(cwd: &Path, base: &str, names: &[String]) -> io::Result<Option<Remote>> {
    let local = format!("refs/heads/{base}");
    let refs = git::text(
        cwd,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(upstream:remotename)%09%(upstream:remoteref)",
            &local,
        ],
    )
    .await?;
    let tracked = refs.lines().find_map(|line| {
        let mut parts = line.split('\t');
        (parts.next() == Some(local.as_str()))
            .then(|| (parts.next().unwrap_or(""), parts.next().unwrap_or("")))
    });
    let selected = match tracked {
        Some((remote, merge)) => merge
            .strip_prefix("refs/heads/")
            .filter(|_| names.iter().any(|name| name == remote))
            .map(|branch| Remote {
                name: remote.to_owned(),
                branch: branch.to_owned(),
            }),
        None => parse_remote(base, names),
    }
    .unwrap_or_else(|| Remote {
        name: "origin".to_owned(),
        branch: base.to_owned(),
    });
    Ok((names.contains(&selected.name) && is_branch(&selected.branch)).then_some(selected))
}

fn parse_remote(reference: &str, names: &[String]) -> Option<Remote> {
    let reference = reference.trim();
    names
        .iter()
        .filter_map(|name| {
            reference
                .strip_prefix(name)
                .and_then(|branch| branch.strip_prefix('/'))
                .map(|branch| (name, branch.trim()))
        })
        .max_by_key(|(name, _)| name.len())
        .filter(|(_, branch)| !branch.is_empty())
        .map(|(name, branch)| Remote {
            name: name.clone(),
            branch: branch.to_owned(),
        })
}

fn is_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch != "HEAD"
        && !branch.starts_with('-')
        && !branch.contains(['~', '^', ':', '?', '*', '\\', '[', ']', '|'])
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
}

async fn fetch(
    cwd: &Path,
    remote: &Remote,
    cancelled: &mut watch::Receiver<bool>,
) -> io::Result<Option<Base>> {
    let destination = remote.reference();
    let follow = format!("+refs/heads/{}:{destination}", remote.branch);
    let args = ["fetch", "--quiet", "--no-tags", "--", &remote.name, &follow];
    let mut result = git::fetch(cwd, &args, cancelled).await?;
    if !result.status.success() && ref_lock_failure(&String::from_utf8_lossy(&result.stderr)) {
        result = git::fetch(cwd, &args, cancelled).await?;
    }
    if !result.status.success() {
        let missing = format!(
            "fatal: couldn't find remote ref refs/heads/{}",
            remote.branch
        );
        if String::from_utf8_lossy(&result.stderr)
            .lines()
            .any(|line| line == missing)
        {
            return Ok(None);
        }
        let diagnostics = String::from_utf8_lossy(&result.stderr);
        let detail = fetch_diagnosis(&diagnostics)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("git fetch {} failed", remote.name));
        return Err(io::Error::other(format!(
            "Git command failed in GitVcsDriver.fetchRemote ({}): {detail}",
            cwd.display()
        )));
    }
    Ok(commit(cwd, &destination).await?.map(|commit| Base {
        commit,
        comparison: Some(remote.comparison()),
        upstream: None,
    }))
}

fn ref_lock_failure(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.starts_with("error: cannot lock ref")
            || line.starts_with("fatal: cannot lock ref")
            || (line.starts_with("fatal: unable to create ") && line.contains(".lock"))
            || (line.starts_with("error: fetching ref ")
                && (line.ends_with("incorrect old value provided")
                    || line.ends_with("reference already exists")))
    })
}

fn fetch_diagnosis(stderr: &str) -> Option<&'static str> {
    let lines = stderr
        .lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if lines.iter().any(|line| {
        line.starts_with("fatal: authentication failed")
            || line.starts_with("fatal: could not read username")
            || line.starts_with("fatal: could not read password")
            || line.contains(": permission denied (publickey")
    }) {
        return Some(
            "Git could not authenticate with the remote. Check Git credentials or SSH access on the server, then retry.",
        );
    }
    if lines.iter().any(|line| {
        line.contains("could not resolve host")
            || (line.starts_with("fatal: unable to access") && line.contains(": failed to connect"))
            || (line.starts_with("ssh: connect to host")
                && [
                    "connection timed out",
                    "connection refused",
                    "network is unreachable",
                ]
                .iter()
                .any(|error| line.ends_with(error)))
    }) {
        return Some(
            "Git could not reach the remote. Check the server's network connection and remote host, then retry.",
        );
    }
    if lines.iter().any(|line| {
        line == "remote: repository not found"
            || line == "remote: repository not found."
            || (line.starts_with("fatal: repository ") && line.ends_with(" not found"))
            || (line.starts_with("fatal: ")
                && line.ends_with(" does not appear to be a git repository"))
    }) {
        return Some(
            "Git could not access the remote repository. Check the remote URL and repository permissions on the server.",
        );
    }
    if ref_lock_failure(stderr) {
        return Some(
            "Git could not update a local reference. Another Git operation or a stale lock may be blocking the fetch; check the repository on the server, then retry.",
        );
    }
    if lines
        .iter()
        .any(|line| line.starts_with("fatal: bad object refs/"))
    {
        return Some(
            "Git found a broken local ref. Check the local repository on the server for a damaged or stray ref, then retry.",
        );
    }
    None
}

async fn existing(cwd: &Path, name: &str, names: &[String]) -> io::Result<Option<Base>> {
    let Some(commit) = commit(cwd, name).await? else {
        return Ok(None);
    };
    let reference = symbolic_ref(cwd, name).await?;
    let (comparison, upstream) = if let Some(reference) = reference.as_deref() {
        if let Some(branch) = reference.strip_prefix("refs/heads/") {
            (
                Some(Comparison {
                    branch: branch.to_owned(),
                    remote_ref: None,
                }),
                None,
            )
        } else if let Some(remote) = reference
            .strip_prefix("refs/remotes/")
            .and_then(|value| parse_remote(value, names))
        {
            (Some(remote.comparison()), Some(reference.to_owned()))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    Ok(Some(Base {
        commit,
        comparison,
        upstream,
    }))
}

async fn symbolic_ref(cwd: &Path, name: &str) -> io::Result<Option<String>> {
    let name = if name == "-" { "@{-1}" } else { name };
    let output = git::metadata(
        cwd,
        &[
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--end-of-options",
            name,
        ],
    )
    .await?;
    let reference = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    Ok((output.status.success() && !reference.trim().is_empty())
        .then(|| reference.trim().to_owned()))
}

async fn commit(cwd: &Path, name: &str) -> io::Result<Option<String>> {
    let name = if name == "-" { "@{-1}" } else { name };
    let name = if name.starts_with(":/") {
        name.to_owned()
    } else {
        format!("{name}^{{commit}}")
    };
    let output = git::metadata(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &name,
        ],
    )
    .await?;
    let commit = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    Ok((output.status.success() && !commit.trim().is_empty()).then(|| commit.trim().to_owned()))
}
