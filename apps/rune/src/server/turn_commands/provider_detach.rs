use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;

use super::provider_session::SHARED_CODEX_SESSION;
use super::{
    ThreadEventStream, TurnCommandError, TurnCommands, record_projected_sequence, timestamp,
};

pub(in crate::server) const COMMAND: &str = "provider-session.detach";
const EVENT: &str = "provider-session.detached";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct Detach {
    command_id: String,
    pub(in crate::server) thread_id: String,
    provider_session_id: String,
    reason: Option<String>,
}

impl TurnCommands {
    pub(in crate::server) fn detach_provider_session(
        &self,
        command: &Detach,
    ) -> Result<(i64, bool), TurnCommandError> {
        let Detach {
            command_id,
            thread_id,
            provider_session_id,
            reason,
        } = command;
        if command_id.trim().is_empty()
            || thread_id.trim().is_empty()
            || provider_session_id.trim().is_empty()
        {
            return Err(TurnCommandError::Unsupported(
                "A command, thread and provider session ID are required.",
            ));
        }
        let mut database = self.connect()?;
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((replay, sequence)) = transaction
            .query_row(
                "SELECT aggregate_kind = 'thread' AND aggregate_id = ?2 AND command_type = ?3
                    AND status = 'accepted', result_sequence
               FROM orchestration_command_receipts WHERE command_id = ?1",
                params![command_id, thread_id, COMMAND],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            return if replay {
                Ok((sequence, false))
            } else {
                Err(TurnCommandError::Unsupported(
                    "This command ID already has a receipt for a different operation or outcome.",
                ))
            };
        }
        if !transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM orchestration_v2_projection_provider_session_bindings
                 WHERE thread_id = ?1 AND provider_session_id = ?2
             )",
            params![thread_id, provider_session_id],
            |row| row.get::<_, bool>(0),
        )? {
            return Err(TurnCommandError::ProviderSessionNotBound {
                session_id: provider_session_id.clone(),
                thread_id: thread_id.clone(),
            });
        }
        let ordinary_idle = provider_session_id == SHARED_CODEX_SESSION && transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM orchestration_v2_projection_threads AS thread
                JOIN orchestration_v2_projection_provider_threads AS native
                  ON native.provider_thread_id = thread.active_provider_thread_id
                 AND native.thread_id = thread.thread_id
                WHERE thread.thread_id = ?1 AND thread.deleted_at IS NULL
                  AND json_extract(thread.payload_json, '$.lineage.rootThreadId') = thread.thread_id
                  AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
                  AND json_extract(thread.payload_json, '$.forkedFrom') IS NULL
                  AND native.provider_session_id = ?2
                  AND native.driver = 'codex' AND native.provider_instance_id = 'codex' AND native.status = 'idle'
                  AND json_extract(native.payload_json, '$.nativeThreadRef.strength') = 'strong'
                  AND json_type(native.payload_json, '$.nativeThreadRef.nativeId') = 'text'
                  AND COALESCE(json_array_length(native.payload_json, '$.pendingBackgroundTasks'), 0) = 0
                  AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_provider_threads AS related
                       WHERE related.thread_id = thread.thread_id AND related.provider_session_id = ?2
                         AND related.provider_thread_id <> native.provider_thread_id
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_runs AS run
                       WHERE run.thread_id = thread.thread_id
                         AND run.status IN ('preparing', 'queued', 'starting', 'running', 'waiting')
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_runtime_requests AS request
                       WHERE request.thread_id = thread.thread_id AND request.status = 'pending'
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_turn_items AS item
                       WHERE item.thread_id = thread.thread_id AND item.type = 'command_execution'
                         AND item.status = 'running'
                  )
             )",
            params![thread_id, provider_session_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !ordinary_idle {
            return Err(TurnCommandError::Unsupported(
                "Only idle ordinary Codex threads support provider session detach.",
            ));
        }
        let now = timestamp(OffsetDateTime::now_utc());
        let mut event = json!({"providerSessionId": provider_session_id, "detachedAt": now});
        let mut effect = json!({"type": COMMAND, "providerSessionId": provider_session_id});
        if let Some(reason) = reason {
            event["reason"] = json!(reason);
            effect["detail"] = json!(reason);
        }
        let sequence = ThreadEventStream::command(&transaction, thread_id, command_id).append(
            EVENT,
            &now,
            &json!({"driver": "codex", "providerInstanceId": "codex"}),
            &event,
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, command_type, accepted_at, result_sequence, status
             ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, 'accepted')",
            params![command_id, thread_id, COMMAND, now, sequence],
        )?;
        transaction.execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status,
                attempt_count, available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
            params![
                format!("effect:{command_id}:{COMMAND}:{provider_session_id}"),
                command_id,
                thread_id,
                COMMAND,
                effect.to_string(),
                now
            ],
        )?;
        transaction.commit()?;
        Ok((sequence, true))
    }

    pub(in crate::server) fn claim_provider_detach(
        &self,
        effect_id: &str,
        worker: &str,
    ) -> Result<Option<String>, TurnCommandError> {
        Ok(self.connect()?.query_row(
            "UPDATE orchestration_v2_effect_outbox AS effect
             SET status = 'running', attempt_count = attempt_count + 1,
                 lease_owner = ?2, updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1 AND effect_type = 'provider-session.detach'
               AND status = 'pending' AND available_at <= ?3
               AND json_extract(payload_json, '$.providerSessionId') = ?4
               AND json_type(payload_json, '$.revokeMcpCredential') IS NULL
               AND EXISTS (
                   SELECT 1 FROM orchestration_command_receipts AS receipt
                    WHERE receipt.command_id = effect.command_id AND receipt.aggregate_id = effect.thread_id
                      AND receipt.command_type = effect.effect_type AND receipt.status = 'accepted'
               )
               AND NOT EXISTS (
                   SELECT 1 FROM orchestration_v2_effect_outbox AS ahead
                    WHERE ahead.thread_id = effect.thread_id AND ahead.effect_id <> effect.effect_id
                      AND ahead.effect_type <> 'thread-title.generate'
                      AND (ahead.status = 'running' OR (ahead.status = 'pending' AND ahead.rowid < effect.rowid))
               )
             RETURNING thread_id",
            params![effect_id, worker, timestamp(OffsetDateTime::now_utc()), SHARED_CODEX_SESSION],
            |row| row.get(0),
        ).optional()?)
    }

    pub(in crate::server) fn finish_provider_detach(
        &self,
        effect_id: &str,
        worker: &str,
    ) -> Result<(), TurnCommandError> {
        self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox SET status = 'succeeded', lease_owner = NULL,
                completed_at = ?3, updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1 AND lease_owner = ?2 AND status = 'running'
               AND effect_type = 'provider-session.detach'",
            params![effect_id, worker, timestamp(OffsetDateTime::now_utc())],
        )?;
        Ok(())
    }

    pub(in crate::server) fn requeue_provider_detaches(
        &self,
        claim: Option<(&str, &str)>,
    ) -> Result<bool, TurnCommandError> {
        let (effect_id, worker) = claim.unzip();
        Ok(self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox AS effect SET status = 'pending',
                lease_owner = NULL, lease_expires_at = NULL, updated_at = ?3
             WHERE effect_type = 'provider-session.detach' AND status = 'running'
               AND (?1 IS NULL OR (effect_id = ?1 AND lease_owner = ?2))
               AND EXISTS (
                   SELECT 1 FROM orchestration_command_receipts AS receipt
                    WHERE receipt.command_id = effect.command_id AND receipt.aggregate_id = effect.thread_id
                      AND receipt.command_type = effect.effect_type AND receipt.status = 'accepted'
               )",
            params![effect_id, worker, timestamp(OffsetDateTime::now_utc())],
        )? != 0)
    }
}

#[cfg(test)]
mod tests;
