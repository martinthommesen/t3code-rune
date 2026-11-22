use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::super::thread_commands::{CreateThread, append_thread_creation};
use super::super::turn_commands::{
    NativeProvider, PreparingDispatch, ThreadEventStream, append_preparing_dispatch,
    claude_slash_command, encode_id_part, record_projected_sequence, timestamp,
    validate_model_selection,
};
use super::{DispatchCommit, InitialDispatch, LaunchError, PreparedLaunch, input, state};

const TITLE_EFFECT: &str = "thread-title.generate";
const CREATE_THREAD: &str = "thread.create";
const UPDATE_THREAD: &str = "thread.metadata.update";
const DISPATCH_MESSAGE: &str = "message.dispatch";

pub(super) fn reuse_claim(plan: &PreparedLaunch) -> Result<DispatchCommit, LaunchError> {
    let operation = "update-thread";
    let fail = |cause| plan.input.fail(operation, cause);
    let command_id = &plan.input.command_id.0;
    let thread_id = &plan.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if let Some(receipt) = ExistingClaim::read(&transaction, command_id).map_err(fail)? {
        return receipt.replay(&transaction, plan, command_id, thread_id, operation);
    }
    let mut projection = empty_draft(&transaction, plan, thread_id, operation)?;
    let now = timestamp(OffsetDateTime::now_utc());
    projection["updatedAt"] = json!(now);
    let metadata = json!({"providerInstanceId": projection["providerInstanceId"]});
    let sequence = ThreadEventStream::command(&transaction, thread_id, command_id)
        .append("thread.metadata-updated", &now, &metadata, &projection)
        .map_err(fail)?;
    record_projected_sequence(&transaction, sequence, &now).map_err(fail)?;
    let sequence = u64::try_from(sequence).map_err(|cause| plan.input.fail(operation, cause))?;
    state::record_receipt(
        &transaction,
        command_id,
        thread_id,
        UPDATE_THREAD,
        sequence,
        &now,
        None,
    )
    .map_err(fail)?;
    transaction.commit().map_err(fail)?;
    Ok(DispatchCommit {
        sequence,
        replayed: false,
        thread_id: Some(thread_id.clone()),
        run_id: None,
    })
}

pub(super) fn fallback_message(
    plan: &PreparedLaunch,
    dispatch: &InitialDispatch,
    provider_enabled: bool,
) -> Result<DispatchCommit, LaunchError> {
    let operation = "dispatch-message";
    let fail = |cause| plan.input.fail(operation, cause);
    let command_id = dispatch.command["commandId"].as_str().ok_or_else(|| {
        plan.input
            .fail(operation, "The initial message command is missing its id.")
    })?;
    let thread_id = &dispatch.thread_id;
    let mut database = state::open_writer(&plan.database).map_err(fail)?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    if let Some(receipt) = ExistingClaim::read(&transaction, command_id).map_err(fail)? {
        return receipt.replay(&transaction, plan, command_id, thread_id, operation);
    }
    if !provider_enabled {
        return Err(plan.input.provider_disabled());
    }
    let projection = empty_draft(&transaction, plan, thread_id, operation)?;
    let message = plan.input.initial_message.as_ref().ok_or_else(|| {
        plan.input
            .fail(operation, "This launch has no initial message.")
    })?;
    let message_id = plan.initial_message_id.as_deref().ok_or_else(|| {
        plan.input
            .fail(operation, "This launch has no initial message id.")
    })?;
    validate_message(plan, operation, &message.text)?;
    ensure_unused_message(&transaction, plan, operation, message_id)?;
    let result = persist_initial_message(
        &transaction,
        plan,
        projection,
        InitialWrite {
            command_id,
            command_type: DISPATCH_MESSAGE,
            thread_id,
            message_id,
            text: &message.text,
        },
    )
    .map_err(fail)?;
    transaction.commit().map_err(fail)?;
    Ok(result)
}

fn empty_draft(
    transaction: &Transaction<'_>,
    plan: &PreparedLaunch,
    thread_id: &str,
    operation: &'static str,
) -> Result<Value, LaunchError> {
    let found =
        state::thread(transaction, thread_id).map_err(|cause| plan.input.fail(operation, cause))?;
    if !found.is_some_and(|thread| {
        thread.active && thread.empty && thread.project_id == plan.input.project_id.0
    }) {
        return Err(plan.input.fail(
            operation,
            "Only an empty active thread in the target project can launch.",
        ));
    }
    let payload: String = transaction
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .map_err(|cause| plan.input.fail(operation, cause))?;
    let projection: Value =
        serde_json::from_str(&payload).map_err(|cause| plan.input.fail(operation, cause))?;
    if projection["providerInstanceId"] != "codex"
        || !projection["lineage"]["parentThreadId"].is_null()
        || !projection["activeProviderThreadId"].is_null()
        || !projection["forkedFrom"].is_null()
        || projection["historyOrigin"] == "v1_import"
        || !projection["settledOverride"].is_null()
        || !projection["snoozedUntil"].is_null()
    {
        return Err(plan.input.fail(
            operation,
            "This draft is not a new root workspace Codex thread.",
        ));
    }
    Ok(projection)
}

pub(super) fn initial_claim(
    plan: &PreparedLaunch,
    provider_enabled: bool,
) -> Result<DispatchCommit, LaunchError> {
    let Some(message) = plan.input.initial_message.as_ref() else {
        return Err(plan
            .input
            .fail("create-thread", "This launch has no initial message."));
    };
    let Some(message_id) = plan.initial_message_id.as_deref() else {
        return Err(plan
            .input
            .fail("create-thread", "This launch has no initial message id."));
    };
    if plan.input.reuse_existing_thread == Some(true) {
        return Err(plan.input.fail(
            "update-thread",
            "This launch needs a reusable-thread claim.",
        ));
    }
    let mut database = state::open_writer(&plan.database)
        .map_err(|cause| plan.input.fail("create-thread", cause))?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|cause| plan.input.fail("create-thread", cause))?;
    let existing = ExistingClaim::read(&transaction, &plan.input.command_id.0)
        .map_err(|cause| plan.input.fail("read-receipt", cause))?;
    if let Some(existing) = existing {
        return existing.replay(
            &transaction,
            plan,
            &plan.input.command_id.0,
            &plan.thread_id,
            "create-thread",
        );
    }
    if !provider_enabled {
        return Err(plan.input.provider_disabled());
    }
    let current_workspace = state::project_workspace(&transaction, &plan.input.project_id.0)
        .map_err(|cause| plan.input.fail("resolve-project", cause))?;
    if current_workspace.as_ref() != Some(&plan.workspace_root) {
        return Err(plan.input.fail(
            "resolve-project",
            "Project workspace changed during launch.",
        ));
    }
    validate_message(plan, "create-thread", &message.text)?;
    let command_id = &plan.input.command_id.0;
    let thread_id = &plan.thread_id;
    if state::thread(&transaction, thread_id)
        .map_err(|cause| plan.input.fail("create-thread", cause))?
        .is_some()
    {
        let message = format!("Thread {thread_id} already exists. Use a new thread id.");
        let sequence: u64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events
             WHERE aggregate_kind = 'thread' AND application_event_version = 2 AND stream_id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .map_err(|cause| plan.input.fail("create-thread", cause))?;
        state::record_receipt(
            &transaction,
            command_id,
            thread_id,
            CREATE_THREAD,
            sequence,
            &timestamp(OffsetDateTime::now_utc()),
            Some(&message),
        )
        .map_err(|cause| plan.input.fail("create-thread", cause))?;
        transaction
            .commit()
            .map_err(|cause| plan.input.fail("create-thread", cause))?;
        return Err(plan.input.fail_with_defect(
            "create-thread",
            json!({
                "name":"OrchestratorThreadAlreadyExistsError",
                "message":message
            }),
        ));
    }
    ensure_unused_message(&transaction, plan, "create-thread", message_id)?;

    let mut payload = plan.claim_command();
    let _ = payload
        .as_object_mut()
        .expect("creation command is an object")
        .remove("initialMessage");
    let create = CreateThread::parse_for_launch(payload)
        .map_err(|cause| plan.input.fail("create-thread", cause))?;
    let created_at = timestamp(OffsetDateTime::now_utc());
    let (_, projection) = append_thread_creation(&transaction, &create, &created_at)
        .map_err(|cause| plan.input.fail("create-thread", cause))?;
    let run = persist_initial_message(
        &transaction,
        plan,
        projection,
        InitialWrite {
            command_id,
            command_type: CREATE_THREAD,
            thread_id,
            message_id,
            text: &message.text,
        },
    )
    .map_err(|cause| plan.input.fail("create-thread", cause))?;
    transaction
        .commit()
        .map_err(|cause| plan.input.fail("create-thread", cause))?;
    Ok(run)
}

fn ensure_unused_message(
    transaction: &Transaction<'_>,
    plan: &PreparedLaunch,
    operation: &'static str,
    message_id: &str,
) -> Result<(), LaunchError> {
    let user_item = format!("turn-item:message:{}", encode_id_part(message_id));
    let already_stored: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_messages WHERE message_id = ?1)
           OR EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items WHERE turn_item_id = ?2)",
        [message_id, &user_item],
        |row| row.get(0),
    ).map_err(|cause| plan.input.fail(operation, cause))?;
    if already_stored {
        return Err(plan.input.fail(
            operation,
            format!("Message {message_id} already exists. Use a new message id."),
        ));
    }
    Ok(())
}

fn validate_message(
    plan: &PreparedLaunch,
    operation: &'static str,
    text: &str,
) -> Result<(), LaunchError> {
    let text = input::trim_js(text);
    if !matches!(
        plan.input.creation_source.unwrap_or_default(),
        input::CreationSource::Web
    ) || text.is_empty()
        || text.eq_ignore_ascii_case("/compact")
        || text.eq_ignore_ascii_case("/logout")
        || text.split_whitespace().next() == Some("/goal")
    {
        return Err(plan
            .input
            .fail(operation, "This launch needs a regular web text prompt."));
    }
    match plan.provider {
        NativeProvider::Codex => {
            validate_model_selection(&json!(plan.input.model_selection))
                .map_err(|cause| plan.input.fail(operation, cause))?;
        }
        NativeProvider::Claude if claude_slash_command(text) => {
            return Err(plan.input.fail(
                operation,
                "A Claude Code launch requires a plain text prompt.",
            ));
        }
        NativeProvider::Claude => {}
    }
    Ok(())
}

struct InitialWrite<'a> {
    command_id: &'a str,
    command_type: &'static str,
    thread_id: &'a str,
    message_id: &'a str,
    text: &'a str,
}

fn persist_initial_message(
    transaction: &Transaction<'_>,
    plan: &PreparedLaunch,
    mut projection: Value,
    initial: InitialWrite<'_>,
) -> rusqlite::Result<DispatchCommit> {
    let InitialWrite {
        command_id,
        command_type,
        thread_id,
        message_id,
        text,
    } = initial;
    let commands = ThreadEventStream::command(transaction, thread_id, command_id);
    let metadata = json!({"providerInstanceId": plan.provider.as_str()});
    if projection["lastVisitedAt"].is_null() {
        let visit = OffsetDateTime::now_utc();
        projection["lastVisitedAt"] = json!(timestamp(visit - time::Duration::milliseconds(1)));
        commands.append(
            "thread.metadata-updated",
            &timestamp(visit),
            &metadata,
            &projection,
        )?;
    }
    let generate_title = plan.input.generate_title == Some(true)
        && !has_title_before_first_message(&projection, &plan.input.title.0);
    if generate_title {
        let title_started = timestamp(OffsetDateTime::now_utc());
        projection["title"] = json!(plan.input.title);
        projection["titleRegeneration"] = json!({
            "requestId": command_id,
            "startedAt": title_started
        });
        projection["updatedAt"] = json!(title_started);
        commands.append(
            "thread.metadata-updated",
            &title_started,
            &metadata,
            &projection,
        )?;
    }
    let now = timestamp(OffsetDateTime::now_utc());
    let strategy = plan.workspace_strategy();
    let (sequence, run_id) = append_preparing_dispatch(
        transaction,
        PreparingDispatch {
            command_id,
            thread_id,
            message_id,
            text,
            model_selection: json!(plan.input.model_selection),
            provider: plan.provider,
            workspace_strategy: &strategy,
            now: &now,
        },
    )?;
    if generate_title {
        let queued = timestamp(OffsetDateTime::now_utc());
        transaction.execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status,
                attempt_count, available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
            params![
                format!("effect:{command_id}:{TITLE_EFFECT}"),
                command_id,
                thread_id,
                TITLE_EFFECT,
                json!({
                    "type": TITLE_EFFECT,
                    "kind": {"type":"initial","messageId": message_id}
                })
                .to_string(),
                queued,
            ],
        )?;
    }
    let sequence = u64::try_from(sequence).map_err(|_| rusqlite::Error::InvalidQuery)?;
    state::record_receipt(
        transaction,
        command_id,
        thread_id,
        command_type,
        sequence,
        &now,
        None,
    )?;
    Ok(DispatchCommit {
        sequence,
        replayed: false,
        thread_id: Some(thread_id.to_owned()),
        run_id: Some(run_id),
    })
}

fn has_title_before_first_message(thread: &Value, seed: &str) -> bool {
    let title = thread["title"]
        .as_str()
        .map(input::trim_js)
        .unwrap_or_default();
    let placeholder = title.rsplit_once(" thread ").is_some_and(|(_, number)| {
        !number.is_empty() && number.bytes().all(|digit| digit.is_ascii_digit())
    });
    thread["titleRenamed"] == true
        || (!title.is_empty() && title != "New thread" && title != seed && !placeholder)
}

struct ExistingClaim {
    aggregate_kind: String,
    aggregate_id: String,
    sequence: u64,
    rejected: bool,
    error: Option<String>,
}

impl ExistingClaim {
    fn read(transaction: &Transaction<'_>, command_id: &str) -> rusqlite::Result<Option<Self>> {
        transaction
            .query_row(
                "SELECT aggregate_kind, aggregate_id, result_sequence, status, error
             FROM orchestration_command_receipts WHERE command_id = ?1",
                [command_id],
                |row| {
                    Ok(Self {
                        aggregate_kind: row.get(0)?,
                        aggregate_id: row.get(1)?,
                        sequence: row.get(2)?,
                        rejected: row.get::<_, String>(3)? == "rejected",
                        error: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    fn replay(
        self,
        transaction: &Transaction<'_>,
        plan: &PreparedLaunch,
        command_id: &str,
        thread_id: &str,
        operation: &'static str,
    ) -> Result<DispatchCommit, LaunchError> {
        if self.aggregate_kind != "thread" {
            return Err(plan.input.fail(
                operation,
                format!("Command {command_id} was already used by a project command."),
            ));
        }
        if self.rejected {
            let detail = self.error.as_deref().unwrap_or("Previously rejected.");
            return Err(plan.input.fail_with_defect(
                operation,
                json!({
                    "name": "OrchestratorCommandPreviouslyRejectedError",
                    "message": format!("Command {command_id} was previously rejected: {detail}")
                }),
            ));
        }
        if self.aggregate_id != thread_id {
            return Err(plan.input.fail_with_defect(operation, json!({
                "name": "OrchestratorCommandIdConflictError",
                "message": format!(
                    "Command {command_id} was already handled for thread {} and cannot be replayed for {}.",
                    self.aggregate_id, thread_id
                )
            })));
        }
        let run_id = transaction
            .query_row(
                "SELECT json_extract(payload_json, '$.id') FROM orchestration_events
             WHERE aggregate_kind = 'thread' AND stream_id = ?1 AND command_id = ?2
               AND application_event_version = 2 AND event_type = 'run.created'
               AND sequence <= ?3 ORDER BY sequence LIMIT 1",
                params![thread_id, command_id, self.sequence],
                |row| row.get(0),
            )
            .optional()
            .map_err(|cause| plan.input.fail("read-receipt", cause))?;
        Ok(DispatchCommit {
            sequence: self.sequence,
            replayed: true,
            thread_id: Some(self.aggregate_id),
            run_id,
        })
    }
}
