mod history;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use t3_rune::providers::codex::ThreadId;
use time::OffsetDateTime;
use uuid::Uuid;

use self::history::{HistoricalMessage, HistorySelection, native_budget, select, token_cap};
use super::start_effect::ClaimedTurnStart;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, encode_id_part,
    record_projected_sequence, timestamp,
};

const STRATEGY: &str = "delta_since_target_last_seen";

pub(in crate::server) struct PendingHistory {
    pub(in crate::server) native_items: Vec<Value>,
    pub(in crate::server) inline_context: String,
    pub(in crate::server) sequence: i64,
    payload: Value,
}

pub(in crate::server) enum HistoryDelivery {
    Injected,
    Inline,
}

impl TurnCommands {
    pub(in crate::server) fn prepare_unbound_history(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        native: &ThreadId,
    ) -> Result<Option<PendingHistory>, TurnCommandError> {
        if claim.run_ordinal == 1 || claim.native_thread_id.is_some() {
            return Ok(None);
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(provider_id) = transaction.query_row(
            "SELECT provider.provider_thread_id FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = run.thread_id
             JOIN orchestration_v2_projection_provider_threads AS provider
                  ON provider.provider_thread_id = run.provider_thread_id AND provider.thread_id = run.thread_id
             WHERE run.run_id = ?1 AND run.thread_id = ?2 AND run.status = 'running'
               AND effect.effect_id = ?3 AND effect.status = 'running' AND effect.lease_owner = ?4
               AND effect.effect_type = 'provider-turn.start' AND json_extract(effect.payload_json, '$.runId') = run.run_id
               AND provider.driver = 'codex' AND provider.status = 'active'
               AND json_extract(provider.payload_json, '$.nativeThreadRef.nativeId') = ?5",
            params![claim.run_id, thread_id, claim.effect_id, claim.worker_id, native.as_str()],
            |row| row.get::<_, String>(0),
        ).optional()? else {
            return Err(TurnCommandError::Unsupported("The active Codex thread has changed during history preparation."));
        };
        let (from, to, messages) = missed_messages(&transaction, claim, thread_id, &provider_id)?;
        let messages: Vec<_> = messages.iter().collect();
        let (Some(first), Some(last)) = (messages.first(), messages.last()) else {
            return Err(TurnCommandError::Unsupported(
                "The earlier interrupted run has no recoverable history.",
            ));
        };
        let coverage = format!(
            "Provider context handoff. Thread: {thread_id}. Covered app runs: {from}-{to}.\nSource item range: {} through {}.\nRecover an omitted item only when the request needs it, with t3_thread_read({{threadId:\"{thread_id}\",view:\"activity\",itemId:\"<omitted item ID>\",maxCharsPerItem:4000}}); continue long text with textOffset=nextTextOffset until null. Items included below need no read. Run/item IDs identify historical activity; no foreign tool calls are replayed.",
            first.item_id, last.item_id,
        );
        let token_cap = token_cap();
        let preview = select(&messages, &coverage, &[], 0, token_cap);
        let budget = native_budget(token_cap, &claim.turn.text);
        let mut delivery_coverage = format!("Context handoff ({STRATEGY}):\n{coverage}");
        let empty = HistorySelection {
            messages: vec![],
            omitted_ids: vec![],
            omitted_count: 0,
            context: delivery_coverage.clone(),
        };
        if empty.cost() > 4_000.min(budget / 2) {
            delivery_coverage = format!(
                "Context handoff ({STRATEGY}). 1 handoff records; detailed coverage references omitted. Recover an omitted item only when the request needs it, with t3_thread_read({{threadId:\"{thread_id}\",view:\"activity\",itemId:\"<omitted item ID>\",maxCharsPerItem:4000}}); continue long text with textOffset=nextTextOffset until null. Follow fork/handoff source references in activity. Items included below need no read."
            );
        }
        let delivery = select(
            &preview.messages,
            &delivery_coverage,
            &preview.omitted_ids,
            preview.omitted_count,
            budget,
        );
        if delivery.cost() > budget {
            return Err(TurnCommandError::Unsupported(
                "The Codex context window cannot fit the historical context and the requested prompt.",
            ));
        }

        let at = timestamp(OffsetDateTime::now_utc());
        let handoff_id = format!(
            "context-handoff:thread:{}:from-provider-instance:codex:to-provider-instance:codex:{}",
            encode_id_part(thread_id),
            Uuid::new_v4(),
        );
        let payload = json!({
            "id": handoff_id, "transferId": null, "threadId": thread_id, "targetRunId": claim.run_id,
            "fromProviderThreadIds": [provider_id], "toProviderThreadId": provider_id,
            "coveredRunOrdinals": {"from": from, "to": to}, "strategy": STRATEGY,
            "status": "ready", "summaryMessageId": null, "summaryText": preview.render(),
            "history": {
                "messages": preview.messages, "coverage": coverage,
                "omittedItems": preview.omitted_count, "omittedItemIds": preview.omitted_ids,
            },
            "delivery": {
                "nativeThreadId": native, "status": "pending",
                "itemIds": delivery.messages.iter().map(|message| &message.item_id).collect::<Vec<_>>(),
                "omittedItemIds": preview.omitted_ids.iter().chain(&delivery.omitted_ids).collect::<Vec<_>>(),
            },
            "createdByProviderInstanceId": null, "createdAt": at, "updatedAt": at,
        });
        let sequence = append(&transaction, thread_id, &claim.run_id, &at, &payload)?;
        transaction.commit()?;
        Ok(Some(PendingHistory {
            native_items: delivery.response_items(),
            inline_context: delivery.render(),
            sequence,
            payload,
        }))
    }

    pub(in crate::server) fn finish_history_delivery(
        &self,
        claim: &ClaimedTurnStart,
        thread_id: &str,
        history: PendingHistory,
        delivered: HistoryDelivery,
    ) -> Result<i64, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = timestamp(OffsetDateTime::now_utc());
        let mut payload = history.payload;
        payload["delivery"]["status"] = json!(match delivered {
            HistoryDelivery::Injected => "injected",
            HistoryDelivery::Inline => "inline",
        });
        payload["updatedAt"] = json!(at);
        let sequence = append(&transaction, thread_id, &claim.run_id, &at, &payload)?;
        transaction.commit()?;
        Ok(sequence)
    }
}

fn missed_messages(
    transaction: &Transaction<'_>,
    claim: &ClaimedTurnStart,
    thread_id: &str,
    provider_id: &str,
) -> Result<(i64, i64, Vec<HistoricalMessage>), TurnCommandError> {
    let mut statement = transaction.prepare(
        "SELECT run.ordinal, item.payload_json
         FROM orchestration_v2_projection_runs AS run
         JOIN orchestration_v2_projection_turn_items AS item ON item.run_id = run.run_id
         WHERE run.thread_id = ?1 AND item.thread_id = ?1 AND run.provider_thread_id = ?2
           AND run.ordinal < ?3 AND run.status = 'interrupted'
           AND item.type IN ('user_message', 'command_execution', 'run_interrupt_result')
           AND NOT EXISTS (
               SELECT 1 FROM orchestration_v2_projection_provider_turns AS delivered
               WHERE delivered.run_attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
           )
         ORDER BY item.ordinal, item.turn_item_id",
    )?;
    let rows = statement.query_map(params![thread_id, provider_id, claim.run_ordinal], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut messages = Vec::new();
    let mut first = None;
    let mut last = 0;
    for row in rows {
        let (run_ordinal, encoded) = row?;
        let item: Value = serde_json::from_str(&encoded)?;
        let kind = field(&item, "type")?;
        let text = match kind {
            "user_message" => field(&item, "text")?.to_owned(),
            "run_interrupt_result" => field(&item, "message")?.to_owned(),
            _ => format!(
                "Command: {}\nExit code: {}\n{}",
                field(&item, "input")?,
                item["exitCode"]
                    .as_i64()
                    .map_or("unknown".into(), |value| value.to_string()),
                item["output"].as_str().unwrap_or(""),
            ),
        };
        messages.push(HistoricalMessage {
            role: if kind == "user_message" {
                "user"
            } else {
                "assistant"
            },
            text,
            thread_id: thread_id.to_owned(),
            run_id: field(&item, "runId")?.to_owned(),
            item_id: field(&item, "id")?.to_owned(),
            provider_thread_id: provider_id.to_owned(),
            status: field(&item, "status")?.to_owned(),
            kind: kind.to_owned(),
            run_status: "interrupted",
        });
        first.get_or_insert(run_ordinal);
        last = run_ordinal;
    }
    Ok((first.unwrap_or(0), last, messages))
}

fn field<'a>(item: &'a Value, name: &str) -> Result<&'a str, TurnCommandError> {
    item[name].as_str().ok_or(TurnCommandError::Unsupported(
        "An interrupted turn item has incomplete historical context.",
    ))
}

fn append(
    transaction: &Transaction<'_>,
    thread_id: &str,
    run_id: &str,
    at: &str,
    payload: &Value,
) -> Result<i64, TurnCommandError> {
    let stream = ThreadEventStream::server(transaction, thread_id);
    let sequence = stream.append(
        "context-handoff.updated",
        at,
        &json!({"runId": run_id, "providerInstanceId": CODEX_INSTANCE}),
        payload,
    )?;
    record_projected_sequence(transaction, sequence, at)?;
    Ok(sequence)
}
