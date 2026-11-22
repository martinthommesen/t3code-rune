use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{PROTOCOL_VERSION, SharedEnvironment, bearer_token, repository_identity};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ShellQuery {
    resume_from: Option<String>,
    omit_idle_subagents: Option<String>,
    omit_older_settled: Option<String>,
}

pub(super) async fn snapshot(
    State(environment): State<SharedEnvironment>,
    Query(query): Query<ShellQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let local = environment.local.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let token = bearer_token(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    if !environment
        .credentials
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .authorizes(token, Instant::now())
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let version = PROTOCOL_VERSION.to_string();
    if headers
        .get("x-t3-orchestration-protocol")
        .is_none_or(|header| header.as_bytes() != version.as_bytes())
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let local = Arc::clone(local);
    let store = environment.store.clone();
    let task = tokio::task::spawn_blocking(move || {
        if let Some(cursor) = query.resume_from.as_deref().and_then(resume_cursor)
            && store.can_resume_shell(cursor)?
        {
            return Ok(json!({"resumable": true}));
        }
        let (mut shell, _) = store.live_subscriber_shell_snapshot(
            query.omit_idle_subagents.as_deref() == Some("1"),
            query.omit_older_settled.as_deref() == Some("1"),
        )?;
        let mut snapshot = shell["snapshot"].take();
        let projects = snapshot["projects"]
            .as_array_mut()
            .expect("project snapshot");
        repository_identity::enrich_projects(projects, &local.repository_identities);
        snapshot["projectIdentities"] = json!(repository_identity::digest_projects(projects));
        Ok::<_, rusqlite::Error>(snapshot)
    });
    task.await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn resume_cursor(raw: &str) -> Option<u64> {
    let raw =
        raw.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}');
    let number = if let Some(raw) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u64::from_str_radix(raw, 16).ok()? as f64
    } else if let Some(raw) = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")) {
        u64::from_str_radix(raw, 8).ok()? as f64
    } else if let Some(raw) = raw.strip_prefix("0b").or_else(|| raw.strip_prefix("0B")) {
        u64::from_str_radix(raw, 2).ok()? as f64
    } else if raw.is_empty() {
        0.0
    } else {
        raw.parse::<f64>().ok()?
    };
    ((0.0..=9_007_199_254_740_991.0).contains(&number) && number.fract() == 0.0)
        .then_some(number as u64)
}
