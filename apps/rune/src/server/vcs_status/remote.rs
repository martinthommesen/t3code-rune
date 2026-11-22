use std::path::Path;

use reqwest::Url;

use super::git::{config, ref_exists, text};
use super::local::Read;
use super::{HostingProvider, RemoteStatus, trim};

pub(super) struct RemoteRefs {
    pub primary: Option<String>,
    default_branch: Option<String>,
}

impl RemoteRefs {
    pub async fn read(cwd: &Path) -> Self {
        let remotes = text(cwd, &["remote"]).await;
        let mut names: Vec<_> = remotes
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(trim)
            .filter(|name| !name.is_empty())
            .collect();
        names.sort_by_cached_key(|name| std::cmp::Reverse(name.encode_utf16().count()));
        let primary = if names.contains(&"origin") {
            Some("origin")
        } else {
            names.first().copied()
        };
        let default_branch = if primary.is_some() {
            match default_branch(cwd, "origin").await {
                Some(origin) => Some(origin),
                None => match primary {
                    Some(remote) if remote != "origin" => default_branch(cwd, remote).await,
                    _ => None,
                },
            }
        } else {
            None
        };
        Self {
            primary: primary.map(str::to_owned),
            default_branch,
        }
    }

    pub fn is_default(&self, branch: Option<&str>) -> bool {
        branch.is_some_and(|branch| {
            self.default_branch
                .as_deref()
                .map_or(matches!(branch, "main" | "master"), |default| {
                    branch == default
                })
        })
    }
}

async fn default_branch(cwd: &Path, remote: &str) -> Option<String> {
    let prefix = format!("refs/remotes/{remote}/");
    let head = format!("{prefix}HEAD");
    let target = text(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--symbolic-full-name",
            &head,
        ],
    )
    .await?;
    let branch = trim(target.strip_prefix(&prefix)?);
    (!branch.is_empty() && branch != "HEAD").then(|| branch.to_owned())
}

pub(super) async fn tracking(cwd: &Path, status: &Read) -> RemoteStatus {
    let Some(branch) = status.local.ref_name.as_deref() else {
        return RemoteStatus::default();
    };
    let has_upstream = text(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .await
    .is_some();
    let base_ahead = if !has_upstream || !status.local.is_default_ref {
        match comparison_base(cwd, &status.remotes, branch, false).await {
            Some(base) => {
                let range = format!("{}..HEAD", base.reference);
                text(cwd, &["rev-list", "--count", "--end-of-options", &range])
                    .await
                    .and_then(|count| count.parse().ok())
                    .unwrap_or_default()
            }
            None => 0,
        }
    } else {
        0
    };
    let (ahead_count, behind_count) = if has_upstream {
        let count = text(
            cwd,
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
        )
        .await
        .unwrap_or_default();
        let mut fields = count.split_whitespace();
        (
            fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
        )
    } else {
        (base_ahead, 0)
    };
    RemoteStatus {
        has_upstream,
        ahead_count,
        behind_count,
        ahead_of_default_count: if status.local.is_default_ref {
            0
        } else {
            base_ahead
        },
        pr: None,
    }
}

pub(super) struct Base {
    pub name: String,
    pub reference: String,
}

async fn explicit_base(cwd: &Path, name: &str) -> Option<Base> {
    let reference = if name.starts_with("refs/heads/") || name.starts_with("refs/remotes/") {
        name.to_owned()
    } else if name.contains('/') && !name.starts_with("refs/") {
        format!("refs/remotes/{name}")
    } else {
        return None;
    };
    ref_exists(cwd, &reference).await.then(|| Base {
        name: name.to_owned(),
        reference,
    })
}

async fn tracked_base(cwd: &Path, branch: &str) -> Option<Base> {
    let upstream = format!("{branch}@{{upstream}}");
    let reference = text(
        cwd,
        &[
            "rev-parse",
            "--symbolic-full-name",
            "--end-of-options",
            &upstream,
        ],
    )
    .await?;
    let name = reference.strip_prefix("refs/remotes/")?.to_owned();
    Some(Base { name, reference })
}

pub(super) async fn comparison_base(
    cwd: &Path,
    remotes: &RemoteRefs,
    branch: &str,
    allow_current_remote: bool,
) -> Option<Base> {
    if let Some(configured) = config(cwd, &format!("branch.{branch}.t3-merge-base-ref")).await
        && (configured.starts_with("refs/heads/") || configured.starts_with("refs/remotes/"))
        && let Some(mut base) = explicit_base(cwd, &configured).await
    {
        let short = configured
            .strip_prefix("refs/remotes/")
            .unwrap_or(&configured);
        if !ref_exists(cwd, &format!("refs/heads/{short}")).await {
            base.name = short.to_owned();
        }
        return Some(base);
    }
    let current_key = format!("branch.{branch}.t3-merge-base");
    let legacy_key = format!("branch.{branch}.gh-merge-base");
    let configured = match config(cwd, &current_key).await {
        Some(name) => Some(name),
        None => config(cwd, &legacy_key).await,
    };
    if let Some(configured) = configured.as_deref() {
        if let Some(base) = explicit_base(cwd, configured).await {
            if configured.contains('/') && !configured.starts_with("refs/") {
                let reference = format!("refs/heads/{configured}");
                if ref_exists(cwd, &reference).await {
                    return Some(Base {
                        name: reference.clone(),
                        reference,
                    });
                }
            }
            return Some(base);
        }
        if configured != branch
            && let Some(base) = tracked_base(cwd, configured).await
        {
            return Some(base);
        }
    }
    let remote = remotes.primary.as_deref();
    let default = match remote {
        Some(remote) => default_branch(cwd, remote).await,
        None => None,
    };
    let remote_prefix = remote.map(|remote| format!("{remote}/"));
    for candidate in [
        configured.as_deref(),
        default.as_deref(),
        Some("main"),
        Some("master"),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = candidate
            .strip_prefix("origin/")
            .or_else(|| {
                remote_prefix
                    .as_deref()
                    .and_then(|prefix| candidate.strip_prefix(prefix))
            })
            .unwrap_or(candidate);
        if candidate.is_empty() || (candidate == branch && !allow_current_remote) {
            continue;
        }
        if candidate == branch
            && let Some(base) = tracked_base(cwd, branch).await
        {
            return Some(base);
        }
        if let Some(remote) = remote {
            let name = format!("{remote}/{candidate}");
            let reference = format!("refs/remotes/{name}");
            if ref_exists(cwd, &reference).await {
                return Some(Base { name, reference });
            }
        }
        if candidate != branch {
            let reference = format!("refs/heads/{candidate}");
            if ref_exists(cwd, &reference).await {
                return Some(Base {
                    name: candidate.to_owned(),
                    reference,
                });
            }
        }
    }
    None
}

pub(super) async fn hosting_provider(cwd: &Path, branch: Option<&str>) -> Option<HostingProvider> {
    let remote = match branch {
        Some(branch) => config(cwd, &format!("branch.{branch}.remote")).await,
        None => None,
    };
    let mut url = text(
        cwd,
        &[
            "config",
            "--get-all",
            &format!("remote.{}.url", remote.as_deref().unwrap_or("origin")),
        ],
    )
    .await;
    if url.is_none() && remote.as_deref().is_some_and(|remote| remote != "origin") {
        url = text(cwd, &["config", "--get-all", "remote.origin.url"]).await;
    }
    recognize_provider(url.as_deref()?.lines().next()?.trim())
}

fn recognize_provider(remote: &str) -> Option<HostingProvider> {
    let url = Url::parse(remote).ok().filter(|url| url.has_host());
    let (hostname, host) = match url.as_ref() {
        Some(url) => {
            let hostname = url.host_str()?.to_ascii_lowercase();
            let host = match url.port().filter(|_| url.scheme() != "ssh") {
                Some(port) => format!("{hostname}:{port}"),
                None => hostname.clone(),
            };
            (hostname, host)
        }
        None => {
            let (authority, path) = remote.split_once(':')?;
            let hostname = if let Some((user, host)) = authority.split_once('@') {
                if user.is_empty()
                    || !user
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                {
                    return None;
                }
                host
            } else {
                if authority.encode_utf16().count() < 2
                    || path.starts_with(':')
                    || path.trim_matches('/').is_empty()
                    || path.chars().any(char::is_whitespace)
                {
                    return None;
                }
                authority
            };
            if hostname.is_empty()
                || hostname.contains(['/', '\\', '@'])
                || hostname.chars().any(char::is_whitespace)
            {
                return None;
            }
            let host = hostname.to_ascii_lowercase();
            (host.clone(), host)
        }
    };
    let host_label = |label| hostname.split('.').any(|part| part == label);
    let (kind, name, base_url) = match hostname.as_str() {
        "github.com" => ("github", "GitHub", format!("https://{host}")),
        _ if host_label("github") || hostname.ends_with(".ghe.com") => {
            ("github", "GitHub Self-Hosted", format!("https://{host}"))
        }
        "gitlab.com" => ("gitlab", "GitLab", format!("https://{host}")),
        _ if host_label("gitlab") => ("gitlab", "GitLab Self-Hosted", format!("https://{host}")),
        "origin.cursor.com" => (
            "cursor-origin",
            "Cursor Origin",
            "https://cursor.com/codebase".into(),
        ),
        "source.tanstack.com" => (
            "tanstack-source",
            "TanStack Source",
            "https://source.tanstack.com".into(),
        ),
        "git.cafe" | "staging.git.cafe" => ("gitcafe", "GitCafe", format!("https://{hostname}")),
        _ if hostname == "codeberg.org" || host_label("forgejo") || host_label("gitea") => (
            "forgejo",
            "Forgejo",
            url.as_ref()
                .filter(|url| matches!(url.scheme(), "https" | "http"))
                .map_or_else(
                    || format!("https://{host}"),
                    |url| url.origin().ascii_serialization(),
                ),
        ),
        "bitbucket.org" | "altssh.bitbucket.org" => {
            ("bitbucket", "Bitbucket", "https://bitbucket.org".into())
        }
        _ if hostname.ends_with(".bitbucket.org") => {
            ("bitbucket", "Bitbucket", "https://bitbucket.org".into())
        }
        _ if host_label("bitbucket") => (
            "bitbucket",
            "Bitbucket Self-Hosted",
            format!("https://{host}"),
        ),
        "dev.azure.com" | "ssh.dev.azure.com" => {
            ("azure-devops", "Azure DevOps", format!("https://{host}"))
        }
        _ if hostname.ends_with(".visualstudio.com") || hostname.ends_with(".dev.azure.com") => {
            ("azure-devops", "Azure DevOps", format!("https://{host}"))
        }
        _ => ("unknown", host.as_str(), format!("https://{host}")),
    };
    Some(HostingProvider {
        kind,
        name: name.to_owned(),
        base_url,
    })
}
