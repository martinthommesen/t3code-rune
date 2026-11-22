use std::path::Path;

use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params, types::Type};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::super::turn_commands::{
    ThreadEventStream, allocate_turn_item_in_run, encode_id_part, record_projected_sequence,
    timestamp, validate_model_selection,
};
use super::{LaunchCommit, LaunchError, PreparedLaunch, WorkspacePreparation, retry, state};

const WORKSPACE_COMMAND: &str = "thread.metadata.update";
const RELEASE_COMMAND: &str = "prepared-run.release";
const PROGRESS_COMMAND: &str = "prepared-run.progress";
const FAILURE_COMMAND: &str = "prepared-run.fail";
const START_EFFECT: &str = "provider-turn.start";

pub(super) fn record_workspace(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    preparation: &WorkspacePreparation,
) -> Result<bool, LaunchError> {
    let operation = "update-thread";
    let fail = |error| plan.input.fail(operation, error);
    let command_id = format!("{}:workspace", plan.input.command_id.0);
    let thread_id = &commit.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if replay_exists(
        &transaction,
        plan,
        operation,
        &command_id,
        thread_id,
        WORKSPACE_COMMAND,
    )? {
        return Ok(false);
    }
    if preparation.newly_claimed_checkout
        && let Some(run_id) = &commit.run_id
        && state::preparing_strategy(&transaction, thread_id, run_id)
            .map_err(fail)?
            .is_none()
    {
        return Err(plan.input.fail(
            operation,
            "The worktree run stopped before its workspace was recorded.",
        ));
    }
    let current_workspace =
        state::project_workspace(&transaction, &plan.input.project_id.0).map_err(fail)?;
    if current_workspace.as_ref() != Some(&preparation.workspace_root) {
        return Err(plan
            .input
            .fail(operation, "Project workspace changed during preparation."));
    }
    let mut thread = transaction
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_threads
         WHERE thread_id = ?1 AND project_id = ?2
           AND archived_at IS NULL AND deleted_at IS NULL",
            params![thread_id, plan.input.project_id.0],
            |row| read_payload(row, 0),
        )
        .optional()
        .map_err(fail)?
        .ok_or_else(|| {
            plan.input
                .fail(operation, "The root workspace thread is no longer active.")
        })?;
    let now = timestamp(OffsetDateTime::now_utc());
    thread["branch"] = preparation.metadata_command["branch"].clone();
    thread["worktreePath"] = preparation.metadata_command["worktreePath"].clone();
    thread["updatedAt"] = json!(now);
    let metadata = json!({"providerInstanceId": thread["providerInstanceId"]});
    let stream = ThreadEventStream::command(&transaction, thread_id, &command_id);
    let sequence = stream
        .append("thread.metadata-updated", &now, &metadata, &thread)
        .map_err(fail)?;
    commit_command(
        transaction,
        &command_id,
        thread_id,
        WORKSPACE_COMMAND,
        sequence,
        &now,
    )
    .map_err(fail)?;
    Ok(true)
}

pub(super) enum ProgressPhase {
    Worktree,
    Setup,
}

pub(super) fn record_cancel_workspace(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    checkout: &Path,
) -> Result<bool, LaunchError> {
    let operation = "update-thread";
    let fail = |error| plan.input.fail(operation, error);
    let command_id = format!("{}:cancel-workspace", plan.input.command_id.0);
    let thread_id = &commit.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if replay_exists(
        &transaction,
        plan,
        operation,
        &command_id,
        thread_id,
        WORKSPACE_COMMAND,
    )? || replay_exists(
        &transaction,
        plan,
        operation,
        &format!("{}:release", plan.input.command_id.0),
        thread_id,
        RELEASE_COMMAND,
    )? {
        return Ok(false);
    }
    let Some(mut thread) = transaction
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_threads
         WHERE thread_id=?1 AND project_id=?2 AND deleted_at IS NULL",
            params![thread_id, plan.input.project_id.0],
            |row| read_payload(row, 0),
        )
        .optional()
        .map_err(fail)?
    else {
        return Ok(false);
    };
    if thread["worktreePath"]
        .as_str()
        .is_some_and(|path| Path::new(path) != checkout)
    {
        return Ok(false);
    }
    let now = timestamp(OffsetDateTime::now_utc());
    thread["worktreePath"] = Value::Null;
    thread["branch"] = Value::Null;
    thread["updatedAt"] = json!(now);
    let metadata = json!({"providerInstanceId": thread["providerInstanceId"]});
    let stream = ThreadEventStream::command(&transaction, thread_id, &command_id);
    let sequence = stream
        .append("thread.metadata-updated", &now, &metadata, &thread)
        .map_err(fail)?;
    commit_command(
        transaction,
        &command_id,
        thread_id,
        WORKSPACE_COMMAND,
        sequence,
        &now,
    )
    .map_err(fail)?;
    Ok(true)
}

pub(super) fn record_progress(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    phase: ProgressPhase,
) -> Result<bool, LaunchError> {
    let Some(run_id) = commit.run_id.as_deref() else {
        return Ok(false);
    };
    let operation = "update-thread";
    let fail = |error| plan.input.fail(operation, error);
    let (stage, title) = match phase {
        ProgressPhase::Worktree => ("worktree", "Preparing worktree"),
        ProgressPhase::Setup => ("setup", "Starting setup script"),
    };
    let command_id = format!("{}:progress:{stage}", plan.input.command_id.0);
    let thread_id = &commit.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if replay_exists(
        &transaction,
        plan,
        operation,
        &command_id,
        thread_id,
        PROGRESS_COMMAND,
    )? {
        return Ok(false);
    }
    let Some(mut prepared) =
        PreparingRoot::read(&transaction, plan, thread_id, run_id, true).map_err(fail)?
    else {
        return Err(plan.input.fail(
            operation,
            format!("Run {run_id} is not awaiting workspace preparation."),
        ));
    };
    let now = timestamp(OffsetDateTime::now_utc());
    prepared.item["title"] = json!(title);
    prepared.item["updatedAt"] = json!(now);
    let metadata = json!({
        "runId": run_id,
        "nodeId": prepared.node["id"],
        "providerInstanceId": prepared.run["providerInstanceId"],
    });
    let stream = ThreadEventStream::command(&transaction, thread_id, &command_id);
    let sequence = stream
        .append("turn-item.updated", &now, &metadata, &prepared.item)
        .map_err(fail)?;
    commit_command(
        transaction,
        &command_id,
        thread_id,
        PROGRESS_COMMAND,
        sequence,
        &now,
    )
    .map_err(fail)?;
    Ok(true)
}

pub(super) fn record_release(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    codex_enabled: bool,
) -> Result<bool, LaunchError> {
    let Some(run_id) = commit.run_id.as_deref() else {
        return Ok(false);
    };
    let operation = "release-run";
    let fail = |error| plan.input.fail(operation, error);
    let command_id = format!("{}:release", plan.input.command_id.0);
    let thread_id = &commit.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if replay_exists(
        &transaction,
        plan,
        operation,
        &command_id,
        thread_id,
        RELEASE_COMMAND,
    )? {
        return Ok(false);
    }
    if !codex_enabled {
        return Err(plan
            .input
            .fail(operation, plan.input.provider_disabled().message));
    }
    let workspace_id = format!("{}:workspace", plan.input.command_id.0);
    let workspace_recorded = if let Some(reused) = &plan.reused_worktree {
        state::thread(&transaction, thread_id)
            .map_err(fail)?
            .is_some_and(|thread| thread.active && reused.matches(&thread))
    } else {
        replay_exists(
            &transaction,
            plan,
            operation,
            &workspace_id,
            thread_id,
            WORKSPACE_COMMAND,
        )?
    };
    if !workspace_recorded {
        return Err(plan
            .input
            .fail(operation, "Workspace metadata has not been committed."));
    }
    let Some(mut prepared) =
        PreparingRoot::read(&transaction, plan, thread_id, run_id, true).map_err(fail)?
    else {
        return Err(plan.input.fail(
            operation,
            format!("Run {run_id} is not awaiting workspace preparation."),
        ));
    };
    validate_model_selection(&prepared.run["modelSelection"])
        .map_err(|cause| plan.input.fail(operation, cause))?;
    let now = timestamp(OffsetDateTime::now_utc());
    let scope = prepared.scope(thread_id, run_id, &now).map_err(fail)?;
    prepared.node["checkpointScopeId"] = scope["id"].clone();
    prepared.item["status"] = json!("completed");
    prepared.item["title"] = json!("Workspace ready");
    prepared.item["output"] = json!("Workspace preparation completed.");
    prepared.item["exitCode"] = json!(0);
    prepared.item["completedAt"] = json!(now);
    prepared.item["updatedAt"] = json!(now);
    prepared.run["status"] = json!("starting");
    let metadata = json!({
        "runId": run_id,
        "nodeId": prepared.node["id"],
        "providerInstanceId": prepared.run["providerInstanceId"],
    });
    let stream = ThreadEventStream::command(&transaction, thread_id, &command_id);
    stream
        .append("checkpoint-scope.created", &now, &metadata, &scope)
        .map_err(fail)?;
    stream
        .append("node.updated", &now, &metadata, &prepared.node)
        .map_err(fail)?;
    stream
        .append("turn-item.updated", &now, &metadata, &prepared.item)
        .map_err(fail)?;
    let sequence = stream
        .append("run.updated", &now, &metadata, &prepared.run)
        .map_err(fail)?;
    let queued = timestamp(OffsetDateTime::now_utc());
    transaction
        .execute(
            "INSERT INTO orchestration_v2_effect_outbox (
            effect_id, command_id, thread_id, effect_type, payload_json, status,
            attempt_count, available_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
            params![
                format!("effect:{command_id}:{START_EFFECT}:{run_id}"),
                command_id,
                thread_id,
                START_EFFECT,
                json!({"type":START_EFFECT, "runId":run_id}).to_string(),
                queued,
            ],
        )
        .map_err(fail)?;
    commit_command(
        transaction,
        &command_id,
        thread_id,
        RELEASE_COMMAND,
        sequence,
        &now,
    )
    .map_err(fail)?;
    Ok(true)
}

// A failed preparation must settle the preparing run before queued work can continue.
pub(super) fn record_failure(
    plan: &PreparedLaunch,
    commit: &LaunchCommit,
    message: &str,
) -> rusqlite::Result<bool> {
    let Some(run_id) = commit.run_id.as_deref() else {
        return Ok(false);
    };
    let command_id = format!("{}:fail", plan.input.command_id.0);
    let thread_id = &commit.thread_id;
    let mut database = state::open_writer(&plan.database)?;
    let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(receipt) = state::receipt(&transaction, &command_id)? {
        if receipt.accepted
            && receipt.thread_id == *thread_id
            && receipt.command_type == FAILURE_COMMAND
        {
            return Ok(false);
        }
        return Err(rusqlite::Error::InvalidQuery);
    }
    let Some(mut prepared) = PreparingRoot::read(&transaction, plan, thread_id, run_id, false)?
    else {
        return Ok(false);
    };
    let now = timestamp(OffsetDateTime::now_utc());
    prepared.attempt["status"] = json!("failed");
    prepared.attempt["completedAt"] = json!(now);
    prepared.node["status"] = json!("failed");
    prepared.node["completedAt"] = json!(now);
    prepared.item["status"] = json!("failed");
    prepared.item["title"] = json!("Workspace preparation failed");
    prepared.item["output"] = json!(message);
    prepared.item["exitCode"] = json!(1);
    prepared.item["completedAt"] = json!(now);
    prepared.item["updatedAt"] = json!(now);
    prepared.run["status"] = json!("failed");
    prepared.run["completedAt"] = json!(now);
    let failure_item_id = format!(
        "turn-item:provider:codex:native-item:{}",
        encode_id_part(&format!("workspace-preparation-failure:{run_id}"))
    );
    let ordinal = allocate_turn_item_in_run(&transaction, thread_id, &failure_item_id, 1)?;
    let failure_item = json!({
        "id": failure_item_id,
        "threadId": thread_id,
        "runId": run_id,
        "nodeId": prepared.node["id"],
        "providerThreadId": prepared.run["providerThreadId"],
        "providerTurnId": null,
        "nativeItemRef": null,
        "parentItemId": null,
        "ordinal": ordinal,
        "status": "failed",
        "title": "Workspace preparation failed",
        "startedAt": now,
        "completedAt": now,
        "updatedAt": now,
        "type": "error",
        "failure": {
            "class": "validation_error",
            "message": message,
            "code": "workspace_preparation_failed",
            "retryable": false,
        },
    });
    let metadata = json!({
        "runId": run_id,
        "nodeId": prepared.node["id"],
        "providerInstanceId": prepared.run["providerInstanceId"],
    });
    let stream = ThreadEventStream::command(&transaction, thread_id, &command_id);
    stream.append("run-attempt.updated", &now, &metadata, &prepared.attempt)?;
    stream.append("node.updated", &now, &metadata, &prepared.node)?;
    stream.append("turn-item.updated", &now, &metadata, &prepared.item)?;
    stream.append("turn-item.updated", &now, &metadata, &failure_item)?;
    let sequence = stream.append("run.updated", &now, &metadata, &prepared.run)?;
    let should_hold = prepared.run["workspacePreparation"]["type"] == "worktree"
        && state::thread(&transaction, thread_id)?
            .is_some_and(|thread| thread.worktree_path.is_none())
        && retry::latest_executed_run(&transaction, thread_id)?.as_deref() == Some(run_id);
    let projected_sequence = if should_hold {
        let system = ThreadEventStream::server(&transaction, thread_id);
        retry::change_queued_holds(&transaction, &system, thread_id, &now, true)?
            .unwrap_or(sequence)
    } else {
        sequence
    };
    record_projected_sequence(&transaction, projected_sequence, &now)?;
    state::record_receipt(
        &transaction,
        &command_id,
        thread_id,
        FAILURE_COMMAND,
        u64::try_from(sequence).map_err(|_| rusqlite::Error::InvalidQuery)?,
        &now,
        None,
    )?;
    transaction.commit()?;
    Ok(true)
}

pub(super) fn transient_write_error(error: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;
    error.sqlite_error_code().is_some_and(|code| {
        matches!(
            code,
            ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
                | ErrorCode::DiskFull
                | ErrorCode::SystemIoFailure
                | ErrorCode::CannotOpen
        )
    })
}

struct PreparingRoot {
    run: Value,
    attempt: Value,
    node: Value,
    item: Value,
    cwd: Option<String>,
}

impl PreparingRoot {
    fn read(
        transaction: &Transaction<'_>,
        plan: &PreparedLaunch,
        thread_id: &str,
        run_id: &str,
        require_active_project: bool,
    ) -> rusqlite::Result<Option<Self>> {
        transaction
            .query_row(
                PREPARING_ROOT_RUN,
                params![
                    thread_id,
                    run_id,
                    plan.input.project_id.0,
                    require_active_project
                ],
                |row| {
                    Ok(Self {
                        run: read_payload(row, 0)?,
                        attempt: read_payload(row, 1)?,
                        node: read_payload(row, 2)?,
                        item: read_payload(row, 3)?,
                        cwd: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    fn scope(&self, thread_id: &str, run_id: &str, now: &str) -> rusqlite::Result<Value> {
        let cwd = self.cwd.as_deref().ok_or(rusqlite::Error::InvalidQuery)?;
        Ok(json!({
            "id": format!("checkpoint-scope:thread:{}:name:root", encode_id_part(thread_id)),
            "threadId": thread_id,
            "runId": run_id,
            "nodeId": self.node["id"],
            "parentScopeId": null,
            "providerThreadId": self.run["providerThreadId"],
            "kind": "root_run",
            "ordinalWithinParent": 0,
            "advancesAppRunCount": true,
            "cwd": cwd,
            "createdAt": now,
        }))
    }
}

const PREPARING_ROOT_RUN: &str = "
    SELECT run.payload_json, attempt.payload_json, node.payload_json, item.payload_json,
           COALESCE(json_extract(thread.payload_json, '$.worktreePath'), project.workspace_root)
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    LEFT JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_nodes AS node
      ON node.node_id = json_extract(run.payload_json, '$.rootNodeId')
         AND node.run_id = run.run_id AND node.node_id = attempt.root_node_id
    JOIN orchestration_v2_projection_provider_threads AS provider
      ON provider.provider_thread_id = run.provider_thread_id AND provider.thread_id = run.thread_id
    JOIN orchestration_v2_projection_turn_items AS item
      ON item.run_id = run.run_id AND item.thread_id = run.thread_id
    JOIN orchestration_v2_projection_messages AS message
      ON message.message_id = json_extract(run.payload_json, '$.userMessageId')
         AND message.run_id = run.run_id AND message.thread_id = run.thread_id
    WHERE run.thread_id = ?1 AND run.run_id = ?2 AND thread.project_id = ?3
      AND (?4 = 0 OR (thread.archived_at IS NULL AND thread.deleted_at IS NULL
                      AND project.project_id IS NOT NULL AND project.deleted_at IS NULL))
      AND run.status = 'preparing' AND run.ordinal = 1 AND run.provider_instance_id = 'codex'
      AND json_extract(run.payload_json, '$.workspacePreparation.type') IN ('root', 'existing_worktree', 'worktree')
      AND node.kind = 'root_turn' AND node.status = 'pending'
      AND attempt.status = 'pending' AND attempt.attempt_ordinal = 1
      AND provider.status = 'not_loaded' AND provider.driver = 'codex'
      AND item.type = 'command_execution' AND item.status = 'running'
      AND json_extract(item.payload_json, '$.input') = 'Preparing workspace'
      AND message.role = 'user'
    LIMIT 1";

fn read_payload(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let payload: String = row.get(index)?;
    serde_json::from_str(&payload).map_err(|cause| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(cause))
    })
}

fn replay_exists(
    transaction: &Transaction<'_>,
    plan: &PreparedLaunch,
    operation: &'static str,
    command_id: &str,
    thread_id: &str,
    command_type: &str,
) -> Result<bool, LaunchError> {
    let receipt = state::receipt(transaction, command_id)
        .map_err(|cause| plan.input.fail("read-receipt", cause))?;
    match receipt {
        None => Ok(false),
        Some(receipt)
            if receipt.accepted
                && receipt.thread_id == thread_id
                && receipt.command_type == command_type =>
        {
            Ok(true)
        }
        Some(_) => Err(plan.input.fail(
            operation,
            format!("Command {command_id} already has a conflicting receipt."),
        )),
    }
}

fn commit_command(
    transaction: Transaction<'_>,
    command_id: &str,
    thread_id: &str,
    command_type: &str,
    sequence: i64,
    now: &str,
) -> rusqlite::Result<()> {
    record_projected_sequence(&transaction, sequence, now)?;
    state::record_receipt(
        &transaction,
        command_id,
        thread_id,
        command_type,
        u64::try_from(sequence).map_err(|_| rusqlite::Error::InvalidQuery)?,
        now,
        None,
    )?;
    transaction.commit()
}
