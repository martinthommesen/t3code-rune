use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use t3_rune::providers::codex::{ThreadId, TurnSnapshot};
use time::OffsetDateTime;

use super::runtime_requests::append_released_stop_approvals;
use super::start_effect::ClaimedTurnStart;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, record_projected_sequence,
    timestamp,
};

pub(super) const SHARED_CODEX_SESSION: &str = "provider-session:provider-instance:codex:shared";

#[derive(Debug, PartialEq, Eq)]
pub(in crate::server) enum SessionAttachment {
    Attached(i64),
    AlreadyAttached,
}

impl TurnCommands {
    pub(in crate::server) fn record_session_ready(
        &self,
        claim: &ClaimedTurnStart,
        reuse: bool,
    ) -> Result<Option<SessionAttachment>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(binding) = prepared_binding(&transaction, claim)? else {
            return Ok(None);
        };
        if reuse && binding.session.is_some() {
            return Ok(Some(SessionAttachment::AlreadyAttached));
        }
        let now = timestamp(OffsetDateTime::now_utc());
        let previous = if reuse {
            transaction
                .query_row(
                    "SELECT payload_json FROM orchestration_v2_projection_provider_sessions
                 WHERE provider_session_id = ?1 AND status = 'ready'",
                    [&binding.provider_session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        } else {
            None
        };
        let payload = if let Some(previous) = previous {
            serde_json::from_str(&previous)?
        } else {
            json!({
            "id": binding.provider_session_id,
            "driver": "codex",
            "providerInstanceId": CODEX_INSTANCE,
            "status": "ready",
            "cwd": claim.thread.cwd,
            "model": claim.thread.model,
            "capabilities": codex_capabilities(),
            "createdAt": now,
            "updatedAt": now,
            "lastError": null,
            })
        };
        let stream = ThreadEventStream::provider(
            &transaction,
            &binding.thread_id,
            &binding.provider_session_id,
        );
        let sequence = stream.append(
            "provider-session.attached",
            &now,
            &json!({ "driver": "codex", "providerInstanceId": CODEX_INSTANCE }),
            &payload,
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(SessionAttachment::Attached(sequence)))
    }

    pub(in crate::server) fn bind_native_thread(
        &self,
        claim: &ClaimedTurnStart,
        native_thread: &ThreadId,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(binding) = prepared_binding(&transaction, claim)? else {
            return Ok(None);
        };
        if claim
            .native_thread_id
            .as_ref()
            .is_some_and(|expected| expected != native_thread)
        {
            return Ok(None);
        }
        let Some(session) = binding.session else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let mut provider_thread: Map<String, Value> =
            serde_json::from_str(&binding.provider_thread)?;
        provider_thread.insert(
            "nativeThreadRef".into(),
            json!({ "driver": "codex", "nativeId": native_thread, "strength": "strong" }),
        );
        provider_thread.entry("contextUsage").or_insert(Value::Null);
        provider_thread.insert("status".into(), json!("active"));
        provider_thread.insert("updatedAt".into(), json!(now));
        let run = start_run_record(&binding.run, &now)?;
        let mut attempt = start_run_record(&binding.attempt, &now)?;
        attempt.insert("nativeThreadId".into(), json!(native_thread));
        let node = start_run_record(&binding.node, &now)?;
        let provider_metadata = json!({ "driver": "codex", "providerInstanceId": CODEX_INSTANCE });
        let run_metadata = json!({
            "runId": claim.run_id,
            "nodeId": binding.root_node_id,
            "providerInstanceId": CODEX_INSTANCE,
        });

        let provider_stream = ThreadEventStream::provider(
            &transaction,
            &binding.thread_id,
            &binding.provider_session_id,
        );
        provider_stream.append(
            "provider-session.updated",
            &now,
            &provider_metadata,
            &serde_json::from_str(&session)?,
        )?;
        let server_stream = ThreadEventStream::server(&transaction, &binding.thread_id);
        server_stream.append(
            "provider-thread.updated",
            &now,
            &provider_metadata,
            &Value::Object(provider_thread),
        )?;
        let mut sequence = 0;
        for (event_type, payload) in [
            ("run.updated", run),
            ("run-attempt.updated", attempt),
            ("node.updated", node),
        ] {
            sequence =
                server_stream.append(event_type, &now, &run_metadata, &Value::Object(payload))?;
        }
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }

    pub(in crate::server) fn release_provider_session(
        &self,
        thread_ids: &[String],
        failure: Option<&str>,
    ) -> Result<Vec<String>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_id = SHARED_CODEX_SESSION;
        let previous: Option<String> = transaction
            .query_row(
                "SELECT payload_json FROM orchestration_v2_projection_provider_sessions
             WHERE provider_session_id = ?1 AND status = 'ready'",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let mut payload = previous
            .as_deref()
            .map(serde_json::from_str::<Map<String, Value>>)
            .transpose()?;
        if let Some(payload) = &mut payload {
            payload.insert(
                "status".into(),
                json!(if failure.is_some() {
                    "error"
                } else {
                    "stopped"
                }),
            );
            payload.insert("updatedAt".into(), json!(now));
            payload.insert("lastError".into(), json!(failure));
        }
        let payload = payload.map(Value::Object);
        let mut changed = Vec::new();
        let mut sequence = None;
        for thread_id in thread_ids {
            let bound: bool = transaction.query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM orchestration_v2_projection_provider_session_bindings
                    WHERE provider_session_id = ?1 AND thread_id = ?2
                )",
                params![session_id, thread_id],
                |row| row.get(0),
            )?;
            let stream = ThreadEventStream::provider(&transaction, thread_id, session_id);
            let stopped = if bound && let Some(payload) = &payload {
                Some(stream.append(
                    "provider-session.updated",
                    &now,
                    &json!({ "driver": "codex", "providerInstanceId": CODEX_INSTANCE }),
                    payload,
                )?)
            } else {
                None
            };
            let released =
                append_released_stop_approvals(&transaction, &stream, &now, failure.is_some())?;
            if let Some(latest) = released.or(stopped) {
                sequence = Some(latest);
                changed.push(thread_id.clone());
            }
        }
        if let Some(sequence) = sequence {
            record_projected_sequence(&transaction, sequence, &now)?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    pub(in crate::server) fn accept_provider_start(
        &self,
        claim: &ClaimedTurnStart,
        accepted: &TurnSnapshot,
    ) -> Result<bool, TurnCommandError> {
        let connection = self.connect()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let matched = connection.execute(
            "UPDATE orchestration_v2_effect_outbox AS candidate
             SET status = 'succeeded', lease_owner = NULL, completed_at = ?5,
                 updated_at = ?5, last_error = NULL
             WHERE effect_id = ?1 AND status = 'running' AND lease_owner = ?2
               AND effect_type = 'provider-turn.start'
               AND json_extract(candidate.payload_json, '$.runId') = ?3
               AND EXISTS (
                 SELECT 1 FROM orchestration_v2_projection_runs AS run
                 JOIN orchestration_v2_projection_provider_threads AS provider_thread
                    ON provider_thread.provider_thread_id = run.provider_thread_id
                 WHERE run.thread_id = candidate.thread_id AND run.run_id = ?3
                   AND provider_thread.driver = 'codex'
                   AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?4
               )",
            params![
                claim.effect_id,
                claim.worker_id,
                claim.run_id,
                accepted.key.thread_id.as_str(),
                now,
            ],
        )?;
        Ok(matched == 1)
    }
}

fn start_run_record(payload: &str, now: &str) -> serde_json::Result<Map<String, Value>> {
    let mut record: Map<String, Value> = serde_json::from_str(payload)?;
    record.insert("status".into(), json!("running"));
    if record.get("startedAt").is_none_or(Value::is_null) {
        record.insert("startedAt".into(), json!(now));
    }
    Ok(record)
}

struct PreparedBinding {
    thread_id: String,
    provider_session_id: String,
    root_node_id: String,
    session: Option<String>,
    provider_thread: String,
    run: String,
    attempt: String,
    node: String,
}

fn prepared_binding(
    transaction: &Transaction<'_>,
    claim: &ClaimedTurnStart,
) -> rusqlite::Result<Option<PreparedBinding>> {
    transaction
        .query_row(
            PREPARED_BINDING,
            params![
                claim.effect_id,
                claim.run_id,
                claim.worker_id,
                claim.native_thread_id.as_ref().map(ThreadId::as_str)
            ],
            |row| {
                Ok(PreparedBinding {
                    thread_id: row.get(0)?,
                    provider_session_id: row.get(1)?,
                    root_node_id: row.get(2)?,
                    session: row.get(3)?,
                    provider_thread: row.get(4)?,
                    run: row.get(5)?,
                    attempt: row.get(6)?,
                    node: row.get(7)?,
                })
            },
        )
        .optional()
}

const PREPARED_BINDING: &str = "
    SELECT run.thread_id, provider_thread.provider_session_id, node.node_id,
           session.payload_json, provider_thread.payload_json,
           run.payload_json, attempt.payload_json, node.payload_json
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_v2_projection_runs AS run
      ON run.run_id = json_extract(effect.payload_json, '$.runId') AND run.thread_id = effect.thread_id
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = attempt.provider_thread_id
         AND provider_thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_nodes AS node
      ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    LEFT JOIN orchestration_v2_projection_provider_session_bindings AS binding
      ON binding.thread_id = run.thread_id
         AND binding.provider_session_id = provider_thread.provider_session_id
    LEFT JOIN orchestration_v2_projection_provider_sessions AS session
      ON session.provider_session_id = binding.provider_session_id AND session.status = 'ready'
    WHERE effect.effect_id = ?1 AND run.run_id = ?2 AND effect.lease_owner = ?3
      AND effect.effect_type = 'provider-turn.start' AND effect.status = 'running'
      AND run.ordinal > 0 AND run.status = 'starting' AND run.provider_instance_id = 'codex'
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND attempt.status = 'pending' AND node.status = 'pending'
      AND provider_thread.driver = 'codex'
      AND ((?4 IS NULL AND provider_thread.status = 'not_loaded'
            AND json_extract(provider_thread.payload_json, '$.nativeThreadRef') IS NULL)
           OR (run.ordinal > 1
               AND (provider_thread.status = 'idle'
                    OR (provider_thread.status = 'not_loaded'
                        AND effect.command_id = 'command:system:start-queued:' || run.run_id))
               AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?4))
      AND thread.active_provider_thread_id = provider_thread.provider_thread_id
";

fn codex_capabilities() -> Value {
    json!({
        "sessions": {
            "supportsMultipleProviderThreadsPerSession": true,
            "supportsModelSwitchInSession": true,
            "supportsProviderSwitchingViaHandoff": true,
            "supportsRuntimeModeSwitchInSession": true,
            "pendingRequestsSurviveRestart": false,
        },
        "threads": {
            "canCreateEmptyThread": true,
            "canReadThreadSnapshot": true,
            "canRollbackThread": true,
            "canForkThread": true,
            "canForkFromTurn": true,
            "canForkWhileSourceRunning": true,
            "canForkFromSubagentThread": true,
            "exposesNativeThreadId": true,
        },
        "turns": {
            "exposesNativeTurnId": true,
            "emitsTurnStarted": true,
            "emitsTurnCompleted": true,
            "supportsInterrupt": true,
            "supportsNativeResume": true,
            "supportsActiveSteering": true,
            "activeSteeringDuringCompaction": true,
            "supportsSteeringByInterruptRestart": true,
            "supportsQueuedMessages": true,
            "terminalStatusQuality": "strong",
        },
        "streaming": {
            "streamsAssistantText": true,
            "streamsReasoning": true,
            "streamsToolOutput": true,
            "streamsPlanText": true,
            "emitsMessageCompleted": true,
        },
        "tools": {
            "exposesToolItemIds": true,
            "emitsToolStarted": true,
            "emitsToolCompleted": true,
            "emitsToolOutput": true,
            "supportsMcpTools": true,
            "supportsDynamicToolCallbacks": true,
        },
        "approvals": {
            "supportsCommandApproval": true,
            "supportsFileReadApproval": true,
            "supportsFileChangeApproval": true,
            "supportsApplyPatchApproval": true,
            "approvalsHaveNativeRequestIds": true,
            "approvalCallbacksAreLiveOnly": true,
            "approvalsCanOriginateFromSubagents": true,
        },
        "planning": {
            "emitsPlanUpdated": true,
            "emitsTodoList": true,
            "emitsProposedPlan": true,
            "supportsStructuredQuestions": true,
            "planDeltasHaveItemIds": true,
        },
        "subagents": {
            "supportsSubagents": true,
            "exposesSubagentThreadIds": true,
            "emitsSubagentLifecycle": true,
            "canWaitForSubagents": true,
            "canCloseSubagents": true,
            "canForkSubagentThread": true,
        },
        "context": {
            "acceptsSystemContext": true,
            "acceptsDeveloperContext": true,
            "acceptsSyntheticUserContext": true,
            "canGenerateSummaries": true,
            "canConsumeHandoffSummaries": true,
            "supportsDeltaHandoff": true,
            "supportsFullThreadHandoff": true,
            "maxRecommendedHandoffChars": null,
        },
        "checkpointing": {
            "appCanCheckpointFilesystem": true,
            "supportsNestedCheckpointScopes": true,
            "providerCanRollbackConversation": true,
            "providerRollbackReturnsSnapshot": true,
            "providerCanReadConversationSnapshot": true,
        },
        "identity": {
            "nativeThreadIds": "strong",
            "nativeTurnIds": "strong",
            "nativeItemIds": "strong",
            "nativeRequestIds": "strong",
        },
        "runtimePolicy": { "enforcement": "native" },
    })
}
