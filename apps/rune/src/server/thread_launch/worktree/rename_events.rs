use std::path::Path;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::server::turn_commands::{
    ThreadEventStream, allocate_turn_item_in_run, encode_id_part, record_projected_sequence,
    timestamp,
};

use super::super::{LaunchCommit, LaunchError, PreparedLaunch, state};

const COMMAND: &str = "thread.metadata.update";

pub(in crate::server::thread_launch) fn settled(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
) -> Result<bool, LaunchError> {
    let fail = |error| plan.input.fail("update-thread", error);
    let database = state::open(&plan.database).map_err(fail)?;
    let command_id = format!("{}:branch-rename", plan.input.command_id.0);
    if state::receipt(&database, &command_id)
        .map_err(fail)?
        .is_some_and(|receipt| {
            receipt.accepted
                && receipt.thread_id == commit.thread_id
                && receipt.command_type == COMMAND
        })
    {
        return Ok(true);
    }
    let Some(run_id) = &commit.run_id else {
        return Ok(false);
    };
    let item_id = format!(
        "turn-item:run:{}:signal:branch-rename",
        encode_id_part(run_id)
    );
    database.query_row("SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_turn_items WHERE thread_id=?1 AND turn_item_id=?2)",rusqlite::params![commit.thread_id,item_id],|row|row.get(0)).map_err(fail)
}

pub(in crate::server::thread_launch) fn renamed(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    checkout: &Path,
    old: &str,
    new: &str,
) -> Result<bool, LaunchError> {
    let fail = |error| plan.input.fail("update-thread", error);
    let command_id = format!("{}:branch-rename", plan.input.command_id.0);
    let thread_id = &commit.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if let Some(receipt) = state::receipt(&transaction, &command_id).map_err(fail)? {
        if receipt.thread_id == *thread_id && receipt.accepted && receipt.command_type == COMMAND {
            return Ok(false);
        }
        return Err(plan.input.fail(
            "update-thread",
            "The branch rename has a conflicting command receipt.",
        ));
    }
    let row: Option<String> = transaction
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_threads
         WHERE thread_id=?1 AND project_id=?2 AND deleted_at IS NULL
           AND json_extract(payload_json, '$.worktreePath')=?3
           AND json_extract(payload_json, '$.branch')=?4",
            params![thread_id, plan.input.project_id.0, checkout.to_str(), old],
            |row| row.get(0),
        )
        .optional()
        .map_err(fail)?;
    let Some(row) = row else {
        return Ok(false);
    };
    let mut thread: Value =
        serde_json::from_str(&row).map_err(|error| plan.input.fail("update-thread", error))?;
    let now = timestamp(OffsetDateTime::now_utc());
    thread["branch"] = json!(new);
    thread["updatedAt"] = json!(now);
    let sequence = ThreadEventStream::command(&transaction, thread_id, &command_id)
        .append(
            "thread.metadata-updated",
            &now,
            &json!({"providerInstanceId":thread["providerInstanceId"]}),
            &thread,
        )
        .map_err(fail)?;
    record_projected_sequence(&transaction, sequence, &now).map_err(fail)?;
    state::record_receipt(
        &transaction,
        &command_id,
        thread_id,
        COMMAND,
        sequence as u64,
        &now,
        None,
    )
    .map_err(fail)?;
    transaction.commit().map_err(fail)?;
    Ok(true)
}

pub(in crate::server::thread_launch) fn failure(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    branch: &str,
    message: &str,
) -> Result<bool, LaunchError> {
    let Some(run_id) = &commit.run_id else {
        return Ok(false);
    };
    let fail = |error| plan.input.fail("update-thread", error);
    let thread_id = &commit.thread_id;
    let item_id = format!(
        "turn-item:run:{}:signal:branch-rename",
        encode_id_part(run_id)
    );
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_turn_items WHERE turn_item_id=?1)",
        [&item_id], |row| row.get::<_,bool>(0),
    ).map_err(fail)? {
        return Ok(false);
    }
    let now = timestamp(OffsetDateTime::now_utc());
    let ordinal = allocate_turn_item_in_run(&transaction, thread_id, &item_id, 1).map_err(fail)?;
    let notice = json!({
        "id":item_id, "threadId":thread_id, "runId":run_id, "nodeId":null,
        "providerThreadId":null, "providerTurnId":null, "nativeItemRef":null,"parentItemId":null,
        "ordinal":ordinal,"status":"completed","title":"Branch not renamed","type":"system_notice",
        "startedAt":now,"completedAt":now,"updatedAt":now,
        "message":format!("Could not rename the worktree branch, so it stays {branch}. {message}"),
    });
    let sequence = ThreadEventStream::server(&transaction, thread_id)
        .append("turn-item.updated", &now, &json!({"runId":run_id}), &notice)
        .map_err(fail)?;
    record_projected_sequence(&transaction, sequence, &now).map_err(fail)?;
    transaction.commit().map_err(fail)?;
    Ok(true)
}
