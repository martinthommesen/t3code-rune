use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use super::queued_worktree::{ROOT_SCOPE_MATCH, START_ORIGIN};
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, record_projected_sequence,
    timestamp,
};

pub(in crate::server) enum RecoveryTrigger<'a> {
    Startup,
    Shutdown(&'a str),
}

impl RecoveryTrigger<'_> {
    fn labels(&self) -> (&'static str, &'static str) {
        match self {
            Self::Startup => (
                "startup",
                "Cancelled because the server restarted before the provider work completed.",
            ),
            Self::Shutdown(_) => (
                "shutdown",
                "Cancelled because the server shut down before the provider work completed.",
            ),
        }
    }

    fn lease(&self) -> Option<&str> {
        match self {
            Self::Startup => None,
            Self::Shutdown(worker_id) => Some(worker_id),
        }
    }
}

impl TurnCommands {
    pub(in crate::server) fn recover_lost_starts(&self) -> Result<Vec<String>, TurnCommandError> {
        let effect_ids = {
            let connection = self.connect()?;
            let mut statement = connection.prepare(
                "SELECT effect.effect_id FROM orchestration_v2_effect_outbox AS effect
                 JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
                   AND run.run_id = json_extract(effect.payload_json, '$.runId')
                 WHERE effect.effect_type = 'provider-turn.start'
                   AND effect.status IN ('pending', 'running', 'succeeded')
                   AND run.ordinal > 0 AND run.provider_instance_id = 'codex'
                   AND run.status IN ('starting', 'running')
                 ORDER BY effect.rowid",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut changed = Vec::new();
        for effect_id in effect_ids {
            if let Some(thread_id) =
                self.recover_unaccepted_start(&effect_id, RecoveryTrigger::Startup)?
            {
                changed.push(thread_id);
            }
        }
        Ok(changed)
    }

    pub(in crate::server) fn recover_unaccepted_start(
        &self,
        effect_id: &str,
        trigger: RecoveryTrigger<'_>,
    ) -> Result<Option<String>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = transaction
            .query_row(
                &format!("{UNACCEPTED_START}{ROOT_SCOPE_MATCH}{START_ORIGIN}"),
                params![effect_id, trigger.lease()],
                |row| {
                    Ok(Unaccepted {
                        thread_id: row.get(0)?,
                        run_id: row.get(1)?,
                        node_id: row.get(2)?,
                        run: row.get(3)?,
                        attempt: row.get(4)?,
                        node: row.get(5)?,
                        item: row.get(6)?,
                        provider_thread: row.get(7)?,
                        provider_session: row.get(8)?,
                    })
                },
            )
            .optional()?;
        let Some(state) = state else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let (trigger, reason) = trigger.labels();
        let command_id = format!(
            "command:runtime-reconcile:{trigger}:{}:{now}",
            state.thread_id
        );
        let stream = ThreadEventStream::command(&transaction, &state.thread_id, &command_id);
        let node_metadata = json!({
            "runId": state.run_id,
            "nodeId": state.node_id,
            "providerInstanceId": CODEX_INSTANCE,
        });
        for (event_type, original) in [
            ("run-attempt.updated", state.attempt),
            ("node.updated", state.node),
        ] {
            let mut payload: Map<String, Value> = serde_json::from_str(&original)?;
            if payload.get("status") == Some(&json!("running")) {
                payload.insert("status".into(), json!("pending"));
                payload.insert("startedAt".into(), Value::Null);
                stream.append(event_type, &now, &node_metadata, &Value::Object(payload))?;
            }
        }
        let mut item: Map<String, Value> = serde_json::from_str(&state.item)?;
        for (key, value) in [
            ("inputIntent", json!("queued_turn")),
            ("status", json!("pending")),
            ("startedAt", Value::Null),
            ("completedAt", Value::Null),
            ("updatedAt", json!(now)),
        ] {
            item.insert(key.into(), value);
        }
        stream.append(
            "turn-item.updated",
            &now,
            &json!({ "runId": state.run_id }),
            &Value::Object(item),
        )?;
        let mut run: Map<String, Value> = serde_json::from_str(&state.run)?;
        for (key, value) in [
            ("status", json!("queued")),
            ("queueHeld", json!(true)),
            ("queuePosition", json!(1)),
            ("startedAt", Value::Null),
        ] {
            run.insert(key.into(), value);
        }
        let mut sequence = stream.append(
            "run.updated",
            &now,
            &json!({ "runId": state.run_id, "providerInstanceId": CODEX_INSTANCE }),
            &Value::Object(run),
        )?;
        let provider_metadata = json!({ "driver": "codex", "providerInstanceId": CODEX_INSTANCE });
        let mut provider_thread: Map<String, Value> = serde_json::from_str(&state.provider_thread)?;
        if provider_thread.get("status") == Some(&json!("active")) {
            provider_thread.insert("status".into(), json!("idle"));
            provider_thread.insert("pendingBackgroundTasks".into(), json!([]));
            provider_thread.insert("updatedAt".into(), json!(now));
            sequence = stream.append(
                "provider-thread.updated",
                &now,
                &provider_metadata,
                &Value::Object(provider_thread),
            )?;
        }
        if let Some(payload) = state.provider_session {
            let mut session: Map<String, Value> = serde_json::from_str(&payload)?;
            session.insert("status".into(), json!("stopped"));
            session.insert("updatedAt".into(), json!(now));
            session.insert("lastError".into(), Value::Null);
            sequence = stream.append(
                "provider-session.updated",
                &now,
                &provider_metadata,
                &Value::Object(session),
            )?;
        }
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'cancelled', lease_owner = NULL, completed_at = ?2,
                 updated_at = ?2, last_error = ?3
             WHERE effect_id = ?1 AND status IN ('pending', 'running')",
            params![effect_id, now, reason],
        )?;
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
             ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', 'provider-runtime.reconcile')",
            params![command_id, state.thread_id, now, sequence],
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(state.thread_id))
    }
}

struct Unaccepted {
    thread_id: String,
    run_id: String,
    node_id: String,
    run: String,
    attempt: String,
    node: String,
    item: String,
    provider_thread: String,
    provider_session: Option<String>,
}

const UNACCEPTED_START: &str = "
    SELECT run.thread_id, run.run_id, node.node_id,
           run.payload_json, attempt.payload_json, node.payload_json, item.payload_json,
           provider_thread.payload_json, session.payload_json
    FROM orchestration_v2_effect_outbox AS effect
    LEFT JOIN orchestration_command_receipts AS receipt ON receipt.command_id = effect.command_id
    JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
      AND run.run_id = json_extract(effect.payload_json, '$.runId')
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id
         AND node.run_id = run.run_id
    JOIN orchestration_v2_projection_checkpoint_scopes AS scope
      ON scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId')
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = run.provider_thread_id
         AND provider_thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_turn_items AS item ON item.run_id = run.run_id
      AND item.type = 'user_message'
      AND json_extract(item.payload_json, '$.messageId') = json_extract(run.payload_json, '$.userMessageId')
      AND json_extract(item.payload_json, '$.createdBy') = 'user'
      AND json_extract(item.payload_json, '$.creationSource') = 'web'
    LEFT JOIN orchestration_v2_projection_provider_session_bindings AS binding
      ON binding.provider_session_id = provider_thread.provider_session_id AND binding.thread_id = run.thread_id
    LEFT JOIN orchestration_v2_projection_provider_sessions AS session
      ON session.provider_session_id = binding.provider_session_id AND session.status = 'ready'
    WHERE effect.effect_id = ?1 AND effect.effect_type = 'provider-turn.start'
      AND effect.status IN ('pending', 'running', 'succeeded')
      AND (?2 IS NULL OR effect.lease_owner = ?2 OR effect.status = 'succeeded')
      AND scope.kind = 'root_run'
      AND json_extract(scope.payload_json, '$.cwd') =
          COALESCE(json_extract(thread.payload_json, '$.worktreePath'), project.workspace_root)
      AND run.ordinal > 0 AND run.provider_instance_id = 'codex' AND run.status IN ('starting', 'running')
      AND attempt.attempt_ordinal = 1 AND attempt.status IN ('pending', 'running')
      AND node.kind = 'root_turn' AND node.status IN ('pending', 'running')
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_provider_turns AS native_turn
          WHERE native_turn.run_attempt_id = attempt.attempt_id
      )
";
