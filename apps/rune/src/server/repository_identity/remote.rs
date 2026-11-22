use reqwest::Url;

use super::super::locale::path_collator;

pub(super) struct Remote {
    pub name: String,
    pub url: String,
}

pub(super) fn fetch_remotes(output: &str) -> Vec<Remote> {
    let mut remotes = Vec::<Remote>::new();
    for line in output.lines() {
        let line = line
            .rsplit_once(" [")
            .filter(|(_, annotation)| annotation.ends_with(']'))
            .map_or(line, |(head, _)| head);
        let Some((name, url)) = line
            .strip_suffix(" (fetch)")
            .and_then(|line| line.split_once('\t'))
        else {
            continue;
        };
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            continue;
        }
        let Some(url) = public_locator(url) else {
            continue;
        };
        if let Some(remote) = remotes.iter_mut().find(|remote| remote.name == name) {
            remote.url = url;
        } else {
            remotes.push(Remote {
                name: name.to_owned(),
                url,
            });
        }
    }
    remotes
}

fn public_locator(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        return None;
    }
    if let Some((scheme, remaining)) = value.split_once("://") {
        if !url_scheme(scheme) {
            // A remote helper can embed another URL with its own credentials.
            return None;
        }
        let unfiltered = remaining.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = unfiltered.split_once('/').unwrap_or((unfiltered, ""));
        let authority = match authority.rsplit_once('@') {
            Some((user, _)) if scheme.eq_ignore_ascii_case("ssh") && simple_user(user) => authority,
            Some((_, host)) => host,
            None => authority,
        };
        let tail = if unfiltered.contains('/') {
            format!("/{path}")
        } else {
            String::new()
        };
        return Some(format!("{scheme}://{authority}{tail}"));
    }
    let colon = value.find(':')?;
    if value.find(['/', '\\']).is_some_and(|slash| slash < colon)
        || (colon == 1 && value.as_bytes()[0].is_ascii_alphabetic())
        || value[colon + 1..].starts_with(':')
    {
        return None;
    }
    // A regular SCP locator keeps the SSH login. An ambiguous user-info spelling does not.
    let path_start = value[colon + 1..].split('/').next().unwrap_or_default();
    if (path_start.contains('@') && path_start.contains(':'))
        || (value.contains('@') && scp(value).is_none())
    {
        return None;
    }
    Some(value.to_owned())
}

fn url_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"+.-".contains(&byte))
}

fn simple_user(user: &str) -> bool {
    !user.is_empty()
        && user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

pub(super) fn primary<'a>(remotes: &'a [Remote], marks: &str) -> Option<&'a Remote> {
    let mut marked = marks.lines().filter_map(|line| {
        let (field, value) = line.trim().split_once(char::is_whitespace)?;
        let name = field
            .strip_prefix("remote.")?
            .strip_suffix(".gh-resolved")?;
        let value = value.trim();
        if value.is_empty() || value.chars().any(char::is_whitespace) {
            return None;
        }
        let remote = remotes.iter().find(|remote| remote.name == name)?;
        Some((remote, value))
    });
    if let Some((marked_remote, value)) = marked.next()
        && marked.next().is_none()
    {
        if value == "base" {
            return Some(marked_remote);
        }
        let marked_path = value.to_lowercase();
        let marked_host = host(&marked_remote.url).map(|host| host.portable);
        if let Some(selected) = remotes.iter().find(|remote| {
            host(&remote.url).map(|host| host.portable) == marked_host
                && after_first_slash(&normalize(&remote.url)) == marked_path
        }) {
            return Some(selected);
        }
    }
    let collator = path_collator();
    remotes
        .iter()
        .find(|remote| remote.name == "upstream")
        .or_else(|| remotes.iter().find(|remote| remote.name == "origin"))
        .or_else(|| {
            remotes.iter().min_by(|left, right| {
                collator.as_ref().map_or_else(
                    || left.name.cmp(&right.name),
                    |collator| collator.compare(&left.name, &right.name),
                )
            })
        })
}

pub(super) fn repository_key(value: &str) -> String {
    if let Some((hostname, path)) = repository_parts(value, true) {
        if hostname == "source.tanstack.com"
            && let Some(rest) = path.strip_prefix("%40").or_else(|| path.strip_prefix('@'))
            && let Some((owner, repository)) = rest.split_once('/')
            && !owner.is_empty()
            && !owner.contains(['@', '%'])
            && !repository.contains('/')
        {
            return format!("{hostname}/{}", strip_git_suffix(rest));
        }
        if matches!(
            hostname.as_str(),
            "ssh.dev.azure.com" | "vs-ssh.visualstudio.com"
        ) && let Some(tail) = path.strip_prefix("v3/")
        {
            let mut segments = tail.split('/');
            if let (Some(org), Some(project), Some(repo), None) = (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) {
                return if hostname == "ssh.dev.azure.com" {
                    format!("dev.azure.com/{org}/{project}/_git/{repo}")
                } else {
                    format!("{org}.visualstudio.com/{project}/_git/{repo}")
                };
            }
        }
        if azure_host(&hostname) {
            return format!("{hostname}/{path}");
        }
    }
    normalize(value)
}

fn normalize(value: &str) -> String {
    repository_parts(value, false).map_or_else(
        || strip_git_suffix(value.trim().trim_end_matches('/')).to_lowercase(),
        |(host, path)| format!("{host}/{path}"),
    )
}

fn repository_parts(value: &str, keep_git: bool) -> Option<(String, String)> {
    let value = value.trim().to_lowercase();
    if let Some((scheme, _)) = value.split_once("://")
        && matches!(scheme, "https" | "http" | "ssh" | "git")
    {
        let url = Url::parse(&value).ok()?;
        let hostname = url.host_str()?;
        let segments: Vec<_> = repository_path(url.path(), keep_git)
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.len() > 1 {
            return Some((hostname.to_owned(), canonical_path(&segments.join("/"))));
        }
    }
    let (raw_host, path) = scp(&value)?;
    let path = repository_path(path.strip_prefix('/').unwrap_or(path), keep_git);
    if !qualified_path(path) {
        return None;
    }
    let hostname = if raw_host.starts_with('[') {
        Url::parse(&format!("ssh://{raw_host}"))
            .ok()?
            .host_str()?
            .to_owned()
    } else {
        raw_host.to_owned()
    };
    Some((hostname, canonical_path(path)))
}

fn repository_path(value: &str, keep_git: bool) -> &str {
    let value = value.trim_end_matches('/');
    if keep_git {
        value
    } else {
        strip_git_suffix(value)
    }
}

fn scp(value: &str) -> Option<(&str, &str)> {
    if value.contains("://") {
        return None;
    }
    let colon = value.find(':')?;
    let user = value[..colon].split_once('@').map(|(user, _)| user);
    let host_and_path = user.map_or(value, |user| &value[user.len() + 1..]);
    if user.is_some_and(|user| !simple_user(user)) {
        return None;
    }
    let (host, path) = if let Some(remainder) = host_and_path.strip_prefix('[') {
        let (bracketed, path) = remainder.split_once("]:")?;
        if bracketed.is_empty()
            || !bracketed
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || b":.".contains(&byte))
        {
            return None;
        }
        (&host_and_path[..bracketed.len() + 2], path)
    } else {
        host_and_path.split_once(':')?
    };
    if host.is_empty()
        || (user.is_none() && host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic())
        || host.contains(['@', '/', '\\'])
        || host.chars().any(char::is_whitespace)
        || path.starts_with(':')
    {
        return None;
    }
    Some((host, path))
}

pub(super) fn display_path(key: &str, value: &str) -> String {
    let canonical = after_first_slash(key);
    if let Some(path) = provider_path(value) {
        let expected = canonical_path(canonical);
        if canonical_path(path) == expected {
            return path.to_owned();
        }
        let literal_suffix = format!("{path}.git");
        if canonical_path(&literal_suffix) == expected {
            return literal_suffix;
        }
    }
    canonical.to_owned()
}

pub(super) fn provider_path(value: &str) -> Option<&str> {
    let single = value.split(['?', '#']).next().unwrap_or_default();
    let display = if let Some((scheme, authority)) = single.split_once("://")
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git"
        ) {
        authority.split_once('/').map(|(_, path)| path)
    } else {
        scp(value).map(|(_, path)| path)
    };
    display
        .map(|path| path.strip_suffix('/').unwrap_or(path))
        .filter(|path| qualified_path(path))
        .map(strip_git_suffix)
}

fn after_first_slash(value: &str) -> &str {
    value.split_once('/').map_or("", |(_, rest)| rest)
}

fn qualified_path(path: &str) -> bool {
    path.contains('/')
        && !path.chars().any(char::is_whitespace)
        && path.split('/').all(|part| !part.is_empty())
}

fn strip_git_suffix(value: &str) -> &str {
    let before = value.len().saturating_sub(4);
    if value
        .get(before..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".git"))
    {
        &value[..before]
    } else {
        value
    }
}

fn canonical_path(value: &str) -> String {
    value
        .split('/')
        .map(canonical_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_segment(value: &str) -> String {
    let Some(decoded) = decode_percent(value) else {
        return value.to_lowercase();
    };
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::new();
    for byte in decoded.to_lowercase().bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 15) as usize]));
        }
    }
    encoded
}

fn decode_percent(value: &str) -> Option<String> {
    let mut bytes = value.bytes();
    let mut decoded = Vec::with_capacity(value.len());
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let upper = char::from(bytes.next()?).to_digit(16)?;
            let lower = char::from(bytes.next()?).to_digit(16)?;
            (upper * 16 + lower) as u8
        } else {
            byte
        };
        decoded.push(byte);
    }
    String::from_utf8(decoded).ok()
}

struct Host {
    portable: String,
    hostname: String,
}

fn host(value: &str) -> Option<Host> {
    if let Some((hostname, _)) = scp(value) {
        let hostname = hostname.to_lowercase();
        return Some(Host {
            portable: hostname.clone(),
            hostname,
        });
    }
    let url = Url::parse(value).ok()?;
    let hostname = url.host_str()?.to_lowercase();
    let portable = match url.port().filter(|_| url.scheme() != "ssh") {
        Some(port) => format!("{hostname}:{port}"),
        None => hostname.clone(),
    };
    Some(Host { portable, hostname })
}

pub(super) fn provider(value: &str) -> Option<&'static str> {
    let host = host(value)?;
    let hostname = host.hostname.as_str();
    let label = |label| hostname.split('.').any(|part| part == label);
    Some(match hostname {
        "origin.cursor.com" => "cursor-origin",
        "git.cafe" | "staging.git.cafe" => "gitcafe",
        "source.tanstack.com" => "tanstack-source",
        _ if label("github") || hostname.ends_with(".ghe.com") => "github",
        _ if label("gitlab") => "gitlab",
        _ if hostname == "codeberg.org" || label("forgejo") || label("gitea") => "forgejo",
        _ if label("bitbucket") || hostname.ends_with(".bitbucket.org") => "bitbucket",
        _ if azure_host(hostname) => "azure-devops",
        _ => "unknown",
    })
}

fn azure_host(hostname: &str) -> bool {
    hostname == "dev.azure.com"
        || hostname.ends_with(".dev.azure.com")
        || hostname.ends_with(".visualstudio.com")
}

pub(super) fn public_host(hostname: &str) -> bool {
    const HOSTS: &[&str] = &[
        "github.com",
        "origin.cursor.com",
        "gitlab.com",
        "git.cafe",
        "source.tanstack.com",
        "codeberg.org",
        "bitbucket.org",
        "dev.azure.com",
    ];
    HOSTS.iter().any(|host| hostname.eq_ignore_ascii_case(host))
}
