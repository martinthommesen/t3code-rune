use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::json;
use t3_rune::providers::codex::{ReasoningStream, TurnKey};
use time::OffsetDateTime;

use super::provider_turn::provider_turn_id;
use super::turn_text::{ACTIVE_CODEX_TURN, assistant_position};
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, TurnIds, encode_id_part,
    record_projected_sequence, timestamp,
};

#[derive(PartialEq, Eq)]
pub(in crate::server) struct ReasoningPart {
    pub(in crate::server) native_item_id: String,
    pub(in crate::server) stream: ReasoningStream,
    pub(in crate::server) index: usize,
}

#[derive(Clone, Copy)]
pub(in crate::server) enum ReasoningStatus {
    Running,
    Completed,
    Interrupted,
}

pub(in crate::server) struct ReasoningSnapshot<'a> {
    pub(in crate::server) text: &'a str,
    pub(in crate::server) status: ReasoningStatus,
}

impl TurnCommands {
    pub(in crate::server) fn update_reasoning(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        part: &ReasoningPart,
        snapshot: Option<ReasoningSnapshot<'_>>,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider_turn = provider_turn_id(key);
        let binding = transaction
            .query_row(
                ACTIVE_CODEX_TURN,
                params![
                    thread_id,
                    run_id,
                    provider_turn,
                    key.thread_id.as_str(),
                    key.turn_id.as_str()
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((message, run_ordinal, provider_thread_id)) = binding else {
            return Ok(None);
        };
        let ids = TurnIds::for_run(thread_id, &message, run_ordinal, Some(&provider_thread_id));
        let native_id = serde_json::to_string(&(
            key.turn_id.as_str(),
            &part.native_item_id,
            part.stream,
            part.index,
        ))?;
        let encoded_id = encode_id_part(&native_id);
        let item_id = format!("turn-item:provider:codex:native-item:{encoded_id}");
        let node_id = format!("node:provider:codex:native-item:{encoded_id}");
        let Some(ordinal) = assistant_position(&transaction, thread_id, &item_id, run_ordinal)?
        else {
            return Ok(None);
        };
        let Some(snapshot) = snapshot else {
            transaction.commit()?;
            return Ok(None);
        };
        let at = timestamp(OffsetDateTime::now_utc());
        let started_at = transaction.query_row(
            "SELECT json_extract(payload_json, '$.startedAt')
             FROM orchestration_v2_projection_turn_items WHERE thread_id = ?1 AND turn_item_id = ?2",
            params![thread_id, item_id],
            |row| row.get::<_, String>(0),
        ).optional()?;
        let started_at = started_at.as_deref().unwrap_or(&at);
        let (status, completed_at) = match snapshot.status {
            ReasoningStatus::Running => ("running", None),
            ReasoningStatus::Completed => ("completed", Some(&at)),
            ReasoningStatus::Interrupted => ("interrupted", Some(&at)),
        };
        let native_ref =
            json!({"driver": "codex", "nativeId": part.native_item_id, "strength": "strong"});
        let node = json!({
            "id": node_id, "threadId": thread_id, "runId": run_id,
            "parentNodeId": ids.root_node_id, "rootNodeId": ids.root_node_id,
            "kind": "reasoning", "status": status, "countsForRun": false,
            "providerThreadId": provider_thread_id, "providerTurnId": provider_turn,
            "nativeItemRef": native_ref, "runtimeRequestId": null, "checkpointScopeId": null,
            "startedAt": started_at, "completedAt": completed_at,
        });
        let item = json!({
            "id": item_id, "threadId": thread_id, "runId": run_id, "nodeId": node_id,
            "providerThreadId": provider_thread_id, "providerTurnId": provider_turn,
            "nativeItemRef": native_ref, "parentItemId": null, "ordinal": ordinal,
            "status": status, "title": null, "startedAt": started_at,
            "completedAt": completed_at, "updatedAt": at, "type": "reasoning",
            "text": snapshot.text, "streaming": completed_at.is_none(),
        });
        let stream = ThreadEventStream::provider(&transaction, thread_id, &ids.provider_session_id);
        let metadata = json!({"runId": run_id, "nodeId": node_id, "driver": "codex", "providerInstanceId": CODEX_INSTANCE});
        stream.append("node.updated", &at, &metadata, &node)?;
        let sequence = stream.append("turn-item.updated", &at, &metadata, &item)?;
        record_projected_sequence(&transaction, sequence, &at)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}
