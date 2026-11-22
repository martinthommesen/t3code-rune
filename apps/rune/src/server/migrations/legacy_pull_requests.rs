use reqwest::Url;
use rusqlite::{ToSql, Transaction, params, types::FromSql};
use serde::{Deserialize, Serialize};

const PAGE_SIZE: usize = 500;
pub(super) const MAX_SAFE_NUMBER: u64 = 9_007_199_254_740_991;

pub(super) fn migrate_64(database: &Transaction<'_>) -> rusqlite::Result<()> {
    rewrite_links(
        database,
        "SELECT thread_id, json_extract(payload_json, '$.linkedPullRequest'), updated_at
         FROM orchestration_v2_projection_threads
         WHERE thread_id > ?1 AND json_valid(payload_json)
           AND json_type(payload_json, '$.pullRequests') IS NULL
           AND json_type(payload_json, '$.linkedPullRequest') = 'object'
         ORDER BY thread_id LIMIT ?2",
        "UPDATE orchestration_v2_projection_threads
         SET payload_json = json_set(payload_json, '$.pullRequests', json(?2))
         WHERE thread_id = ?1",
        String::new(),
    )?;
    rewrite_links(
        database,
        "SELECT sequence, json_extract(payload_json, '$.linkedPullRequest'),
                json_extract(payload_json, '$.updatedAt')
         FROM orchestration_events
         WHERE sequence > ?1 AND application_event_version = 2 AND aggregate_kind = 'thread'
           AND event_type LIKE 'thread.%' AND json_valid(payload_json)
           AND json_type(payload_json, '$.pullRequests') IS NULL
           AND json_type(payload_json, '$.linkedPullRequest') = 'object'
         ORDER BY sequence LIMIT ?2",
        "UPDATE orchestration_events
         SET payload_json = json_set(payload_json, '$.pullRequests', json(?2))
         WHERE sequence = ?1",
        0_i64,
    )
}

fn rewrite_links<Id: FromSql + ToSql>(
    database: &Transaction<'_>,
    select: &str,
    update: &str,
    mut after: Id,
) -> rusqlite::Result<()> {
    let mut select = database.prepare(select)?;
    let mut update = database.prepare(update)?;
    loop {
        let mut rows = select
            .query_map(params![after, PAGE_SIZE], |row| {
                Ok((
                    row.get::<_, Id>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, link, at) in &rows {
            update.execute(params![id, encode_link(link, at)?])?;
        }
        if rows.len() < PAGE_SIZE {
            return Ok(());
        }
        after = rows.pop().expect("a full migration page").0;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyLink {
    project_id: String,
    repository: String,
    number: u64,
    url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Link<'a> {
    host: String,
    repository: String,
    number: u64,
    url: &'a str,
    source: &'static str,
    linked_at: &'a str,
    snapshot: Option<()>,
    stack: Option<()>,
}

fn encode_link(legacy: &str, at: &str) -> rusqlite::Result<String> {
    let error =
        |cause| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, cause);
    let legacy: LegacyLink =
        serde_json::from_str(legacy).map_err(|cause| error(Box::new(cause)))?;
    let url = trim(&legacy.url);
    let repository = trim(&legacy.repository);
    if trim(&legacy.project_id).is_empty()
        || repository.is_empty()
        || url.is_empty()
        || legacy.number == 0
        || legacy.number > MAX_SAFE_NUMBER
    {
        return Err(error("Invalid legacy pull request link".into()));
    }
    let (host, repository) = legacy_key(repository, url, Some(legacy.number));
    serde_json::to_string(&[Link {
        host,
        repository,
        number: legacy.number,
        url,
        source: "manual",
        linked_at: at,
        snapshot: None,
        stack: None,
    }])
    .map_err(|cause| error(Box::new(cause)))
}

pub(super) fn legacy_key(
    repository: &str,
    url: &str,
    number: Option<u64>,
) -> (String, String) {
    let parsed = Url::parse(url).ok();
    parsed
        .as_ref()
        .zip(number)
        .and_then(|(parsed, number)| keyed_authority(parsed, number))
        .unwrap_or_else(|| {
            let host = parsed.as_ref().and_then(Url::host_str).unwrap_or_default();
            let host = if host.is_empty() { "unknown" } else { host };
            (host.to_lowercase(), trim(repository).to_lowercase())
        })
}

pub(super) fn trim(value: &str) -> &str {
    value.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
}

fn keyed_authority(url: &Url, expected: u64) -> Option<(String, String)> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let path: Vec<&str> = url.path().strip_prefix('/')?.split('/').collect();
    let has_host = |apex: &str, label| {
        host == apex
            || host.ends_with(&format!(".{apex}"))
            || host.split('.').any(|part| part == label)
    };
    let is_git_cafe = matches!(host.as_str(), "git.cafe" | "staging.git.cafe");
    if is_git_cafe {
        if has_fixed_owner(&path, "pull") || has_fixed_owner(&path, "pulls") {
            return None;
        }
    } else {
        if has_host("github.com", "github") && has_fixed_owner(&path, "pull") {
            return None;
        }
        if let Some((repository, number)) = change_request(&path, "pulls", |_| true) {
            return (number == Some(expected))
                .then(|| canonical_repository(authority(&host, url), repository));
        }
        if path.iter().enumerate().skip(2).any(|(index, segment)| {
            *segment == "-"
                && path[..index].iter().all(|part| !part.is_empty())
                && path.get(index + 1) == Some(&"merge_requests")
                && path
                    .get(index + 2)
                    .is_some_and(|segment| decimal_id(segment))
        }) || (has_host("bitbucket.org", "bitbucket") && has_fixed_owner(&path, "pull-requests"))
        {
            return None;
        }
    }
    let (repository, number) = change_request(&path, "pullrequest", |prefix| {
        prefix.iter().rev().nth(1) == Some(&"_git")
    })?;
    if number != Some(expected) {
        return None;
    }
    if url.port().is_some() {
        return Some(canonical_repository(authority(&host, url), repository));
    }
    let canonical = canonical_repository(host, repository);
    (canonical.0 == "dev.azure.com").then_some(canonical)
}

fn canonical_repository(host: String, repository: String) -> (String, String) {
    let parts: Vec<&str> = repository.split('/').collect();
    if matches!(host.as_str(), "ssh.dev.azure.com" | "vs-ssh.visualstudio.com")
        && let ["v3", account, project, name] = parts.as_slice()
    {
        return ("dev.azure.com".into(), format!("{account}/{project}/_git/{name}"));
    }
    if let Some(account) = host
        .strip_suffix(".visualstudio.com")
        .filter(|account| !account.is_empty() && !account.contains('.'))
    {
        let path = match parts.as_slice() {
            ["defaultcollection", project, "_git", name] | [project, "_git", name] => {
                Some(format!("{account}/{project}/_git/{name}"))
            }
            _ => None,
        };
        if let Some(path) = path {
            return ("dev.azure.com".into(), path);
        }
    }
    (host, repository)
}

fn authority(host: &str, url: &Url) -> String {
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn has_fixed_owner(path: &[&str], marker: &str) -> bool {
    matches!(path, [owner, repo, name, id, ..]
        if !owner.is_empty() && !repo.is_empty() && *name == marker && decimal_id(id))
}

fn change_request(
    path: &[&str],
    marker: &str,
    matches_path: impl Fn(&[&str]) -> bool,
) -> Option<(String, Option<u64>)> {
    let index = (2..path.len().saturating_sub(1)).rev().find(|&index| {
        path[index] == marker
            && decimal_id(path[index + 1])
            && path[..index].iter().all(|part| !part.is_empty())
            && matches_path(&path[..index])
    })?;
    let id = path[index + 1]
        .parse::<u64>()
        .ok()
        .filter(|number| (1..=MAX_SAFE_NUMBER).contains(number));
    Some((path[..index].join("/").to_lowercase(), id))
}

fn decimal_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}
