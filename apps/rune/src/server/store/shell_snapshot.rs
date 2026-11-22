use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Params, Row, types::Type};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

mod background;
mod trim;

const THREAD_COLUMNS: &str = include_str!("shell_threads.sql");
const THREADS: &str = "WHERE t.deleted_at IS NULL AND (t.archived_at IS NOT NULL) = ?1
    ORDER BY t.updated_at ASC, t.thread_id ASC";
const SELECTED_ACTIVE_THREADS: &str = "WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
    AND t.thread_id IN (SELECT value FROM json_each(?1))
    ORDER BY t.updated_at ASC, t.thread_id ASC";
const FAMILY_THREADS: &str = "WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
    AND CASE WHEN json_valid(t.payload_json)
        THEN json_extract(t.payload_json, '$.lineage.rootThreadId') END = (
        SELECT json_extract(member.payload_json, '$.lineage.rootThreadId')
        FROM orchestration_v2_projection_threads member WHERE member.thread_id = ?1
    )
    ORDER BY t.updated_at ASC, t.thread_id ASC";
const WORKTREE_THREAD_IDS: &str = "SELECT t.thread_id FROM orchestration_v2_projection_threads t
    WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
      AND CASE WHEN json_valid(t.payload_json)
        THEN json_extract(t.payload_json, '$.worktreePath') END = ?1
    ORDER BY t.updated_at ASC, t.thread_id ASC";
const ACTIVE_THREAD: &str = "WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
    AND t.thread_id = ?1";
const STORAGE_THREADS: &str = "WHERE t.deleted_at IS NULL ORDER BY t.created_at, t.thread_id";
const PROJECTS: &str = "
    SELECT project_id, title, has_custom_title, workspace_root, default_model_selection_json,
           default_thread_env_mode, auto_pull, favicon_path, project_icon_json, scripts_json,
           tracked_folders_json, pinned_at, archived_at, created_at, updated_at
    FROM projection_projects WHERE deleted_at IS NULL";

pub(super) fn read_archive(
    database: &Connection,
    snapshot_sequence: u64,
) -> rusqlite::Result<Value> {
    let projects = read_projects(database)?;
    let threads = read_threads(database, THREADS, [true])?;
    Ok(json!({
        "schemaVersion": 2,
        "snapshotSequence": snapshot_sequence,
        "projects": projects,
        "threads": threads,
    }))
}

pub(super) fn read_storage_threads(database: &Connection) -> rusqlite::Result<Vec<Value>> {
    read_threads(database, STORAGE_THREADS, [])
}

pub(super) fn read_subscriber(
    database: &Connection,
    snapshot_sequence: u64,
    omit_idle_subagents: bool,
    omit_older_settled: bool,
) -> rusqlite::Result<(Value, HashSet<String>)> {
    let projects = read_projects(database)?;
    let options = trim::Options {
        omit_idle_subagents,
        omit_older_settled,
    };
    let (threads, active_subagents, older_settled) = if omit_idle_subagents || omit_older_settled {
        let plan = trim::Plan::read(database, options)?;
        let mut threads = read_selected_active_threads(database, &plan.ids)?;
        let older_settled = plan.apply(&mut threads, snapshot_sequence);
        (threads, plan.active_subagents, older_settled)
    } else {
        let threads = read_threads(database, THREADS, [false])?;
        let active_subagents = threads
            .iter()
            .filter(|thread| trim::subagent(thread))
            .filter_map(|thread| thread["id"].as_str().map(str::to_owned))
            .collect();
        (threads, active_subagents, None)
    };
    let mut snapshot = json!({
        "schemaVersion": 2,
        "snapshotSequence": snapshot_sequence,
        "projects": projects,
        "threads": threads,
        "archivedThreads": [],
    });
    if let Some(older_settled) = older_settled {
        snapshot["olderSettled"] = older_settled;
    }
    Ok((
        json!({"kind": "snapshot", "snapshot": snapshot}),
        active_subagents,
    ))
}

pub(super) fn read_selected_active_threads(
    database: &Connection,
    ids: &[String],
) -> rusqlite::Result<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(ids).expect("serializable thread IDs");
    read_threads(database, SELECTED_ACTIVE_THREADS, [ids])
}

pub(super) fn read_family_threads(
    database: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<Value>> {
    read_threads(database, FAMILY_THREADS, [thread_id])
}

pub(super) fn read_worktree_thread_ids(
    database: &Connection,
    path: &str,
) -> rusqlite::Result<Vec<String>> {
    database
        .prepare(WORKTREE_THREAD_IDS)?
        .query_map([path], |row| row.get(0))?
        .collect()
}

fn read_threads(
    database: &Connection,
    filter: &str,
    params: impl Params,
) -> rusqlite::Result<Vec<Value>> {
    database
        .prepare(&format!("{THREAD_COLUMNS}{filter}"))?
        .query_map(params, |row| thread(database, row))?
        .collect()
}

pub(super) fn read_projects(database: &Connection) -> rusqlite::Result<Vec<Value>> {
    database
        .prepare(&format!("{PROJECTS} ORDER BY created_at, project_id"))?
        .query_map([], project)?
        .collect()
}

pub(super) fn read_project(database: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    database
        .prepare_cached(&format!("{PROJECTS} AND project_id = ?1"))?
        .query_row([id], project)
        .optional()
}

pub(super) fn read_active_thread(
    database: &Connection,
    id: &str,
) -> rusqlite::Result<Option<Value>> {
    database
        .prepare_cached(&format!("{THREAD_COLUMNS}{ACTIVE_THREAD}"))?
        .query_row([id], |row| thread(database, row))
        .optional()
}

fn project(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut model = decode_json(row, 4)?;
    normalize_model(&mut model);
    let mut icon = decode_json(row, 8)?;
    if icon["kind"] == "lucide" {
        icon = match icon.get("monogramText").or_else(|| icon.get("monogram")) {
            Some(text) => json!({"kind": "monogram", "text": text, "color": icon["color"]}),
            None => json!({"kind": "lucide", "name": icon["name"], "color": icon["color"]}),
        };
    }
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "title": row.get::<_, String>(1)?,
        "hasCustomTitle": row.get::<_, bool>(2)?,
        "workspaceRoot": row.get::<_, String>(3)?,
        "repositoryIdentity": null,
        "defaultModelSelection": model,
        "defaultThreadEnvMode": row.get::<_, Option<String>>(5)?,
        "autoPull": row.get::<_, bool>(6)?,
        "faviconPath": row.get::<_, Option<String>>(7)?,
        "projectIcon": icon,
        "scripts": decode_json(row, 9)?,
        "trackedFolders": decode_json(row, 10)?,
        "pinnedAt": row.get::<_, Option<String>>(11)?,
        "archivedAt": row.get::<_, Option<String>>(12)?,
        "createdAt": row.get::<_, String>(13)?,
        "updatedAt": row.get::<_, String>(14)?,
    }))
}

const THREAD_FIELDS: &[&str] = &[
    "createdBy",
    "creationSource",
    "id",
    "projectId",
    "title",
    "providerInstanceId",
    "modelSelection",
    "runtimeMode",
    "interactionMode",
    "branch",
    "worktreePath",
    "lineage",
    "forkedFrom",
    "activeProviderThreadId",
    "createdAt",
    "updatedAt",
    "archivedAt",
    "settledOverride",
    "settledAt",
    "unsettledAt",
    "snoozedUntil",
    "snoozedAt",
    "pinnedAt",
    "autoSettleDisabledAt",
    "pinOrderKey",
    "lastVisitedAt",
    "opens",
    "titleRegeneration",
    "lastTitleRegeneration",
    "limitRecovery",
    "deletedAt",
];
const OPTIONAL_THREAD_FIELDS: &[&str] = &[
    "linkedPullRequest",
    "branchPullRequest",
    "issues",
    "activeOrderKey",
    "historyOrigin",
    "linkOrigin",
    "delegatedFrom",
];
const THREAD_DATES: &[&str] = &[
    "createdAt",
    "updatedAt",
    "archivedAt",
    "settledAt",
    "unsettledAt",
    "snoozedUntil",
    "snoozedAt",
    "pinnedAt",
    "autoSettleDisabledAt",
    "lastVisitedAt",
    "deletedAt",
];

fn thread(database: &Connection, row: &Row<'_>) -> rusqlite::Result<Value> {
    let id: String = row.get("thread_id")?;
    let raw = decode_json(row, row.as_ref().column_index("payload_json")?)?;
    let mut fields: Map<String, Value> = THREAD_FIELDS
        .iter()
        .map(|key| ((*key).to_owned(), raw[*key].clone()))
        .collect();
    for &key in OPTIONAL_THREAD_FIELDS {
        if let Some(field) = raw.get(key) {
            fields.insert(key.into(), field.clone());
        }
    }
    for &key in THREAD_DATES {
        if let Some(text) = fields.get(key).and_then(Value::as_str) {
            fields.insert(key.into(), timestamp(text).into());
        }
    }
    normalize_model(
        fields
            .get_mut("modelSelection")
            .expect("thread model field"),
    );
    if raw["tags"]
        .as_array()
        .is_some_and(|tags| !tags.is_empty() && tags.iter().all(Value::is_string))
    {
        fields.insert("tags".into(), raw["tags"].clone());
    }
    let pull_requests = raw["pullRequests"].as_array().cloned().unwrap_or_default();
    fields.insert("pullRequests".into(), json!(pull_requests));

    let session_error: Option<String> = row.get("last_error")?;
    let blocking_failure = decode_column(row, "blocking_failure_payload_json")?;
    let blocked = !blocking_failure.is_null()
        && failure_matches(&blocking_failure["failure"], session_error.as_deref())["class"]
            == "usage_limit";
    let failure = if blocked {
        blocking_failure
    } else {
        decode_column(row, "terminal_failure_payload_json")?
    };
    let failure = &failure["failure"];
    let current_failure = failure_matches(failure, session_error.as_deref());
    let run_prefix = if blocked {
        "blocking_run"
    } else {
        "latest_run"
    };
    let stored_status: Option<String> = row.get("latest_run_status")?;
    let status = if blocked {
        "failed"
    } else {
        shell_status(stored_status.as_deref())
    };
    let latest_run_id: Option<String> = row.get(format!("{run_prefix}_id").as_str())?;
    let active_run_id: Option<String> = row.get("active_run_id")?;
    let active_run_status: Option<String> = row.get("active_run_status")?;
    let latest_user_authored = date_column(row, "latest_user_authored_message_at")?;
    let pending = pending_request(row, latest_user_authored.as_deref())?;
    let providers = providers(database, &id)?;
    let history = provider_history(&providers, &id);
    let goal = providers
        .iter()
        .find(|provider| provider["id"] == raw["activeProviderThreadId"])
        .map_or(Value::Null, |provider| provider["goal"].clone());
    let (background, thread_wait) = background::read(
        database,
        &id,
        status,
        active_run_id.as_deref(),
        active_run_status.as_deref(),
        &raw,
        &providers,
    )?;
    let item_count = item_count(database, &id)?;
    let visible_item_count = item_count + inherited_count(database, &raw)?;
    let guests = raw["guestShare"]["messages"]
        .as_array()
        .iter()
        .flat_map(|items| items.iter())
        .filter(|message| message["status"] == "pending")
        .count();
    let derived = json!({
        "latestRunId": latest_run_id,
        "latestRunRequestedAt": date_column(row, &format!("{run_prefix}_requested_at"))?,
        "latestRunStartedAt": date_column(row, &format!("{run_prefix}_started_at"))?,
        "latestRunCompletedAt": date_column(row, &format!("{run_prefix}_completed_at"))?,
        "latestRunStopRequested": row.get::<_, bool>(format!("{run_prefix}_stop_requested").as_str())?,
        "latestRunBackgroundWake": row.get::<_, bool>(format!("{run_prefix}_background_wake").as_str())?,
        "activeRunId": active_run_id,
        "activityRunStartedAt": date_column(row, "activity_run_started_at")?,
        "activityRunStatus": row.get::<_, Option<String>>("activity_run_status")?,
        "blockedOnThreadWait": thread_wait,
        "status": status,
        "lastError": session_error.as_deref().or_else(|| failure["message"].as_str()),
        "lastErrorClass": current_failure["class"],
        "lastErrorCode": current_failure["code"],
        "usageLimitResetAt": if current_failure["class"] == "usage_limit" { &current_failure["resetAt"] } else { &Value::Null },
        "pendingRuntimeRequest": pending,
        "pendingGuestMessageCount": guests,
        "latestVisibleMessage": null,
        "latestUserMessageAt": date_column(row, "latest_user_message_at")?,
        "latestUserAuthoredMessageAt": latest_user_authored,
        "hasActionableProposedPlan": row.get::<_, bool>("has_actionable_proposed_plan")?,
        "pendingBackgroundTasks": background,
        "providerInstanceHistory": history,
        "goal": goal,
        "itemCount": item_count,
        "visibleItemCount": visible_item_count,
    });
    if let Value::Object(derived) = derived {
        fields.extend(derived);
    }
    Ok(fields.into())
}

fn failure_matches<'a>(failure: &'a Value, session_error: Option<&str>) -> &'a Value {
    if session_error.is_some_and(|error| failure["message"] != error) {
        &Value::Null
    } else {
        failure
    }
}

fn pending_request(row: &Row<'_>, latest_user_authored: Option<&str>) -> rusqlite::Result<Value> {
    let request = decode_column(row, "pending_request_payload_json")?;
    let fallback = || {
        if request.is_null() {
            Value::Null
        } else {
            json!({
                "id": request["id"], "kind": request["kind"],
                "createdAt": timestamp_value(&request["createdAt"]),
                "blocking": request["responseCapability"]["type"] != "message",
            })
        }
    };
    if !request.is_null() && request["kind"] != "user_input" {
        return Ok(fallback());
    }
    let secret = decode_column(row, "pending_secret_request_payload_json")?;
    let link = decode_column(row, "pending_link_request_payload_json")?;
    let link_created = card_created(&link);
    let link = if latest_user_authored.is_some_and(|user| user > link_created.as_str()) {
        &Value::Null
    } else {
        &link
    };
    let card = if timestamp_value(&secret["updatedAt"]).as_str()
        >= timestamp_value(&link["updatedAt"]).as_str()
    {
        &secret
    } else {
        link
    };
    let created = card_created(card);
    if card["nodeId"].is_null()
        || timestamp_value(&request["createdAt"])
            .as_str()
            .is_some_and(|request| request > created.as_str())
    {
        return Ok(fallback());
    }
    Ok(
        json!({"id": card["id"], "kind": "user_input", "createdAt": created, "blocking": true, "userOnly": true}),
    )
}

fn card_created(card: &Value) -> String {
    timestamp(
        card["startedAt"]
            .as_str()
            .or_else(|| card["updatedAt"].as_str())
            .unwrap_or_default(),
    )
}

fn shell_status(status: Option<&str>) -> &str {
    match status {
        None => "idle",
        Some(
            status @ ("preparing" | "queued" | "starting" | "running" | "waiting" | "completed"
            | "interrupted" | "failed" | "cancelled" | "rolled_back"),
        ) => status,
        Some(_) => "failed",
    }
}

fn providers(database: &Connection, id: &str) -> rusqlite::Result<Vec<Value>> {
    database.prepare("SELECT payload_json FROM orchestration_v2_projection_provider_threads WHERE thread_id = ?1")?
        .query_map([id], |row| decode_json(row, 0))?.collect()
}

fn provider_history<'a>(providers: &'a [Value], id: &str) -> Vec<&'a str> {
    let mut local: Vec<_> = providers
        .iter()
        .filter(|provider| provider["appThreadId"] == id && provider["ownerNodeId"].is_null())
        .collect();
    local.sort_by_key(|provider| (provider["createdAt"].as_str(), provider["id"].as_str()));
    let mut seen = HashSet::new();
    local
        .into_iter()
        .filter_map(|provider| provider["providerInstanceId"].as_str())
        .filter(|instance| seen.insert(*instance))
        .collect()
}

fn item_count(database: &Connection, id: &str) -> rusqlite::Result<u64> {
    database
        .query_row(include_str!("shell_item_count.sql"), [id], |row| row.get(0))
        .optional()
        .map(Option::unwrap_or_default)
}

fn inherited_count(database: &Connection, thread: &Value) -> rusqlite::Result<u64> {
    let mut current = thread.clone();
    let mut seen = HashSet::new();
    let mut count = 0;
    while current["forkedFrom"]["type"] == "run" && current["lineage"]["sideChat"] != true {
        let fork = &current["forkedFrom"];
        let (Some(next), Some(run), Some(created)) = (
            fork["threadId"].as_str(),
            fork["runId"].as_str(),
            current["createdAt"].as_str(),
        ) else {
            break;
        };
        if let Some(id) = current["id"].as_str() {
            seen.insert(id.to_owned());
        }
        let Some(source) = database
            .query_row(
                "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
                [next],
                |row| decode_json(row, 0),
            )
            .optional()?
        else {
            break;
        };
        let item_count: u64 = database.query_row(
            include_str!("shell_inherited_count.sql"),
            rusqlite::params![next, run, created, source["historyOrigin"] == "v1_import"],
            |row| row.get(0),
        )?;
        count += item_count + 1;
        if seen.contains(next) {
            break;
        }
        current = source;
    }
    Ok(count)
}

fn decode_column(row: &Row<'_>, name: &str) -> rusqlite::Result<Value> {
    decode_json(row, row.as_ref().column_index(name)?)
}

fn decode_json(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let raw: Option<String> = row.get(index)?;
    raw.map_or(Ok(Value::Null), |raw| {
        serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        })
    })
}

fn date_column(row: &Row<'_>, column: &str) -> rusqlite::Result<Option<String>> {
    row.get::<_, Option<String>>(column)
        .map(|date| date.as_deref().map(timestamp))
}

fn timestamp_value(value: &Value) -> Value {
    value.as_str().map(timestamp).into()
}

fn timestamp(value: &str) -> String {
    let Ok(date) = OffsetDateTime::parse(value, &Rfc3339) else {
        return value.to_owned();
    };
    let date = date.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
        date.millisecond()
    )
}

fn normalize_model(value: &mut Value) {
    let Some(model) = value.as_object_mut() else {
        return;
    };
    let legacy_provider = model.remove("provider");
    if let Some(provider) = legacy_provider {
        model.entry("instanceId").or_insert(provider);
    }
    model.retain(|key, _| matches!(key.as_str(), "instanceId" | "model" | "options"));
    if let Some(Value::String(name)) = model.get_mut("model") {
        *name = name.trim().to_owned();
    }
    if model.get("options").is_some_and(Value::is_null) {
        model.remove("options");
    }
    if let Some(options) = model.get_mut("options") {
        let normalized = match options {
            Value::Object(values) => values
                .iter()
                .filter_map(|(id, value)| normalize_option(id, value))
                .collect(),
            Value::Array(values) => values
                .iter()
                .filter_map(|option| normalize_option(option["id"].as_str()?, &option["value"]))
                .collect(),
            _ => return,
        };
        *options = Value::Array(normalized);
    }
}

fn normalize_option(id: &str, value: &Value) -> Option<Value> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    match value {
        Value::String(value) if !value.trim().is_empty() => {
            Some(json!({"id": id, "value": value.trim()}))
        }
        Value::Bool(value) => Some(json!({"id": id, "value": value})),
        _ => None,
    }
}
