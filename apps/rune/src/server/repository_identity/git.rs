use std::path::{Path, PathBuf};

use super::super::git_checkpoint::read_command;
use super::{Checkout, RepositoryIdentity, identity, remote, ssh};

pub(super) async fn checkout(cwd: &Path) -> Option<Checkout> {
    if cwd.metadata().is_ok_and(|entry| !entry.is_dir()) {
        return None;
    }
    let top = read_command(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .ok()?;
    if top.status.success() {
        return root(&top.stdout, false);
    }
    for (entry, directory) in [("HEAD", "--git-dir=."), (".git", "--git-dir=.git")] {
        if cwd.join(entry).symlink_metadata().is_err() {
            continue;
        }
        let output = read_command(
            cwd,
            &[
                directory,
                "rev-parse",
                "--is-bare-repository",
                "--absolute-git-dir",
            ],
        )
        .await
        .ok()?;
        if output.status.success() {
            let (bare, stdout) = output.stdout.split_once('\n')?;
            return (bare == "true").then(|| root(stdout, true)).flatten();
        }
    }
    None
}

fn root(stdout: &str, bare: bool) -> Option<Checkout> {
    let path = stdout.strip_suffix('\n').unwrap_or(stdout);
    (!path.is_empty()).then(|| Checkout {
        root: PathBuf::from(path),
        bare,
    })
}

pub(super) async fn lookup(checkout: &Checkout, ssh: &ssh::Config) -> Option<RepositoryIdentity> {
    let prefix = checkout.bare.then_some("--git-dir=.");
    let remotes = [prefix, Some("remote"), Some("-v")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let marks = [
        prefix,
        Some("config"),
        Some("--get-regexp"),
        Some(r"^remote\..*\.gh-resolved$"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let (remotes, marks) = tokio::join!(
        read_command(&checkout.root, &remotes),
        read_command(&checkout.root, &marks),
    );
    let remotes = remotes.ok()?;
    if !remotes.status.success() {
        return None;
    }
    let remotes = remote::fetch_remotes(&remotes.stdout);
    let marks = marks.ok().filter(|result| result.status.success());
    let marks = marks.as_ref().map_or("", |result| result.stdout.as_str());
    let ssh = ssh.clone();
    let checkout = checkout.clone();
    let marks = marks.to_owned();
    tokio::task::spawn_blocking(move || identity(&checkout, &remotes, &marks, &ssh))
        .await
        .ok()
        .flatten()
}
