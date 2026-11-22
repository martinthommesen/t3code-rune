use reqwest::Url;

use crate::server::turn_commands::encode_id_part;

const TANSTACK: &str = "source.tanstack.com";
const AZURE: &str = "dev.azure.com";

pub(super) fn legacy(repository: &str, raw_url: &str, number: u64) -> (String, String) {
    let url = Url::parse(raw_url).ok();
    if let Some(key) = url
        .as_ref()
        .and_then(request_key)
        .filter(|key| key.number == number)
    {
        if let Some(authority) = key.authority {
            return canonical_key(&authority, &key.repository);
        }
        if let Some(owned) = recognized_key(&key.host, &key.repository) {
            return owned;
        }
    }
    let host = url
        .as_ref()
        .and_then(Url::host_str)
        .filter(|host| !host.is_empty())
        .unwrap_or("unknown");
    (host.to_lowercase(), repository.to_lowercase())
}

pub(super) fn stored(host: &str, repository: &str, url: &str, number: u64) -> (String, String) {
    let repository = canonical_repository(repository);
    let authority = Url::parse(url)
        .ok()
        .as_ref()
        .and_then(request_key)
        .filter(|key| key.number == number && key.repository == repository)
        .and_then(|key| key.authority);
    canonical_key(authority.as_deref().unwrap_or(host), &repository)
}

fn canonical_key(host: &str, repository: &str) -> (String, String) {
    let host = host.to_lowercase();
    let repository = canonical_repository(repository);
    recognized_key(&host, &repository).unwrap_or((host, repository))
}

fn recognized_key(host: &str, repository: &str) -> Option<(String, String)> {
    if host == TANSTACK {
        let repository = repository.strip_prefix("%40").unwrap_or(repository);
        return Some((host.to_owned(), repository.to_owned()));
    }
    if host == AZURE {
        return Some((host.to_owned(), repository.to_owned()));
    }
    let parts: Vec<_> = repository.split('/').collect();
    let mapped = match (host, parts.as_slice()) {
        ("ssh.dev.azure.com" | "vs-ssh.visualstudio.com", ["v3", organization, project, repo])
            if [organization, project, repo]
                .iter()
                .all(|part| !part.is_empty()) =>
        {
            format!("{organization}/{project}/_git/{repo}")
        }
        (_, [project, "_git", repo] | ["defaultcollection", project, "_git", repo])
            if !project.is_empty() && !repo.is_empty() =>
        {
            let organization = host.strip_suffix(".visualstudio.com")?;
            if organization.is_empty() || organization.contains('.') {
                return None;
            }
            format!("{organization}/{project}/_git/{repo}")
        }
        _ => return None,
    };
    Some((AZURE.to_owned(), mapped))
}

struct RequestKey {
    host: String,
    repository: String,
    number: u64,
    authority: Option<String>,
}

fn request_key(url: &Url) -> Option<RequestKey> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let parts: Vec<_> = url.path().strip_prefix('/')?.split('/').collect();
    let host_authority = || match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let key = |repository: &str, number, authority| RequestKey {
        host: host.to_owned(),
        repository: canonical_repository(repository),
        number,
        authority,
    };
    if matches!(host, "git.cafe" | "staging.git.cafe")
        && let Some((repository, number)) = fixed_route(&parts, &["pull", "pulls"])
    {
        return Some(key(&repository, number, None));
    }
    if host == TANSTACK
        && let Some((repository, number)) = fixed_route(&parts, &["pull"])
    {
        let owner = parts[0];
        let name = owner
            .strip_prefix('@')
            .or_else(|| owner.strip_prefix("%40"))
            .unwrap_or(owner);
        if !name.is_empty() && !name.contains(['@', '%']) {
            let repository = repository
                .strip_prefix('@')
                .or_else(|| repository.strip_prefix("%40"))
                .unwrap_or(&repository);
            return Some(key(repository, number, None));
        }
    }
    // Cursor's web URL has neither a port identity nor a repository key owner.
    if matches!(host, "cursor.com" | "www.cursor.com")
        && parts.first() == Some(&"codebase")
        && let Some((repository, number)) = fixed_route(&parts[1..], &["pull"])
    {
        return Some(RequestKey {
            host: "origin.cursor.com".to_owned(),
            ..key(&repository, number, None)
        });
    }
    if let Some((repository, number)) = nested_route(&parts, "pullrequest", |prefix| {
        prefix.iter().rev().nth(1) == Some(&"_git")
    }) {
        return Some(key(
            &repository,
            number,
            url.port().map(|_| host_authority()),
        ));
    }
    if let Some((repository, number)) = fixed_route(&parts, &["pull"]) {
        return Some(key(&repository, number, None));
    }
    if let Some((repository, number)) = nested_route(&parts, "merge_requests", |prefix| {
        prefix.len() >= 3 && prefix.last() == Some(&"-")
    }) {
        let repository = repository
            .strip_suffix("/-")
            .expect("validated GitLab route");
        return Some(key(repository, number, None));
    }
    if let Some((repository, number)) = nested_route(&parts, "pulls", |_| true) {
        return Some(key(&repository, number, Some(host_authority())));
    }
    None
}

fn fixed_route(parts: &[&str], markers: &[&str]) -> Option<(String, u64)> {
    let [owner, repo, marker, number, ..] = parts else {
        return None;
    };
    if owner.is_empty() || repo.is_empty() || !markers.contains(marker) {
        return None;
    }
    Some((format!("{owner}/{repo}"), request_number(number)?))
}

fn nested_route(
    parts: &[&str],
    marker: &str,
    route: impl Fn(&[&str]) -> bool,
) -> Option<(String, u64)> {
    let index = (2..parts.len().saturating_sub(1)).rev().find(|&index| {
        parts[index] == marker
            && decimal_digits(parts[index + 1])
            && parts[..index].iter().all(|part| !part.is_empty())
            && route(&parts[..index])
    })?;
    Some((parts[..index].join("/"), request_number(parts[index + 1])?))
}

fn request_number(raw: &str) -> Option<u64> {
    if !decimal_digits(raw) {
        return None;
    }
    raw.parse::<u64>()
        .ok()
        .filter(|number| (1..=9_007_199_254_740_991).contains(number))
}

fn decimal_digits(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit())
}

fn canonical_repository(repository: &str) -> String {
    repository
        .split('/')
        .map(canonical_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_segment(raw: &str) -> String {
    let decode = || {
        let mut bytes = raw.bytes();
        let mut decoded = Vec::with_capacity(raw.len());
        while let Some(byte) = bytes.next() {
            if byte == b'%' {
                let high = char::from(bytes.next()?).to_digit(16)? as u8;
                let low = char::from(bytes.next()?).to_digit(16)? as u8;
                decoded.push(high * 16 + low);
            } else {
                decoded.push(byte);
            }
        }
        String::from_utf8(decoded).ok()
    };
    decode()
        .map(|decoded| encode_id_part(&decoded.to_lowercase()).to_ascii_lowercase())
        .unwrap_or_else(|| raw.to_lowercase())
}
