use std::path::PathBuf;

use serde::Serialize;

mod cache;
mod git;
mod project;
mod remote;
mod ssh;

pub(super) use cache::{Change, Identities};
pub(super) use project::{digest_projects, enrich as enrich_project, enrich_projects};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RepositoryIdentity {
    canonical_key: String,
    locator: Locator,
    #[serde(skip_serializing_if = "Option::is_none")]
    web_url: Option<String>,
    root_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<Origin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Locator {
    source: &'static str,
    remote_name: String,
    remote_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Origin {
    canonical_key: String,
    remote_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct Checkout {
    root: PathBuf,
    bare: bool,
}

fn identity(
    checkout: &Checkout,
    remotes: &[remote::Remote],
    marks: &str,
    ssh: &ssh::Config,
) -> Option<RepositoryIdentity> {
    let primary = remote::primary(remotes, marks)?;
    let key_url = ssh.resolve(&primary.url);
    let canonical_key = remote::repository_key(&key_url);
    let path = remote::display_path(&canonical_key, &primary.url);
    let provider = remote::provider(&primary.url);
    let web_url = (provider == Some("cursor-origin"))
        .then(|| remote::provider_path(&primary.url))
        .flatten()
        .map(|path| format!("https://cursor.com/codebase/{path}"));
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next();
    let name = segments.next_back().or(owner);
    let origin = remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .and_then(|origin| {
            let origin_key = if origin.url == primary.url {
                canonical_key.clone()
            } else {
                remote::repository_key(&ssh.resolve(&origin.url))
            };
            if origin_key == canonical_key {
                return None;
            }
            let path = remote::display_path(&origin_key, &origin.url);
            Some(Origin {
                canonical_key: origin_key,
                remote_url: origin.url.clone(),
                display_name: nonblank(&path),
            })
        });
    Some(RepositoryIdentity {
        canonical_key,
        locator: Locator {
            source: "git-remote",
            remote_name: primary.name.clone(),
            remote_url: primary.url.clone(),
        },
        web_url,
        root_path: checkout.root.clone(),
        display_name: nonblank(&path),
        provider,
        owner: owner.map(str::to_owned),
        name: name.map(str::to_owned),
        origin,
    })
}

fn nonblank(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_an_ssh_alias_after_cold_resolution() {
        let fixture = tempfile::tempdir().unwrap();
        let config = fixture.path().join("ssh-config");
        std::fs::write(&config, "Host proof-forge\n    HostName github.com\n").unwrap();
        let checkout = fixture.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        for arguments in [
            &["init", "--quiet"][..],
            &["remote", "add", "origin", "git@proof-forge:Team/Checkout.git"][..],
        ] {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(&checkout)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let identities = Identities::isolated_ssh(config);
        let mut changes = identities.subscribe();
        let initial = identities.get_available(&checkout);
        assert!(!initial.resolved && initial.identity.is_none());

        let change = tokio::time::timeout(std::time::Duration::from_secs(10), changes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.workspace_root, checkout);
        let identity = change.identity.unwrap();
        assert_eq!(identity.canonical_key, "github.com/team/checkout");
        assert_eq!(identity.locator.remote_url, "git@proof-forge:Team/Checkout.git");
        let warm = identities.get_available(&checkout);
        assert!(warm.resolved);
        assert_eq!(warm.identity, Some(identity));
    }
}
