use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::super::native_events::{AssistantSnapshot, AssistantText, assistant_text};
use super::super::settlement_events::{CompletedRun, checkpoint_waiting};
use super::super::turn_text::assistant_position;
use super::super::{
    CLAUDE_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, encode_id_part,
    record_projected_sequence, timestamp,
};
use super::Claim;

impl Claim {
    fn native_turn_id(&self) -> String {
        format!("turn:{}", self.ids.attempt_id)
    }

    fn provider_turn_id(&self) -> String {
        format!(
            "provider-turn:provider:{CLAUDE_INSTANCE}:native-turn:{}",
            encode_id_part(&self.native_turn_id())
        )
    }

    fn provider_metadata(&self, node_id: &str) -> Value {
        json!({"runId": self.ids.run_id, "nodeId": node_id, "driver": CLAUDE_INSTANCE,
            "providerInstanceId": CLAUDE_INSTANCE})
    }

    fn run_metadata(&self) -> Value {
        json!({"runId": self.ids.run_id, "nodeId": self.ids.root_node_id,
            "providerInstanceId": CLAUDE_INSTANCE})
    }

    fn provider_stream<'a>(&'a self, transaction: &'a Transaction<'a>) -> ThreadEventStream<'a> {
        ThreadEventStream::provider(transaction, &self.thread_id, &self.ids.provider_session_id)
    }
}

const DRIVER: &str = CLAUDE_INSTANCE;

impl TurnCommands {
    pub(in crate::server) fn bind_claude_turn(
        &self,
        claim: &Claim,
        context_usage: Option<Value>,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut stored) = load_run(&tx, claim, "starting")? else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let provider_stream = claim.provider_stream(&tx);
        let provider_metadata = json!({"driver":DRIVER,"providerInstanceId":DRIVER});
        let mut session = if let Some(session) = stored.session.take() {
            session
        } else {
            let session = json!({
                "id":claim.ids.provider_session_id,"driver":DRIVER,"providerInstanceId":DRIVER,
                "status":"ready","cwd":claim.workspace,"model":claim.model,
                "capabilities": capabilities(),"extensionStatuses":[],
                "createdAt":now,"updatedAt":now,"lastError":null,
            });
            provider_stream.append(
                "provider-session.attached",
                &now,
                &provider_metadata,
                &session,
            )?;
            session
        };
        session["updatedAt"] = json!(now);
        session["status"] = json!("ready");
        provider_stream.append(
            "provider-session.updated",
            &now,
            &provider_metadata,
            &session,
        )?;
        let server_stream = ThreadEventStream::server(&tx, &claim.thread_id);
        let native_session_id = claim.target.id().to_string();
        stored.provider_thread["nativeThreadRef"] = native_ref(&native_session_id, "strong");
        stored.provider_thread["status"] = json!("active");
        stored.provider_thread["updatedAt"] = json!(now);
        stored.provider_thread["lastRunOrdinal"] = json!(claim.ordinal);
        stored.provider_thread["nativeMetadata"]["title"] = json!(claim.title);
        if let Some(usage) = context_usage {
            stored.provider_thread["contextUsage"] = usage;
        }
        server_stream.append(
            "provider-thread.updated",
            &now,
            &provider_metadata,
            &stored.provider_thread,
        )?;

        let metadata = claim.run_metadata();
        stored.attempt["nativeThreadId"] = json!(native_session_id);
        for (event_type, mut payload) in [
            ("run.updated", stored.run),
            ("run-attempt.updated", stored.attempt),
            ("node.updated", stored.node),
        ] {
            payload["startedAt"] = json!(now);
            payload["status"] = json!("running");
            server_stream.append(event_type, &now, &metadata, &payload)?;
        }
        let turn = json!({
            "id":claim.provider_turn_id(),"providerThreadId":claim.ids.provider_thread_id,
            "nodeId":claim.ids.root_node_id,"runAttemptId":claim.ids.attempt_id,
            "nativeTurnRef":native_ref(&claim.native_turn_id(),"weak"),
            "ordinal":claim.ordinal,"status":"running","startedAt":now,"completedAt":null,
        });
        let sequence = provider_stream.append(
            "provider-turn.updated",
            &now,
            &claim.provider_metadata(&claim.ids.root_node_id),
            &turn,
        )?;
        record_projected_sequence(&tx, sequence, &now)?;
        tx.commit()?;
        Ok(Some(sequence))
    }

    pub(in crate::server) fn record_claude_offer(
        &self,
        claim: &Claim,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut stored) = load_run(&tx, claim, "running")? else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let offered = tx.execute(
            "UPDATE orchestration_v2_effect_outbox SET status='succeeded', lease_owner=NULL,
                    completed_at=?1, updated_at=?1, last_error=NULL
                    WHERE effect_id=?2 AND status='running' AND lease_owner=?3",
            params![now, claim.effect_id, claim.worker_id],
        )?;
        if offered == 0 {
            return Ok(None);
        }
        let previous =
            stored.provider_thread["nativeMetadata"]["claudeAttachmentBudget"]["bytesUsed"]
                .as_u64()
                .unwrap_or(0);
        let input_bytes = serde_json::to_vec(&claim.text)?.len() as u64;
        let total = previous.saturating_add(input_bytes);
        stored.provider_thread["nativeMetadata"]["claudeAttachmentBudget"] =
            json!({"bytesUsed":total,"pagesUsed":0});
        stored.provider_thread["updatedAt"] = json!(now);
        let sequence = claim.provider_stream(&tx).append(
            "provider-thread.updated",
            &now,
            &json!({"driver":DRIVER,"providerInstanceId":DRIVER}),
            &stored.provider_thread,
        )?;
        record_projected_sequence(&tx, sequence, &now)?;
        tx.commit()?;
        Ok(Some(sequence))
    }

    pub(in crate::server) fn record_claude_text(
        &self,
        claim: &Claim,
        native_item_id: &str,
        text: &str,
        model: &str,
        context_usage: Option<Value>,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(stored) = load_run(&tx, claim, "running")? else {
            return Ok(None);
        };
        let Some(mut turn) = stored.turn else {
            return Ok(None);
        };
        let turn_item_id = format!(
            "turn-item:provider:{DRIVER}:native-item:{}",
            encode_id_part(native_item_id)
        );
        let Some(ordinal) =
            assistant_position(&tx, &claim.thread_id, &turn_item_id, claim.ordinal)?
        else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let native_turn_id = claim.native_turn_id();
        let stream = claim.provider_stream(&tx);
        let metadata = claim.provider_metadata(&claim.ids.root_node_id);
        if let Some(usage) = context_usage {
            update_context(&mut turn, model, usage, &now);
            stream.append("provider-turn.updated", &now, &metadata, &turn)?;
        }
        let events = assistant_text(
            &claim.ids,
            &claim.thread_id,
            AssistantText {
                native_turn_id: &native_turn_id,
                native_item_id,
                ordinal,
                started_at: &now,
                updated_at: &now,
                phase: None,
                snapshot: AssistantSnapshot::Completed { text },
            },
        );
        let mut sequence = 0;
        for event in events {
            sequence = stream.append(
                event.event_type,
                &now,
                &claim.provider_metadata(&event.node_id),
                &event.payload,
            )?;
        }
        record_projected_sequence(&tx, sequence, &now)?;
        tx.commit()?;
        Ok(Some(sequence))
    }

    pub(in crate::server) fn record_claude_context(
        &self,
        claim: &Claim,
        model: &str,
        usage: Value,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut turn) = load_run(&tx, claim, "running")?.and_then(|stored| stored.turn) else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        update_context(&mut turn, model, usage, &now);
        let sequence = claim.provider_stream(&tx).append(
            "provider-turn.updated",
            &now,
            &claim.provider_metadata(&claim.ids.root_node_id),
            &turn,
        )?;
        record_projected_sequence(&tx, sequence, &now)?;
        tx.commit()?;
        Ok(Some(sequence))
    }

    pub(in crate::server) fn complete_claude_turn(
        &self,
        claim: &Claim,
        assistant_id: &str,
        model: &str,
        billed_usage: Value,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(stored) = load_run(&tx, claim, "running")? else {
            return Ok(None);
        };
        let Some(mut turn) = stored.turn else {
            return Ok(None);
        };
        let provider_stream = claim.provider_stream(&tx);
        let now = timestamp(OffsetDateTime::now_utc());
        turn.as_object_mut()
            .expect("projected Claude turn")
            .shift_remove("tokenUsage");
        turn["status"] = json!("completed");
        turn["completedAt"] = json!(now);
        turn["nativeTurnRef"] = native_ref(assistant_id, "weak");
        turn["turnTokenUsage"] = billed_usage;
        turn["model"] = json!(model);
        provider_stream.append(
            "provider-turn.updated",
            &now,
            &claim.provider_metadata(&claim.ids.root_node_id),
            &turn,
        )?;
        provider_stream.append(
            "provider-thread.updated",
            &now,
            &json!({"driver":DRIVER,"providerInstanceId":DRIVER}),
            &stored.provider_thread,
        )?;
        let waiting = checkpoint_waiting(CompletedRun {
            thread_id: &claim.thread_id,
            run_id: &claim.ids.run_id,
            root_node_id: &claim.ids.root_node_id,
            provider_instance_id: DRIVER,
            checkpoint_scope_id: &claim.checkpoint_scope_id,
            completed_at: &now,
            run: serde_json::from_value(stored.run)?,
            attempt: serde_json::from_value(stored.attempt)?,
            root_node: serde_json::from_value(stored.node)?,
            provider_thread: serde_json::from_value(stored.provider_thread)?,
        });
        let stream = ThreadEventStream::server(&tx, &claim.thread_id);
        let mut sequence = 0;
        for event in waiting.events {
            sequence = stream.append(event.event_type, &now, &event.metadata, &event.payload)?;
        }
        let effect = waiting.effect;
        tx.execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                available_at, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
            params![
                effect.id,
                effect.command_id,
                effect.thread_id,
                effect.effect_type,
                effect.request.to_string(),
                now
            ],
        )?;
        record_projected_sequence(&tx, sequence, &now)?;
        tx.commit()?;
        Ok(Some(sequence))
    }
}

struct Stored {
    run: Value,
    attempt: Value,
    node: Value,
    provider_thread: Value,
    session: Option<Value>,
    turn: Option<Value>,
}

fn load_run(
    transaction: &Transaction<'_>,
    claim: &Claim,
    state: &str,
) -> Result<Option<Stored>, TurnCommandError> {
    let row = transaction
        .query_row(
            ACTIVE,
            params![
                claim.effect_id,
                claim.ids.run_id,
                claim.worker_id,
                state,
                claim.provider_turn_id(),
                claim.target.id().to_string()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((run, attempt, node, provider_thread, session, turn)) = row else {
        return Ok(None);
    };
    Ok(Some(Stored {
        run: serde_json::from_str(&run)?,
        attempt: serde_json::from_str(&attempt)?,
        node: serde_json::from_str(&node)?,
        provider_thread: serde_json::from_str(&provider_thread)?,
        session: session.as_deref().map(serde_json::from_str).transpose()?,
        turn: turn.as_deref().map(serde_json::from_str).transpose()?,
    }))
}

fn native_ref(id: &str, strength: &str) -> Value {
    json!({"driver":DRIVER,"nativeId":id,"strength":strength})
}

fn update_context(turn: &mut Value, model: &str, mut usage: Value, now: &str) {
    usage["updatedAt"] = json!(now);
    turn["nativeTurnRef"]["strength"] = json!("strong");
    turn["tokenUsage"] = usage;
    turn["model"] = json!(model);
}

fn capabilities() -> Value {
    json!({
        "sessions":{"supportsMultipleProviderThreadsPerSession":false,"supportsModelSwitchInSession":false,"supportsProviderSwitchingViaHandoff":false,"supportsRuntimeModeSwitchInSession":false,"pendingRequestsSurviveRestart":false},
        "threads":{"canCreateEmptyThread":true,"canReadThreadSnapshot":false,"canRollbackThread":false,"canForkThread":false,"canForkFromTurn":false,"canForkFromSubagentThread":false,"exposesNativeThreadId":true},
        "turns":{"exposesNativeTurnId":false,"emitsTurnStarted":true,"emitsTurnCompleted":true,"supportsInterrupt":false,"supportsActiveSteering":false,"supportsSteeringByInterruptRestart":false,"supportsQueuedMessages":false,"terminalStatusQuality":"weak"},
        "streaming":{"streamsAssistantText":false,"streamsReasoning":false,"streamsToolOutput":false,"streamsPlanText":false,"emitsMessageCompleted":true},
        "tools":{"exposesToolItemIds":false,"emitsToolStarted":false,"emitsToolCompleted":false,"emitsToolOutput":false,"supportsMcpTools":false,"supportsDynamicToolCallbacks":false},
        "approvals":{"supportsCommandApproval":false,"supportsFileReadApproval":false,"supportsFileChangeApproval":false,"supportsApplyPatchApproval":false,"approvalsHaveNativeRequestIds":false,"approvalCallbacksAreLiveOnly":false,"approvalsCanOriginateFromSubagents":false},
        "planning":{"emitsPlanUpdated":false,"emitsTodoList":false,"emitsProposedPlan":false,"supportsStructuredQuestions":false,"planDeltasHaveItemIds":false},
        "subagents":{"supportsSubagents":false,"exposesSubagentThreadIds":false,"emitsSubagentLifecycle":false,"canWaitForSubagents":false,"canCloseSubagents":false,"canForkSubagentThread":false},
        "context":{"acceptsSystemContext":true,"acceptsDeveloperContext":false,"acceptsSyntheticUserContext":false,"canGenerateSummaries":false,"canConsumeHandoffSummaries":false,"supportsDeltaHandoff":false,"supportsFullThreadHandoff":false,"maxRecommendedHandoffChars":null},
        "checkpointing":{"appCanCheckpointFilesystem":true,"supportsNestedCheckpointScopes":false,"providerCanRollbackConversation":false,"providerRollbackReturnsSnapshot":false,"providerCanReadConversationSnapshot":false},
        "identity":{"nativeThreadIds":"strong","nativeTurnIds":"weak","nativeItemIds":"strong","nativeRequestIds":"none"},
        "runtimePolicy":{"enforcement":"native"},
    })
}

const ACTIVE: &str = "
    SELECT run.payload_json, attempt.payload_json, node.payload_json, provider.payload_json,
           session.payload_json, turn.payload_json
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_v2_projection_runs AS run ON run.run_id = json_extract(effect.payload_json, '$.runId') AND run.thread_id = effect.thread_id
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId') AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider ON provider.provider_thread_id = attempt.provider_thread_id AND provider.thread_id = run.thread_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    LEFT JOIN orchestration_v2_projection_provider_session_bindings AS binding ON binding.thread_id = run.thread_id AND binding.provider_session_id = provider.provider_session_id
    LEFT JOIN orchestration_v2_projection_provider_sessions AS session ON session.provider_session_id = binding.provider_session_id
    LEFT JOIN orchestration_v2_projection_provider_turns AS turn ON turn.provider_turn_id = ?5 AND turn.run_attempt_id = attempt.attempt_id
    WHERE effect.effect_id = ?1 AND run.run_id = ?2 AND effect.effect_type = 'provider-turn.start'
      AND run.status = ?4 AND run.provider_instance_id = 'claudeAgent' AND provider.driver = 'claudeAgent'
      AND thread.active_provider_thread_id = provider.provider_thread_id
      AND ((effect.status = 'running' AND effect.lease_owner = ?3)
           OR (effect.status = 'succeeded' AND ?4 = 'running'))
      AND ((?4 = 'starting' AND attempt.status = 'pending' AND node.status = 'pending' AND turn.provider_turn_id IS NULL)
           OR (?4 = 'running' AND attempt.status = 'running' AND node.status = 'running' AND turn.status = 'running'
               AND provider.status = 'active' AND json_extract(provider.payload_json, '$.nativeThreadRef.nativeId') = ?6))
";
