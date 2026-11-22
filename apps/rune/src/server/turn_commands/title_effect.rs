use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use t3_rune::providers::codex::ThreadId;
use time::OffsetDateTime;

use super::{TurnCommandError, TurnCommands, timestamp};

const APPLY_TITLE_EFFECT: &str = "provider-session.apply-thread-title";

pub(in crate::server) fn enqueue(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    title: &str,
    now: &str,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO orchestration_v2_effect_outbox (
            effect_id, command_id, thread_id, effect_type, payload_json, status,
            attempt_count, available_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
        params![
            format!("effect:{command_id}:{APPLY_TITLE_EFFECT}"),
            command_id,
            thread_id,
            APPLY_TITLE_EFFECT,
            json!({"type": APPLY_TITLE_EFFECT, "title": title}).to_string(),
            now
        ],
    )?;
    Ok(())
}

pub(in crate::server) struct ClaimedTitle {
    effect_id: String,
    pub(in crate::server) thread_id: String,
    pub(in crate::server) title: String,
    pub(in crate::server) native_thread_id: Option<ThreadId>,
}

impl TurnCommands {
    pub(in crate::server) fn thread_title(
        &self,
        thread_id: &str,
    ) -> Result<String, TurnCommandError> {
        Ok(self.read_connection()?.query_row(
            "SELECT json_extract(payload_json, '$.title')
             FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )?)
    }

    pub(in crate::server) fn claim_title_effect(
        &self,
        thread_id: Option<&str>,
        worker_id: &str,
    ) -> Result<Option<ClaimedTitle>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp(OffsetDateTime::now_utc());
        let claim = transaction.query_row(
            "SELECT candidate.effect_id, candidate.thread_id,
                    json_extract(candidate.payload_json, '$.title'),
                    CASE WHEN session.provider_session_id IS NOT NULL
                              AND provider.driver = 'codex' AND provider.provider_instance_id = 'codex'
                         THEN json_extract(provider.payload_json, '$.nativeThreadRef.nativeId') END
             FROM orchestration_v2_effect_outbox AS candidate
             JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = candidate.thread_id
             LEFT JOIN orchestration_v2_projection_provider_threads AS provider
                   ON provider.provider_thread_id = thread.active_provider_thread_id
                  AND provider.thread_id = thread.thread_id
             LEFT JOIN orchestration_v2_projection_provider_sessions AS session
                   ON session.provider_session_id = provider.provider_session_id AND session.status = 'ready'
             WHERE candidate.effect_type = 'provider-session.apply-thread-title'
               AND candidate.status = 'pending' AND candidate.available_at <= ?1
               AND (?2 IS NULL OR candidate.thread_id = ?2)
               AND json_extract(candidate.payload_json, '$.type') = candidate.effect_type
               AND json_type(candidate.payload_json, '$.title') = 'text'
               AND NOT EXISTS (
                   SELECT 1 FROM orchestration_v2_effect_outbox AS active
                   WHERE active.thread_id = candidate.thread_id
                     AND active.effect_type <> 'thread-title.generate'
                     AND (active.status = 'running' OR (active.status = 'pending' AND active.rowid < candidate.rowid))
               )
             ORDER BY candidate.available_at, candidate.created_at, candidate.effect_id LIMIT 1",
            params![now, thread_id],
            |row| Ok(ClaimedTitle {
                effect_id: row.get(0)?,
                thread_id: row.get(1)?,
                title: row.get(2)?,
                native_thread_id: row.get::<_, Option<String>>(3)?.map(ThreadId::new),
            }),
        ).optional()?;
        if let Some(claim) = &claim {
            transaction.execute(
                "UPDATE orchestration_v2_effect_outbox SET status = 'running',
                    attempt_count = attempt_count + 1, lease_owner = ?2, updated_at = ?3, last_error = NULL
                 WHERE effect_id = ?1",
                params![claim.effect_id, worker_id, now],
            )?;
            transaction.commit()?;
        }
        Ok(claim)
    }

    pub(in crate::server) fn finish_title_effect(
        &self,
        claim: &ClaimedTitle,
        worker_id: &str,
    ) -> Result<(), TurnCommandError> {
        self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox SET status = 'succeeded', lease_owner = NULL,
                completed_at = ?3, updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1 AND lease_owner = ?2 AND status = 'running'",
            params![
                claim.effect_id,
                worker_id,
                timestamp(OffsetDateTime::now_utc())
            ],
        )?;
        Ok(())
    }

    pub(in crate::server) fn cancel_lost_title_effects(&self) -> Result<(), TurnCommandError> {
        self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox SET status = 'cancelled', lease_owner = NULL,
                completed_at = ?1, updated_at = ?1,
                last_error = 'Cancelled because the server process ended before the effect completed.'
             WHERE effect_type = 'provider-session.apply-thread-title' AND status IN ('pending', 'running')",
            [timestamp(OffsetDateTime::now_utc())],
        )?;
        Ok(())
    }
}
