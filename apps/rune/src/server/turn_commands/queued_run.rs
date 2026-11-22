use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::{
    ThreadEventStream, TurnCommandError, TurnCommands, record_projected_sequence, timestamp,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Deserialize)]
pub(in crate::server) enum MutationKind {
    #[serde(rename = "queued-run.edit")]
    Edit,
    #[serde(rename = "queued-run.cancel")]
    Cancel,
}

impl MutationKind {
    pub(in crate::server) fn tag(self) -> &'static str {
        match self {
            Self::Edit => "queued-run.edit",
            Self::Cancel => "queued-run.cancel",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(in crate::server) struct QueuedRunMutation {
    #[serde(rename = "type")]
    pub(in crate::server) kind: MutationKind,
    pub(in crate::server) command_id: String,
    pub(in crate::server) thread_id: String,
    run_id: String,
    text: Option<String>,
    attachments: Option<Vec<Value>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::server) enum MutationOutcome {
    Accepted { sequence: i64, committed: bool },
    Rejected(String),
    PreviouslyRejected(String),
}

struct QueuedRun {
    run: Value,
    message: Option<Value>,
    attempt: Option<Value>,
    root: Option<Value>,
}

const TARGET: &str = "
    SELECT run.payload_json, message.payload_json, attempt.payload_json, node.payload_json
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_runs AS first ON first.thread_id = run.thread_id AND first.ordinal = 1
    LEFT JOIN orchestration_v2_projection_messages AS message
      ON message.thread_id = run.thread_id AND message.run_id = run.run_id
      AND message.message_id = json_extract(run.payload_json, '$.userMessageId') AND message.role = 'user'
    LEFT JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.thread_id = run.thread_id AND attempt.run_id = run.run_id
      AND attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
    LEFT JOIN orchestration_v2_projection_nodes AS node
      ON node.thread_id = run.thread_id AND node.run_id = run.run_id
      AND node.node_id = json_extract(run.payload_json, '$.rootNodeId')
    WHERE run.thread_id = ?1 AND run.run_id = ?2 AND run.status = 'queued'
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND run.ordinal > 1 AND run.provider_instance_id = 'codex'
      AND json_extract(first.payload_json, '$.workspacePreparation.type') = 'worktree'";

impl TurnCommands {
    pub(in crate::server) fn mutate_queued_run(
        &self,
        command: &QueuedRunMutation,
    ) -> Result<MutationOutcome, TurnCommandError> {
        let QueuedRunMutation {
            kind,
            command_id,
            thread_id,
            run_id,
            text,
            attachments,
        } = command;
        if [command_id, thread_id, run_id]
            .iter()
            .any(|id| id.trim().is_empty())
        {
            return Err(TurnCommandError::Unsupported(
                "A command, thread and run ID are required.",
            ));
        }
        let text = match kind {
            MutationKind::Edit if attachments.as_ref().is_none_or(Vec::is_empty) => {
                Some(text.as_deref().ok_or(TurnCommandError::Unsupported(
                    "Queued message text is required.",
                ))?)
            }
            MutationKind::Cancel if text.is_none() && attachments.is_none() => None,
            _ => {
                return Err(TurnCommandError::Unsupported(
                    "This queue supports editing ordinary text or removing a queued message.",
                ));
            }
        };
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = previous_receipt(&transaction, command)? {
            return Ok(outcome);
        }
        let now = timestamp(OffsetDateTime::now_utc());
        let result = if text.is_some_and(is_blank) {
            Err(format!(
                "Queued run {run_id} cannot be edited to an empty message."
            ))
        } else if let Some(target) = QueuedRun::read(&transaction, thread_id, run_id)? {
            let stream = ThreadEventStream::command(&transaction, thread_id, command_id);
            target.apply(&stream, run_id, text, &now)?
        } else {
            Err(format!("Run {run_id} is not queued."))
        };
        let (position, status, error, outcome) = match result {
            Ok(sequence) => {
                record_projected_sequence(&transaction, sequence, &now)?;
                (
                    sequence,
                    "accepted",
                    None,
                    MutationOutcome::Accepted {
                        sequence,
                        committed: true,
                    },
                )
            }
            Err(cause) => {
                let sequence = transaction.query_row(
                    "SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events
                     WHERE aggregate_kind = 'thread' AND stream_id = ?1 AND application_event_version = 2",
                    [thread_id], |row| row.get(0),
                )?;
                let error = format!(
                    "Failed to dispatch orchestration command {} ({command_id}).",
                    kind.tag()
                );
                (
                    sequence,
                    "rejected",
                    Some(error),
                    MutationOutcome::Rejected(cause),
                )
            }
        };
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, error, command_type
             ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, ?6, ?7)",
            params![command_id, thread_id, now, position, status, error, kind.tag()],
        )?;
        transaction.commit()?;
        Ok(outcome)
    }
}

impl QueuedRun {
    fn read(
        transaction: &Transaction<'_>,
        thread_id: &str,
        run_id: &str,
    ) -> Result<Option<Self>, TurnCommandError> {
        let Some((run, message, attempt, root)) = transaction
            .query_row(TARGET, params![thread_id, run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .optional()?
        else {
            return Ok(None);
        };
        let parse_optional =
            |value: Option<String>| value.as_deref().map(serde_json::from_str).transpose();
        Ok(Some(Self {
            run: serde_json::from_str(&run)?,
            message: parse_optional(message)?,
            attempt: parse_optional(attempt)?,
            root: parse_optional(root)?,
        }))
    }

    fn apply(
        self,
        stream: &ThreadEventStream<'_>,
        run_id: &str,
        text: Option<&str>,
        at: &str,
    ) -> Result<Result<i64, String>, TurnCommandError> {
        let Self {
            mut run,
            message,
            attempt,
            root,
        } = self;
        let mut metadata = json!({"runId":run_id,"providerInstanceId":run["providerInstanceId"]});
        if let Some(root_id) = run["rootNodeId"].as_str() {
            metadata["nodeId"] = json!(root_id);
        }
        if let Some(text) = text {
            return edit(stream, run_id, &run, message, text, at, &metadata);
        }
        if message.as_ref().is_some_and(|value| {
            !value["delegatedCompletion"].is_null() || !value["context"]["guest"].is_null()
        }) {
            return Err(TurnCommandError::Unsupported(
                "This command only removes an ordinary queued message.",
            ));
        }
        run["queuePosition"] = Value::Null;
        let mut sequence = append_cancelled(stream, "run.updated", at, &metadata, run)?;
        if let Some(root) = root {
            if let Some(attempt) = attempt {
                append_cancelled(stream, "run-attempt.updated", at, &metadata, attempt)?;
            }
            sequence = append_cancelled(stream, "node.updated", at, &metadata, root)?;
        }
        Ok(Ok(sequence))
    }
}

fn edit(
    stream: &ThreadEventStream<'_>,
    run_id: &str,
    run: &Value,
    message: Option<Value>,
    text: &str,
    at: &str,
    metadata: &Value,
) -> Result<Result<i64, String>, TurnCommandError> {
    let Some(mut message) = message else {
        return Ok(Err(format!("Queued run {run_id} has no user message.")));
    };
    for field in ["delegatedCompletion", "notification"] {
        if !message[field].is_null() {
            return Ok(Err(
                "Automatic completion deliveries cannot be edited.".to_owned()
            ));
        }
    }
    if !message["context"]["guest"].is_null() {
        return Ok(Err(
            "A guest's message cannot be edited. Cancel it instead.".to_owned(),
        ));
    }
    let reserved = text.trim();
    if message["attachments"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
        || !run["outputSchema"].is_null()
        || !run["steerWhenReadyRunId"].is_null()
        || reserved.eq_ignore_ascii_case("/logout")
        || reserved.eq_ignore_ascii_case("/compact")
        || reserved.split_whitespace().next() == Some("/goal")
    {
        return Err(TurnCommandError::Unsupported(
            "This queue only edits ordinary text messages.",
        ));
    }
    message["text"] = json!(text);
    message["updatedAt"] = json!(at);
    Ok(Ok(stream.append(
        "message.updated",
        at,
        metadata,
        &message,
    )?))
}

fn is_blank(text: &str) -> bool {
    text.chars()
        .all(|ch| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
}

fn append_cancelled(
    stream: &ThreadEventStream<'_>,
    event: &str,
    at: &str,
    metadata: &Value,
    mut payload: Value,
) -> rusqlite::Result<i64> {
    payload["status"] = json!("cancelled");
    payload["completedAt"] = json!(at);
    stream.append(event, at, metadata, &payload)
}

fn previous_receipt(
    transaction: &Transaction<'_>,
    command: &QueuedRunMutation,
) -> Result<Option<MutationOutcome>, TurnCommandError> {
    let row = transaction
        .query_row(
            "SELECT aggregate_kind, aggregate_id, command_type, status, result_sequence, error
         FROM orchestration_command_receipts WHERE command_id = ?1",
            [&command.command_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, owner, command_type, status, sequence, error)) = row else {
        return Ok(None);
    };
    if kind != "thread" || owner != command.thread_id || command_type != command.kind.tag() {
        return Err(TurnCommandError::Unsupported(
            "The command ID already has a different receipt.",
        ));
    }
    Ok(Some(if status == "rejected" {
        MutationOutcome::PreviouslyRejected(
            error.unwrap_or_else(|| "Previously rejected.".to_owned()),
        )
    } else {
        MutationOutcome::Accepted {
            sequence,
            committed: false,
        }
    }))
}
