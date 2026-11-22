use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Row, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::store::{append_import_event, import_shell_previews, parse_timestamp};
use super::thread_commands::{normalize_model_selection, trim_js};
use super::turn_commands::{record_projected_sequence, timestamp};

mod pull_requests;
mod repair;

#[cfg(test)]
mod tests;

const LEGACY_COLUMNS: &str = "
    thread.thread_id, thread.project_id, thread.title, thread.model_selection_json,
    thread.runtime_mode, thread.interaction_mode, thread.branch, thread.worktree_path,
    thread.created_at, thread.updated_at, thread.archived_at, thread.settled_override,
    thread.settled_at, thread.unsettled_at, thread.snoozed_until, thread.snoozed_at, thread.pinned_at,
    thread.auto_settle_disabled_at, thread.pin_order_key, thread.linked_pull_request_json,
    thread.branch_pull_request_json, thread.active_order_key, thread.deleted_at,
           (SELECT json_group_array(json_object(
               'host', pr.host, 'repository', pr.repository, 'number', pr.number, 'url', pr.url,
               'source', pr.source, 'linkedAt', pr.linked_at, 'snapshot', json(pr.snapshot_json),
               'stack', json(pr.stack_json)))
            FROM projection_thread_pull_requests AS pr WHERE pr.thread_id = thread.thread_id)";

const MISSING_SHELLS: &str = "
    FROM projection_threads AS thread
    WHERE NOT EXISTS (
        SELECT 1 FROM orchestration_events AS event INDEXED BY orchestration_events_v2_created_threads_idx
        WHERE event.application_event_version = 2 AND event.aggregate_kind = 'thread'
          AND event.stream_id = thread.thread_id AND event.event_type = 'thread.created'
    )
    ORDER BY thread.created_at, thread.thread_id";

const STORE_THREAD: &str = "
    INSERT INTO orchestration_v2_projection_threads (
        thread_id, project_id, title, default_provider, provider_instance_id, runtime_mode,
        interaction_mode, active_provider_thread_id, created_at, updated_at, archived_at, deleted_at,
        payload_json
    ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
    ON CONFLICT(thread_id) DO UPDATE SET
        project_id = excluded.project_id,
        title = excluded.title,
        default_provider = excluded.default_provider,
        provider_instance_id = excluded.provider_instance_id,
        runtime_mode = excluded.runtime_mode,
        interaction_mode = excluded.interaction_mode,
        active_provider_thread_id = excluded.active_provider_thread_id,
        created_at = excluded.created_at,
        updated_at = excluded.updated_at,
        archived_at = excluded.archived_at,
        deleted_at = excluded.deleted_at,
        payload_json = excluded.payload_json";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) imported_thread_count: usize,
    pub(super) imported_message_count: usize,
}

/// Imports preview rows before normal thread reads can hydrate the rest of a legacy transcript.
pub(super) fn reconcile(path: &Path) -> rusqlite::Result<Summary> {
    let mut database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    database.busy_timeout(Duration::from_secs(5))?;
    reconcile_at(&mut database, OffsetDateTime::now_utc())
}

fn reconcile_at(
    database: &mut Connection,
    imported_at: OffsetDateTime,
) -> rusqlite::Result<Summary> {
    let mut summary = Summary {
        imported_thread_count: repair::reconcile(database, imported_at)?,
        imported_message_count: 0,
    };
    let missing = database
        .prepare(&format!("SELECT {LEGACY_COLUMNS} {MISSING_SHELLS}"))?
        .query_map([], LegacyThread::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for legacy in missing {
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM orchestration_events AS event
                WHERE application_event_version = 2 AND aggregate_kind = 'thread'
                  AND stream_id = ?1 AND event_type = 'thread.created'
            )",
            [&legacy.id],
            |row| row.get(0),
        )? {
            continue;
        }
        let count = legacy.import(&transaction, imported_at)?;
        transaction.commit()?;
        summary.imported_thread_count += 1;
        summary.imported_message_count += count;
    }
    Ok(summary)
}

struct LegacyThread {
    id: String,
    project_id: String,
    title: String,
    model_selection: Option<String>,
    runtime_mode: String,
    interaction_mode: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    created_at: String,
    updated_at: String,
    archived_at: Option<String>,
    settled_override: Option<String>,
    settled_at: Option<String>,
    unsettled_at: Option<String>,
    snoozed_until: Option<String>,
    snoozed_at: Option<String>,
    pinned_at: Option<String>,
    auto_settle_disabled_at: Option<String>,
    pin_order_key: Option<String>,
    linked_pull_request: Option<String>,
    branch_pull_request: Option<String>,
    active_order_key: Option<String>,
    deleted_at: Option<String>,
    pull_requests: String,
}

impl LegacyThread {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            project_id: row.get(1)?,
            title: row.get(2)?,
            model_selection: row.get(3)?,
            runtime_mode: row.get(4)?,
            interaction_mode: row.get(5)?,
            branch: row.get(6)?,
            worktree_path: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            archived_at: row.get(10)?,
            settled_override: row.get(11)?,
            settled_at: row.get(12)?,
            unsettled_at: row.get(13)?,
            snoozed_until: row.get(14)?,
            snoozed_at: row.get(15)?,
            pinned_at: row.get(16)?,
            auto_settle_disabled_at: row.get(17)?,
            pin_order_key: row.get(18)?,
            linked_pull_request: row.get(19)?,
            branch_pull_request: row.get(20)?,
            active_order_key: row.get(21)?,
            deleted_at: row.get(22)?,
            pull_requests: row.get(23)?,
        })
    }

    fn import(
        &self,
        transaction: &Transaction<'_>,
        imported_at: OffsetDateTime,
    ) -> rusqlite::Result<usize> {
        let payload = self.payload(imported_at)?;
        let thread_id = payload["id"].as_str().expect("validated thread ID");
        let event_prefix = format!("migration:v1:thread:{}", self.id);
        append_shell_event(
            transaction,
            &format!("{event_prefix}:created"),
            "thread.created",
            payload["createdAt"].as_str().expect("created timestamp"),
            &payload,
        )?;
        let count = import_shell_previews(transaction, thread_id, imported_at)?;
        let sequence = append_shell_event(
            transaction,
            &format!("{event_prefix}:shell"),
            "thread.metadata-updated",
            payload["updatedAt"].as_str().expect("updated timestamp"),
            &payload,
        )?;
        store_projection(transaction, &payload)?;
        let imported_at = timestamp(imported_at);
        record_projected_sequence(transaction, sequence, &imported_at)?;
        transaction.execute(
            "INSERT INTO orchestration_v2_legacy_imports (
                thread_id, source_updated_at, shell_imported_at, transcript_imported_at,
                imported_message_count, last_error
            ) VALUES (?1, ?2, ?3, NULL, ?4, NULL) ON CONFLICT(thread_id) DO NOTHING",
            params![thread_id, self.updated_at, imported_at, count],
        )?;
        Ok(count)
    }

    fn payload(&self, imported_at: OffsetDateTime) -> rusqlite::Result<Value> {
        let thread_id = required_id(&self.id)?;
        let project_id = required_id(&self.project_id)?;
        let title = nonempty(Some(&self.title)).unwrap_or("Untitled thread");
        let model = self
            .model_selection
            .as_deref()
            .and_then(normalize_model_selection)
            .unwrap_or_else(|| json!({"instanceId":"codex", "model":"gpt-6-astra"}));
        let provider = model["instanceId"]
            .as_str()
            .expect("normalized provider ID");
        let runtime = match self.runtime_mode.as_str() {
            "approval-required" | "auto-accept-edits" | "auto" => self.runtime_mode.as_str(),
            _ => "full-access",
        };
        let interaction = if self.interaction_mode == "plan" {
            "plan"
        } else {
            "default"
        };
        let updated = parse_timestamp(&self.updated_at);
        let created = parse_timestamp(&self.created_at)
            .or(updated)
            .unwrap_or(imported_at);
        let updated = updated.unwrap_or(created);
        let optional_time = |value: &Option<String>| {
            value
                .as_deref()
                .map(|raw| timestamp(parse_timestamp(raw).unwrap_or(updated)))
        };
        let created_at = timestamp(created);
        let updated_at = timestamp(updated);
        let archived_at = optional_time(&self.archived_at);
        let deleted_at = optional_time(&self.deleted_at);
        let linked_pull_request = pull_requests::linked(self.linked_pull_request.as_deref());
        let pull_requests = pull_requests::links(&self.pull_requests, linked_pull_request.as_ref());
        Ok(json!({
            "createdBy": "system",
            "creationSource": "server",
            "id": thread_id,
            "projectId": project_id,
            "title": title,
            "providerInstanceId": provider,
            "modelSelection": model,
            "runtimeMode": runtime,
            "interactionMode": interaction,
            "branch": nonempty(self.branch.as_deref()),
            "worktreePath": nonempty(self.worktree_path.as_deref()),
            "linkedPullRequest": linked_pull_request,
            "pullRequests": pull_requests,
            "branchPullRequest": pull_requests::linked(self.branch_pull_request.as_deref()),
            "activeProviderThreadId": null,
            "historyOrigin": "v1_import",
            "lineage": {"parentThreadId":null,"relationshipToParent":null,"rootThreadId":thread_id},
            "forkedFrom": null,
            "createdAt": created_at,
            "updatedAt": updated_at,
            "archivedAt": archived_at,
            "settledOverride": self.settled_override.as_deref().filter(|value| matches!(*value, "settled" | "active")),
            "settledAt": optional_time(&self.settled_at),
            "unsettledAt": optional_time(&self.unsettled_at),
            "snoozedUntil": optional_time(&self.snoozed_until),
            "snoozedAt": optional_time(&self.snoozed_at),
            "pinnedAt": optional_time(&self.pinned_at),
            "autoSettleDisabledAt": optional_time(&self.auto_settle_disabled_at),
            "pinOrderKey": nonempty(self.pin_order_key.as_deref()),
            "activeOrderKey": nonempty(self.active_order_key.as_deref()),
            "lastVisitedAt": null,
            "deletedAt": deleted_at,
        }))
    }
}

fn append_shell_event(
    transaction: &Transaction<'_>,
    event_id: &str,
    event_type: &str,
    at: &str,
    payload: &Value,
) -> rusqlite::Result<i64> {
    append_import_event(
        transaction,
        event_id,
        payload["id"].as_str().expect("validated thread ID"),
        event_type,
        at,
        &payload.to_string(),
        &json!({"providerInstanceId": payload["providerInstanceId"]}).to_string(),
    )
}

fn store_projection(transaction: &Transaction<'_>, payload: &Value) -> rusqlite::Result<()> {
    transaction.execute(
        STORE_THREAD,
        params![
            payload["id"].as_str(),
            payload["projectId"].as_str(),
            payload["title"].as_str(),
            payload["providerInstanceId"].as_str(),
            payload["runtimeMode"].as_str(),
            payload["interactionMode"].as_str(),
            payload["activeProviderThreadId"].as_str(),
            payload["createdAt"].as_str(),
            payload["updatedAt"].as_str(),
            payload["archivedAt"].as_str(),
            payload["deletedAt"].as_str(),
            payload.to_string(),
        ],
    )?;
    Ok(())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(trim_js).filter(|text| !text.is_empty())
}

fn required_id(value: &str) -> rusqlite::Result<&str> {
    nonempty(Some(value)).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            "Legacy thread or project ID is empty".into(),
        )
    })
}
