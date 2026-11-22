use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use t3_rune::providers::codex::{
    ApprovalRequest, SettlementStatus, ToolItem, ToolLifecycle, TurnKey, WebSearchUpdate,
};
use time::OffsetDateTime;

use super::provider_turn::{provider_turn_id, terminal_status};
use super::runtime_requests::REQUEST_BINDING;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    encode_id_part, record_projected_sequence, timestamp,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeCommand {
    command: String,
    status: NativeStatus,
    aggregated_output: Option<String>,
    exit_code: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum NativeStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

impl NativeStatus {
    fn projected(&self) -> &'static str {
        match self {
            Self::InProgress => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "cancelled",
        }
    }
}

#[derive(Deserialize)]
struct NativeFileChange {
    changes: Vec<FilePatch>,
    status: NativeStatus,
}

#[derive(Deserialize)]
struct FilePatch {
    path: String,
    kind: PatchKind,
    diff: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PatchKind {
    Add,
    Delete,
    Update { move_path: Option<String> },
}

impl PatchKind {
    fn operation(&self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Delete => "delete",
            Self::Update { .. } => "update",
        }
    }
}

pub(in crate::server) fn file_approval_prompt(
    request: &ApprovalRequest,
    file_item: Option<&ToolItem>,
) -> Result<Option<String>, TurnCommandError> {
    if let Some(reason) = request
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Ok(Some(reason.to_owned()));
    }
    if let Some(item) = file_item {
        let NativeFileChange { mut changes, .. } = serde_json::from_value(item.payload.clone())?;
        if let Some(collator) = super::super::locale::path_collator() {
            changes.sort_by(|left, right| collator.compare(&left.path, &right.path));
        }
        if !changes.is_empty() {
            let mut descriptions: Vec<_> = changes
                .iter()
                .take(20)
                .map(|change| {
                    let operation = change.kind.operation();
                    match &change.kind {
                        PatchKind::Update {
                            move_path: Some(destination),
                        } if !destination.is_empty() => {
                            format!("{operation} {} -> {destination}", change.path)
                        }
                        _ => format!("{operation} {}", change.path),
                    }
                })
                .collect();
            if changes.len() > descriptions.len() {
                descriptions.push(format!("+{} more", changes.len() - descriptions.len()));
            }
            return Ok(Some(descriptions.join(
                "
",
            )));
        }
    }
    Ok(request
        .grant_root
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned))
}

fn bound_diff(diff: String) -> String {
    const MAX_BYTES: usize = 32_768;
    const MARKER: &str = "
… output truncated for transport";
    if diff.len() <= MAX_BYTES {
        return diff;
    }
    let mut end = MAX_BYTES - MARKER.len();
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &diff[..end])
}

struct ToolDetails {
    status: NativeStatus,
    fields: Map<String, Value>,
}

fn tool_details(item: &ToolItem) -> Result<Option<ToolDetails>, TurnCommandError> {
    let (status, details) = match item.kind.as_str() {
        "commandExecution" => {
            let native: NativeCommand = serde_json::from_value(item.payload.clone())?;
            let mut details = json!({
                "type": "command_execution",
                "input": native.command,
                "completionWake": "always",
            });
            if let Some(output) = native.aggregated_output {
                details["output"] = json!(output);
            }
            if let Some(exit_code) = native.exit_code {
                details["exitCode"] = json!(exit_code);
            }
            (native.status, details)
        }
        "fileChange" => {
            let native: NativeFileChange = serde_json::from_value(item.payload.clone())?;
            let Some(first) = native.changes.first() else {
                return Ok(None);
            };
            (
                native.status,
                json!({
                    "type": "file_change",
                    "fileName": first.path,
                    "diffStr": bound_diff(first.diff.clone()),
                    "changes": native.changes.iter().map(|change| {
                        let operation = change.kind.operation();
                        match &change.kind {
                            PatchKind::Update { move_path: Some(destination) } if !destination.is_empty() =>
                                json!({"operation":operation,"path":destination,"oldPath":change.path}),
                            _ => json!({"operation":operation,"path":change.path}),
                        }
                    }).collect::<Vec<_>>(),
                }),
            )
        }
        _ => {
            return Err(TurnCommandError::Unsupported(
                "This Codex tool type does not have a projection.",
            ));
        }
    };
    if let Value::Object(details) = details {
        Ok(Some(ToolDetails {
            status,
            fields: details,
        }))
    } else {
        Err(TurnCommandError::Unsupported(
            "Invalid Codex tool projection.",
        ))
    }
}

pub(in crate::server) fn command_item_id(native_item_id: &str) -> String {
    format!(
        "turn-item:provider:codex:native-item:{}",
        encode_id_part(native_item_id)
    )
}

pub(in crate::server) struct ClosedTools {
    pub(in crate::server) sequence: i64,
    pub(in crate::server) command_item_ids: Vec<String>,
}

impl TurnCommands {
    pub(in crate::server) fn close_native_tools(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        status: SettlementStatus,
        completed_at: Option<OffsetDateTime>,
    ) -> Result<Option<ClosedTools>, TurnCommandError> {
        let status = terminal_status(status);
        let turn_id = provider_turn_id(key);
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_id = transaction
            .query_row(
                REQUEST_BINDING,
                params![
                    thread_id,
                    run_id,
                    turn_id,
                    key.thread_id.as_str(),
                    key.turn_id.as_str(),
                    status
                ],
                |row| row.get::<_, String>(2),
            )
            .optional()?;
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let closed_at = timestamp(completed_at.unwrap_or_else(OffsetDateTime::now_utc));
        let items = {
            let mut select = transaction.prepare(
                "SELECT item.turn_item_id, node.node_id, item.payload_json, node.payload_json, item.type
                 FROM orchestration_v2_projection_turn_items AS item
                 JOIN orchestration_v2_projection_nodes AS node ON node.node_id = item.node_id
                   AND node.thread_id = item.thread_id AND node.run_id = item.run_id
                 WHERE item.thread_id = ?1 AND item.run_id = ?2 AND item.provider_turn_id = ?3
                   AND item.status = 'running'
                   AND (item.type = 'web_search' OR (?4 <> 'completed' AND item.type = 'command_execution'))
                 ORDER BY item.type, item.ordinal",
            )?;
            select
                .query_map(params![thread_id, run_id, turn_id, status], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let stream = ThreadEventStream::provider(&transaction, thread_id, &session_id);
        let mut sequence = None;
        let mut command_item_ids = Vec::new();
        for (item_id, node_id, item, node, kind) in items {
            let mut item: Map<String, Value> = serde_json::from_str(&item)?;
            let mut node: Map<String, Value> = serde_json::from_str(&node)?;
            let tool_status = if kind == "web_search" && status == "completed" {
                "cancelled"
            } else {
                status
            };
            for object in [&mut item, &mut node] {
                object.insert("status".into(), json!(tool_status));
                object.insert("completedAt".into(), json!(closed_at));
            }
            item.insert("updatedAt".into(), json!(closed_at));
            if kind == "command_execution" {
                item.remove("exitCode");
                item.remove("completionWake");
                command_item_ids.push(item_id);
            }
            let metadata = json!({"runId":run_id,"nodeId":node_id,"driver":"codex","providerInstanceId":CODEX_INSTANCE});
            stream.append("node.updated", &closed_at, &metadata, &Value::Object(node))?;
            sequence = Some(stream.append(
                "turn-item.updated",
                &closed_at,
                &metadata,
                &Value::Object(item),
            )?);
        }
        let Some(sequence) = sequence else {
            return Ok(None);
        };
        record_projected_sequence(&transaction, sequence, &closed_at)?;
        transaction.commit()?;
        Ok(Some(ClosedTools {
            sequence,
            command_item_ids,
        }))
    }

    pub(in crate::server) fn record_native_tool(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        item: &ToolItem,
    ) -> Result<Option<i64>, TurnCommandError> {
        let Some(details) = tool_details(item)? else {
            return Ok(None);
        };
        self.record_tool(
            thread_id,
            run_id,
            key,
            &item.id,
            item.started_at_ms,
            details,
        )
    }

    pub(in crate::server) fn record_web_search(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        update: WebSearchUpdate,
    ) -> Result<Option<i64>, TurnCommandError> {
        let status = match update.lifecycle {
            ToolLifecycle::Started => NativeStatus::InProgress,
            ToolLifecycle::Completed => NativeStatus::Completed,
        };
        let mut fields = Map::new();
        fields.insert("type".into(), json!("web_search"));
        if !update.patterns.is_empty() {
            fields.insert("patterns".into(), json!(update.patterns));
        }
        self.record_tool(
            thread_id,
            run_id,
            key,
            &update.item_id,
            update.started_at_ms,
            ToolDetails { status, fields },
        )
    }

    fn record_tool(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        native_item_id: &str,
        started_at_ms: Option<i64>,
        details: ToolDetails,
    ) -> Result<Option<i64>, TurnCommandError> {
        let turn_id = provider_turn_id(key);
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding = transaction
            .query_row(
                REQUEST_BINDING,
                params![
                    thread_id,
                    run_id,
                    turn_id,
                    key.thread_id.as_str(),
                    key.turn_id.as_str(),
                    "running"
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((root_node_id, provider_thread_id, session_id, run_ordinal)) = binding else {
            return Ok(None);
        };
        let node_id = format!(
            "node:provider:codex:native-item:{}",
            encode_id_part(native_item_id)
        );
        let item_id = command_item_id(native_item_id);
        let existing = transaction.query_row(
            "SELECT ordinal, status, json_extract(payload_json, '$.startedAt')
             FROM orchestration_v2_projection_turn_items WHERE thread_id = ?1 AND turn_item_id = ?2",
            params![thread_id, item_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)),
        ).optional()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let (ordinal, started_at) = match existing {
            Some((_, status, _)) if status != "running" => return Ok(None),
            Some((ordinal, _, started_at)) => (ordinal, started_at.unwrap_or_else(|| now.clone())),
            None => (
                allocate_turn_item_in_run(&transaction, thread_id, &item_id, run_ordinal)?,
                started_at_ms
                    .and_then(|value| {
                        OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1_000_000)
                            .ok()
                    })
                    .map(timestamp)
                    .unwrap_or_else(|| now.clone()),
            ),
        };
        let status = details.status.projected();
        let completed_at = (status != "running").then_some(now.as_str());
        let native_ref =
            json!({ "driver": "codex", "nativeId": native_item_id, "strength": "strong" });
        let metadata = json!({
            "runId": run_id,
            "nodeId": node_id,
            "driver": "codex",
            "providerInstanceId": CODEX_INSTANCE,
        });
        let stream = ThreadEventStream::provider(&transaction, thread_id, &session_id);
        stream.append(
            "node.updated",
            &now,
            &metadata,
            &json!({
                "id": node_id,
                "threadId": thread_id,
                "runId": run_id,
                "parentNodeId": root_node_id,
                "rootNodeId": root_node_id,
                "kind": "tool_call",
                "status": status,
                "countsForRun": false,
                "providerThreadId": provider_thread_id,
                "providerTurnId": turn_id,
                "nativeItemRef": native_ref,
                "runtimeRequestId": null,
                "checkpointScopeId": null,
                "startedAt": started_at,
                "completedAt": completed_at,
            }),
        )?;
        let mut projection = json!({
            "id": item_id,
            "threadId": thread_id,
            "runId": run_id,
            "nodeId": node_id,
            "providerThreadId": provider_thread_id,
            "providerTurnId": turn_id,
            "nativeItemRef": native_ref,
            "parentItemId": null,
            "ordinal": ordinal,
            "status": status,
            "title": null,
            "startedAt": started_at,
            "completedAt": completed_at,
            "updatedAt": now,
        });
        if let Some(projection) = projection.as_object_mut() {
            projection.extend(details.fields);
        }
        let sequence = stream.append("turn-item.updated", &now, &metadata, &projection)?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}
