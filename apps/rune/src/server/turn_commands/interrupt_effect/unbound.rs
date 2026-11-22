use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

use super::super::queued_worktree::hold_queue;
use super::super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, allocate_turn_item_in_run, encode_id_part,
};

const SELECT_UNBOUND_RUN: &str = include_str!("unbound.sql");

pub(super) fn can_interrupt(connection: &Connection, thread_id: &str) -> rusqlite::Result<bool> {
    Ok(connection
        .query_row(SELECT_UNBOUND_RUN, [thread_id], |_| Ok(()))
        .optional()?
        .is_some())
}

pub(super) fn interrupt(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    reason: Option<&str>,
    now: &str,
) -> Result<Option<i64>, TurnCommandError> {
    let run = transaction
        .query_row(SELECT_UNBOUND_RUN, [thread_id], |row| {
            Ok(UnboundRun {
                id: row.get(0)?,
                ordinal: row.get(1)?,
                node_id: row.get(2)?,
                provider_thread_id: row.get(3)?,
                run: row.get(4)?,
                attempt: row.get(5)?,
                node: row.get(6)?,
                preparation: row.get(7)?,
            })
        })
        .optional()?;
    let Some(run) = run else { return Ok(None) };

    let stream = ThreadEventStream::command(transaction, thread_id, command_id);
    let metadata =
        json!({ "runId": run.id, "nodeId": run.node_id, "providerInstanceId": CODEX_INSTANCE });
    let signal_prefix = format!("turn-item:run:{}:signal:", encode_id_part(&run.id));
    let request_id = format!("{signal_prefix}interrupt-request");
    let result_id = format!("{signal_prefix}interrupt-result");
    let request_ordinal =
        allocate_turn_item_in_run(transaction, thread_id, &request_id, run.ordinal)?;
    let result_ordinal =
        allocate_turn_item_in_run(transaction, thread_id, &result_id, run.ordinal)?;
    let request = json!({
        "id": request_id, "threadId": thread_id, "runId": run.id, "nodeId": run.node_id,
        "providerThreadId": run.provider_thread_id, "providerTurnId": null,
        "nativeItemRef": null, "parentItemId": null, "ordinal": request_ordinal,
        "status": "completed", "title": "Interrupt requested", "startedAt": now,
        "completedAt": now, "updatedAt": now, "type": "run_interrupt_request",
        "message": reason.unwrap_or("Stopped by user"),
    });
    stream.append("turn-item.updated", now, &metadata, &request)?;
    stream.append(
        "turn-item.updated",
        now,
        &metadata,
        &json!({
            "id": result_id, "threadId": thread_id, "runId": run.id, "nodeId": run.node_id,
            "providerThreadId": run.provider_thread_id, "providerTurnId": null,
            "nativeItemRef": null, "parentItemId": request_id, "ordinal": result_ordinal,
            "status": "interrupted", "title": "Interrupted", "startedAt": now,
            "completedAt": now, "updatedAt": now, "type": "run_interrupt_result",
            "message": "Run interrupted before provider start",
        }),
    )?;
    if let Some(preparation) = run.preparation {
        let mut item = super::settlement::interrupted(&preparation, now)?;
        item["title"] = json!("Workspace preparation interrupted");
        item["output"] = json!(reason.unwrap_or("Interrupted before provider start"));
        item["updatedAt"] = json!(now);
        stream.append("turn-item.updated", now, &metadata, &item)?;
    }
    for (event_type, payload) in [
        ("run-attempt.updated", run.attempt),
        ("node.updated", run.node),
    ] {
        stream.append(
            event_type,
            now,
            &metadata,
            &super::settlement::interrupted(&payload, now)?,
        )?;
    }
    let interrupted = super::settlement::interrupted(&run.run, now)?;
    let mut sequence = stream.append("run.updated", now, &metadata, &interrupted)?;
    if interrupted["workspacePreparation"]["type"] == "worktree" {
        sequence = hold_queue(&stream, now)?.unwrap_or(sequence);
    }
    transaction.execute(
        "UPDATE orchestration_v2_effect_outbox SET status = 'cancelled', lease_owner = NULL,
             lease_expires_at = NULL, completed_at = ?3, updated_at = ?3, last_error = ?4
         WHERE thread_id = ?1 AND json_extract(payload_json, '$.runId') = ?2
           AND effect_type IN ('provider-turn.start', 'provider-turn.restart')
           AND status IN ('pending', 'running')",
        params![
            thread_id,
            run.id,
            now,
            format!(
                "Run {} was interrupted before its provider turn started.",
                run.id
            )
        ],
    )?;
    Ok(Some(sequence))
}

struct UnboundRun {
    id: String,
    ordinal: i64,
    node_id: String,
    provider_thread_id: String,
    run: String,
    attempt: String,
    node: String,
    preparation: Option<String>,
}
