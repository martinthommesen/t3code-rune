use std::collections::HashMap;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use t3_rune::providers::codex::{TodoListStep, TodoListStepStatus, TodoListUpdate, TurnKey};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::provider_turn::provider_turn_id;
use super::turn_text::ACTIVE_CODEX_TURN;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, TurnIds,
    allocate_turn_item_in_run, encode_id_part, record_projected_sequence, timestamp,
};

#[derive(Deserialize)]
struct PreviousPlan {
    id: String,
    steps: Vec<TimedStep<TodoListStep>>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimedStep<Step> {
    #[serde(flatten)]
    step: Step,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_anchor_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

fn timed_steps<'a>(
    steps: &'a [TodoListStep],
    previous: &[TimedStep<TodoListStep>],
    now: OffsetDateTime,
    at: &str,
) -> Vec<TimedStep<&'a TodoListStep>> {
    use TodoListStepStatus::{Completed, Pending, Running};

    let previous: HashMap<_, _> = previous
        .iter()
        .map(|step| (step.step.id.as_str(), step))
        .collect();
    let previous_step = |step: &TodoListStep| {
        previous
            .get(step.id.as_str())
            .copied()
            .filter(|prior| prior.step.text == step.text)
    };
    let has_new_completion = steps.iter().any(|step| {
        step.status == Completed
            && previous_step(step).is_none_or(|prior| prior.step.status != Completed)
    });
    let mut fallback_completion_consumed = false;
    steps
        .iter()
        .map(|step| {
            let prior = previous_step(step);
            let prior_status = prior.map(|prior| prior.step.status);
            let prior_anchor = prior.and_then(|prior| prior.duration_anchor_at.as_deref());
            let (duration_anchor_at, duration_ms) = match (step.status, prior_status) {
                (Completed, Some(Completed)) => (None, prior.and_then(|prior| prior.duration_ms)),
                (Completed, _) => {
                    let duration = if prior_status == Some(Running) || !fallback_completion_consumed
                    {
                        prior_anchor
                            .and_then(|anchor| OffsetDateTime::parse(anchor, &Rfc3339).ok())
                            .and_then(|anchor| {
                                let millis = now.unix_timestamp_nanos().div_euclid(1_000_000)
                                    - anchor.unix_timestamp_nanos().div_euclid(1_000_000);
                                u64::try_from(millis).ok().filter(|millis| *millis > 0)
                            })
                    } else {
                        None
                    };
                    fallback_completion_consumed = true;
                    (None, duration)
                }
                (Running, Some(Running)) => (Some(prior_anchor.unwrap_or(at).to_owned()), None),
                (Pending, Some(Pending)) if !has_new_completion => {
                    (Some(prior_anchor.unwrap_or(at).to_owned()), None)
                }
                (Pending | Running, _) => (Some(at.to_owned()), None),
            };
            TimedStep {
                step,
                duration_anchor_at,
                duration_ms,
            }
        })
        .collect()
}

impl TurnCommands {
    pub(in crate::server) fn record_todo_list(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        update: &TodoListUpdate,
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
        let Some((message_id, run_ordinal, provider_thread_id)) = binding else {
            return Ok(None);
        };
        let ids = TurnIds::for_run(
            thread_id,
            &message_id,
            run_ordinal,
            Some(&provider_thread_id),
        );
        let native_id = format!("turn-plan:{}", key.turn_id.as_str());
        let encoded_id = encode_id_part(&native_id);
        let node_id = format!("node:provider:codex:native-item:{encoded_id}");
        let item_id = format!("turn-item:provider:codex:native-item:{encoded_id}");
        let now = OffsetDateTime::now_utc();
        let at = timestamp(now);
        let previous = transaction.query_row(
            "SELECT item.ordinal, json_extract(item.payload_json, '$.startedAt'), plan.payload_json
             FROM orchestration_v2_projection_turn_items AS item
             JOIN orchestration_v2_projection_plans AS plan
               ON plan.plan_id = json_extract(item.payload_json, '$.planId')
               AND plan.thread_id = item.thread_id AND plan.run_id = item.run_id
             WHERE item.thread_id = ?1 AND item.turn_item_id = ?2 AND item.run_id = ?3
               AND item.provider_turn_id = ?4 AND item.type = 'todo_list' AND plan.kind = 'todo_list'",
            params![thread_id, item_id, run_id, provider_turn],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        ).optional()?;
        let (ordinal, started_at, previous) = match previous {
            Some((ordinal, started_at, plan)) => (
                ordinal,
                started_at,
                serde_json::from_str::<PreviousPlan>(&plan)?,
            ),
            None => (
                allocate_turn_item_in_run(&transaction, thread_id, &item_id, run_ordinal)?,
                at.clone(),
                PreviousPlan {
                    id: format!(
                        "plan:thread:{}:provider:codex:run:{}:{}",
                        encode_id_part(thread_id),
                        encode_id_part(run_id),
                        Uuid::new_v4()
                    ),
                    steps: Vec::new(),
                },
            ),
        };
        let native_ref = json!({"driver": "codex", "nativeId": native_id, "strength": "strong"});
        let node = json!({
            "id": node_id, "threadId": thread_id, "runId": run_id,
            "parentNodeId": ids.root_node_id, "rootNodeId": ids.root_node_id,
            "kind": "todo_list", "status": "completed", "countsForRun": false,
            "providerThreadId": provider_thread_id, "providerTurnId": provider_turn,
            "nativeItemRef": native_ref, "runtimeRequestId": null, "checkpointScopeId": null,
            "startedAt": started_at, "completedAt": at,
        });
        let mut plan = json!({
            "id": previous.id, "threadId": thread_id, "runId": run_id, "nodeId": node_id,
            "kind": "todo_list", "status": "active", "steps": timed_steps(&update.steps, &previous.steps, now, &at),
        });
        let mut item = json!({
            "id": item_id, "threadId": thread_id, "runId": run_id, "nodeId": node_id,
            "providerThreadId": provider_thread_id, "providerTurnId": provider_turn,
            "nativeItemRef": native_ref, "parentItemId": null, "ordinal": ordinal,
            "status": "completed", "title": null, "startedAt": started_at,
            "completedAt": at, "updatedAt": at, "type": "todo_list", "planId": previous.id, "steps": update.steps,
        });
        if let Some(explanation) = &update.explanation {
            plan["explanation"] = json!(explanation);
            item["explanation"] = json!(explanation);
        }
        let stream = ThreadEventStream::provider(&transaction, thread_id, &ids.provider_session_id);
        let metadata = json!({"runId": run_id, "nodeId": node_id, "driver": "codex", "providerInstanceId": CODEX_INSTANCE});
        stream.append("node.updated", &at, &metadata, &node)?;
        stream.append("plan.updated", &at, &metadata, &plan)?;
        let sequence = stream.append("turn-item.updated", &at, &metadata, &item)?;
        record_projected_sequence(&transaction, sequence, &at)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}
