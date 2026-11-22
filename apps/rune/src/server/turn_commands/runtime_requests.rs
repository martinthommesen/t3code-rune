mod release_approvals;

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use t3_rune::providers::codex::{
    ApprovalDecision, ApprovalKind, ApprovalRequest, TurnKey, UserInputRequest,
};
use time::OffsetDateTime;
use uuid::Uuid;

pub(super) use self::release_approvals::append_released_stop_approvals;
use super::provider_turn::provider_turn_id;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    encode_id_part, record_projected_sequence, timestamp,
};

const RESPOND: &str = "runtime-request.respond";

#[derive(Clone)]
pub(in crate::server) enum LiveRequest {
    Approval {
        native: ApprovalRequest,
        file_prompt: Option<String>,
    },
    Input(UserInputRequest),
}

pub(in crate::server) struct RecordedRequest {
    pub(in crate::server) id: String,
    pub(in crate::server) sequence: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(in crate::server) struct RuntimeResponse {
    #[serde(rename = "type")]
    command_type: ResponseCommand,
    command_id: String,
    thread_id: String,
    request_id: String,
    decision: Option<ApprovalDecision>,
    answers: Option<BTreeMap<String, SubmittedAnswer>>,
}

#[derive(Deserialize)]
enum ResponseCommand {
    #[serde(rename = "runtime-request.respond")]
    Respond,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SubmittedAnswer {
    Text(String),
    Choices(Vec<String>),
    Wrapped { answers: Vec<String> },
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub(in crate::server) enum Answer {
    Text(String),
    Choices(Vec<String>),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub(in crate::server) enum ResponseValue {
    Approval { decision: ApprovalDecision },
    Input { answers: BTreeMap<String, Answer> },
}

pub(in crate::server) struct ResponseReceipt {
    pub(in crate::server) sequence: i64,
    pub(in crate::server) committed: bool,
}

impl ResponseValue {
    pub(in crate::server) fn native_answers(&self) -> Option<BTreeMap<String, Vec<String>>> {
        match self {
            Self::Input { answers } => Some(
                answers
                    .iter()
                    .map(|(id, answer)| {
                        let values = match answer {
                            Answer::Text(text) => vec![text.clone()],
                            Answer::Choices(choices) => choices.clone(),
                        };
                        (id.clone(), values)
                    })
                    .collect(),
            ),
            Self::Approval { .. } => None,
        }
    }

    fn accepted_node_status(&self) -> &'static str {
        match self {
            Self::Approval {
                decision: ApprovalDecision::Decline | ApprovalDecision::Cancel,
            } => "cancelled",
            _ => "completed",
        }
    }
}

impl LiveRequest {
    fn descriptors(&self) -> Result<RequestDescriptor<'_>, TurnCommandError> {
        match self {
            Self::Approval {
                native: approval,
                file_prompt,
            } => {
                let kind = match approval.kind {
                    ApprovalKind::Command => "command",
                    ApprovalKind::FileRead => "file-read",
                    ApprovalKind::FileChange => "file-change",
                };
                let (native_id, prompt) = if approval.permissions().is_some() {
                    (approval.item_id.as_str(), approval.reason.as_deref())
                } else if approval.kind == ApprovalKind::Command {
                    (
                        approval.approval_id.as_deref().unwrap_or(&approval.item_id),
                        approval.reason.as_deref().or(approval.command.as_deref()),
                    )
                } else {
                    (approval.item_id.as_str(), file_prompt.as_deref())
                };
                let mut extra = json!({"requestKind": kind});
                if let Some(prompt) = prompt {
                    extra["prompt"] = json!(prompt);
                }
                Ok(RequestDescriptor {
                    item_id: &approval.item_id,
                    native_id,
                    kind,
                    artifact_kind: "approval_request",
                    extra,
                })
            }
            Self::Input(input) if input.questions.iter().any(|question| question.is_secret) => Err(
                TurnCommandError::Unsupported("This request requires secret input."),
            ),
            Self::Input(input) => Ok(RequestDescriptor {
                item_id: &input.item_id,
                native_id: &input.item_id,
                kind: "user_input",
                artifact_kind: "user_input_request",
                extra: json!({ "questions": input.questions.iter().enumerate().map(|(index, question)| {
                    let fallback = format!("question-{}", index + 1);
                    json!({
                        "id": nonempty(&question.id, &fallback),
                        "header": nonempty(&question.header, "Question"),
                        "question": nonempty(&question.question, "Choose an answer."),
                        "options": question.options.as_deref().unwrap_or_default()
                            .iter().enumerate().map(|(index, option)| {
                                let label = format!("Option {}", index + 1);
                                json!({
                                    "label": nonempty(&option.label, &label),
                                    "description": nonempty(&option.description, &option.label),
                                })
                            }).collect::<Vec<_>>(),
                    })
                }).collect::<Vec<_>>() }),
            }),
        }
    }
}

struct RequestDescriptor<'a> {
    item_id: &'a str,
    native_id: &'a str,
    kind: &'a str,
    artifact_kind: &'a str,
    extra: Value,
}

fn nonempty<'a>(text: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

impl TurnCommands {
    pub(in crate::server) fn record_runtime_request(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        request: &LiveRequest,
    ) -> Result<Option<RecordedRequest>, TurnCommandError> {
        let descriptor = request.descriptors()?;
        let provider_turn = provider_turn_id(key);
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding = transaction
            .query_row(
                REQUEST_BINDING,
                params![
                    thread_id,
                    run_id,
                    provider_turn,
                    key.thread_id.as_str(),
                    key.turn_id.as_str(),
                    "running"
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((root_node_id, provider_thread_id, session_id, run_ordinal)) = binding else {
            return Ok(None);
        };
        let id = format!(
            "runtime-request:provider:codex:provider-turn:{}:native-request:{}:{}",
            encode_id_part(&provider_turn),
            encode_id_part(descriptor.native_id),
            Uuid::new_v4(),
        );
        let native_item = encode_id_part(descriptor.item_id);
        let native_node_id = format!("node:provider:codex:native-item:{native_item}");
        let (node_id, parent_node_id, item_id) = match request {
            LiveRequest::Approval { .. } => {
                let parent = encode_id_part(&id);
                (
                    format!("node:runtime-request:{parent}"),
                    native_node_id,
                    format!("turn-item:runtime-request:{parent}"),
                )
            }
            LiveRequest::Input(_) => (
                native_node_id,
                root_node_id.clone(),
                format!("turn-item:provider:codex:native-item:{native_item}"),
            ),
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let ordinal = allocate_turn_item_in_run(&transaction, thread_id, &item_id, run_ordinal)?;
        let item_ref =
            json!({ "driver": "codex", "nativeId": descriptor.item_id, "strength": "strong" });
        let mut item = json!({
            "id": item_id,
            "threadId": thread_id,
            "runId": run_id,
            "nodeId": node_id,
            "providerThreadId": provider_thread_id,
            "providerTurnId": provider_turn,
            "nativeItemRef": item_ref,
            "parentItemId": null,
            "ordinal": ordinal,
            "status": "waiting",
            "title": null,
            "startedAt": now,
            "completedAt": null,
            "updatedAt": now,
            "type": descriptor.artifact_kind,
            "requestId": id,
        });
        if let (Some(item), Value::Object(extra)) = (item.as_object_mut(), descriptor.extra) {
            item.extend(extra);
        }
        let metadata = json!({
            "runId": run_id,
            "nodeId": node_id,
            "driver": "codex",
            "providerInstanceId": CODEX_INSTANCE,
        });
        let stream = ThreadEventStream::provider(&transaction, thread_id, &session_id);
        stream.append(
            "node.updated",
            &now,
            &metadata,
            &json!({
                "id": node_id,
                "threadId": thread_id,
                "runId": run_id,
                "parentNodeId": parent_node_id,
                "rootNodeId": root_node_id,
                "kind": descriptor.artifact_kind,
                "status": "waiting",
                "countsForRun": false,
                "providerThreadId": provider_thread_id,
                "providerTurnId": provider_turn,
                "nativeItemRef": item_ref,
                "runtimeRequestId": id,
                "checkpointScopeId": null,
                "startedAt": now,
                "completedAt": null,
            }),
        )?;
        stream.append("turn-item.updated", &now, &metadata, &item)?;
        let sequence = stream.append("runtime-request.updated", &now, &metadata, &json!({
            "id": id,
            "nodeId": node_id,
            "providerTurnId": provider_turn,
            "nativeRequestRef": { "driver":"codex", "nativeId":descriptor.native_id, "strength":"strong" },
            "kind": descriptor.kind,
            "status": "pending",
            "responseCapability": { "type": "live", "providerSessionId": session_id },
            "createdAt": now,
            "resolvedAt": null,
        }))?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(RecordedRequest { id, sequence }))
    }

    pub(in crate::server) fn claim_runtime_response(
        &self,
        thread_id: &str,
        key: &TurnKey,
        request_id: &str,
        worker_id: &str,
    ) -> Result<Option<ClaimedResponse>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider_turn = provider_turn_id(key);
        let now = timestamp(OffsetDateTime::now_utc());
        let matched = transaction
            .query_row(
                CLAIM_RESPONSE,
                params![
                    thread_id,
                    provider_turn,
                    request_id,
                    now,
                    key.thread_id.as_str(),
                    key.turn_id.as_str()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((effect_id, payload)) = matched else {
            return Ok(None);
        };
        let response: ResponseValue = serde_json::from_str(&payload)?;
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'running', attempt_count = attempt_count + 1, lease_owner = ?2,
                 updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1",
            params![effect_id, worker_id, now],
        )?;
        transaction.commit()?;
        Ok(Some(ClaimedResponse {
            effect_id,
            response,
        }))
    }

    pub(in crate::server) fn finish_runtime_response(
        &self,
        effect: &ClaimedResponse,
        worker_id: &str,
        failure: Option<&str>,
    ) -> Result<(), TurnCommandError> {
        let connection = self.connect()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let changed = connection.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = ?3, lease_owner = NULL, completed_at = ?4,
                 updated_at = ?4, last_error = ?5
             WHERE effect_id = ?1 AND effect_type = 'runtime-request.respond'
               AND status = 'running' AND lease_owner = ?2",
            params![
                effect.effect_id,
                worker_id,
                if failure.is_some() {
                    "failed"
                } else {
                    "succeeded"
                },
                now,
                failure,
            ],
        )?;
        if changed != 1 {
            return Err(TurnCommandError::Unsupported(
                "The runtime response is no longer owned by this worker.",
            ));
        }
        Ok(())
    }

    pub(in crate::server) fn respond_runtime_request(
        &self,
        command: RuntimeResponse,
    ) -> Result<ResponseReceipt, TurnCommandError> {
        let RuntimeResponse {
            command_type: ResponseCommand::Respond,
            command_id,
            thread_id,
            request_id,
            decision,
            answers,
        } = command;
        if [&command_id, &thread_id, &request_id]
            .iter()
            .any(|id| id.trim().is_empty())
        {
            return Err(TurnCommandError::Unsupported(
                "Approval responses require command, thread and request IDs.",
            ));
        }
        let response = match (decision, answers) {
            (Some(decision), None) => ResponseValue::Approval { decision },
            (None, Some(answers)) => ResponseValue::Input {
                answers: answers
                    .into_iter()
                    .map(|(id, answer)| {
                        (
                            id,
                            match answer {
                                SubmittedAnswer::Text(text) => Answer::Text(text),
                                SubmittedAnswer::Choices(choices)
                                | SubmittedAnswer::Wrapped { answers: choices } => {
                                    Answer::Choices(choices)
                                }
                            },
                        )
                    })
                    .collect(),
            },
            _ => {
                return Err(TurnCommandError::Unsupported(
                    "A runtime response requires either an approval decision or question answers.",
                ));
            }
        };
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = transaction
            .query_row(
                "SELECT aggregate_kind, aggregate_id, command_type, status, result_sequence
             FROM orchestration_command_receipts WHERE command_id = ?1",
                [&command_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((kind, aggregate, command, status, sequence)) = receipt {
            if kind == "thread"
                && aggregate == thread_id
                && command == RESPOND
                && status == "accepted"
            {
                return Ok(ResponseReceipt {
                    sequence,
                    committed: false,
                });
            }
            return Err(TurnCommandError::Unsupported(
                "The command ID already has a different receipt.",
            ));
        }
        let response_target = transaction
            .query_row(TARGET_RESPONSE, params![thread_id, request_id], |row| {
                Ok(ResponseTarget {
                    run_id: row.get(0)?,
                    node_id: row.get(1)?,
                    kind: row.get(2)?,
                    session_id: row.get(3)?,
                    request: row.get(4)?,
                    node: row.get(5)?,
                    item: row.get(6)?,
                })
            })
            .optional()?;
        let Some(target) = response_target else {
            return Err(TurnCommandError::Unsupported(
                "This Codex runtime request is no longer pending.",
            ));
        };
        let mut item: Map<String, Value> = serde_json::from_str(&target.item)?;
        validate_response(&target.kind, &response, &mut item, &request_id)?;
        let now = timestamp(OffsetDateTime::now_utc());
        let status = response.accepted_node_status();
        let mut request: Map<String, Value> = serde_json::from_str(&target.request)?;
        let mut node: Map<String, Value> = serde_json::from_str(&target.node)?;
        for (key, value) in [("status", json!("resolved")), ("resolvedAt", json!(now))] {
            request.insert(key.into(), value);
        }
        for payload in [&mut node, &mut item] {
            payload.insert("status".into(), json!(status));
            payload.insert("completedAt".into(), json!(now));
        }
        item.insert("updatedAt".into(), json!(now));
        let response = serde_json::to_value(response)?;
        if let Value::Object(response) = &response {
            request.extend(response.clone());
        }
        let metadata = json!({
            "runId": target.run_id,
            "nodeId": target.node_id,
            "driver": "codex",
            "providerInstanceId": CODEX_INSTANCE,
        });
        let stream = ThreadEventStream::command(&transaction, &thread_id, &command_id);
        stream.append(
            "runtime-request.updated",
            &now,
            &metadata,
            &Value::Object(request),
        )?;
        stream.append("node.updated", &now, &metadata, &Value::Object(node))?;
        let sequence = stream.append("turn-item.updated", &now, &metadata, &Value::Object(item))?;
        let effect_id = format!("effect:{command_id}:{RESPOND}:{request_id}");
        let mut effect = json!({
            "type": RESPOND,
            "requestId": request_id,
            "providerSessionId": target.session_id,
        });
        if let (Some(effect), Value::Object(response)) = (effect.as_object_mut(), response) {
            effect.extend(response);
        }
        transaction.execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
            params![
                effect_id,
                command_id,
                thread_id,
                RESPOND,
                effect.to_string(),
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
             ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', ?5)",
            params![command_id, thread_id, now, sequence, RESPOND],
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(ResponseReceipt {
            sequence,
            committed: true,
        })
    }
}

pub(super) fn dismiss_native_user_inputs(
    transaction: &rusqlite::Transaction<'_>,
    stream: &ThreadEventStream<'_>,
    run_id: &str,
    provider_turn: &str,
    now: &str,
) -> Result<(), TurnCommandError> {
    let events = {
        let mut query = transaction.prepare(include_str!("runtime_requests/dismiss_inputs.sql"))?;
        query
            .query_map(params![stream.thread_id, provider_turn], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (event, node_id, payload) in events {
        let mut payload: Map<String, Value> = serde_json::from_str(&payload)?;
        payload.insert("status".into(), json!("cancelled"));
        if event == "runtime-request.updated" {
            payload.insert("resolvedAt".into(), json!(now));
        } else {
            payload.insert("completedAt".into(), json!(now));
        }
        if event == "turn-item.updated" {
            payload.insert("updatedAt".into(), json!(now));
        }
        stream.append(&event, now, &json!({
            "runId":run_id, "nodeId":node_id, "driver":"codex", "providerInstanceId":CODEX_INSTANCE,
        }), &Value::Object(payload))?;
    }
    Ok(())
}

pub(in crate::server) struct ClaimedResponse {
    effect_id: String,
    pub(in crate::server) response: ResponseValue,
}

const CLAIM_RESPONSE: &str = "
    SELECT effect.effect_id, effect.payload_json
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_command_receipts AS receipt ON receipt.command_id = effect.command_id
    JOIN orchestration_v2_projection_runtime_requests AS request
      ON request.runtime_request_id = json_extract(effect.payload_json, '$.requestId')
         AND request.thread_id = effect.thread_id
    JOIN orchestration_v2_projection_provider_turns AS provider_turn
      ON provider_turn.provider_turn_id = request.provider_turn_id AND provider_turn.thread_id = request.thread_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = provider_turn.provider_thread_id
    JOIN orchestration_v2_projection_provider_sessions AS session
      ON session.provider_session_id = provider_thread.provider_session_id
    WHERE effect.thread_id = ?1 AND request.provider_turn_id = ?2 AND request.runtime_request_id = ?3
      AND effect.available_at <= ?4 AND effect.status = 'pending' AND effect.effect_type = 'runtime-request.respond'
      AND receipt.aggregate_kind = 'thread' AND receipt.aggregate_id = ?1
      AND receipt.command_type = 'runtime-request.respond' AND receipt.status = 'accepted'
      AND request.status = 'resolved' AND provider_turn.status = 'running' AND session.status = 'ready'
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_effect_outbox AS active
          WHERE active.thread_id = effect.thread_id AND active.effect_type != 'thread-title.generate'
            AND (active.status = 'running' OR (active.status = 'pending' AND active.rowid < effect.rowid))
      )
      AND session.provider_session_id = json_extract(effect.payload_json, '$.providerSessionId')
      AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?5
      AND json_extract(provider_turn.payload_json, '$.nativeTurnRef.nativeId') = ?6
    ORDER BY effect.rowid LIMIT 1
";

struct ResponseTarget {
    run_id: String,
    node_id: String,
    kind: String,
    session_id: String,
    request: String,
    node: String,
    item: String,
}

fn validate_response(
    kind: &str,
    response: &ResponseValue,
    item: &mut Map<String, Value>,
    request_id: &str,
) -> Result<(), TurnCommandError> {
    match (kind, response) {
        ("command" | "file-read" | "file-change", ResponseValue::Approval { .. }) => Ok(()),
        ("user_input", ResponseValue::Input { answers }) => {
            let questions = item.get("questions").and_then(Value::as_array).ok_or(
                TurnCommandError::Unsupported("This question has no saved input fields."),
            )?;
            let question_ids: BTreeSet<_> = questions
                .iter()
                .filter_map(|question| question["id"].as_str())
                .collect();
            if answers.is_empty() || !answers.keys().all(|id| question_ids.contains(id.as_str())) {
                return Err(TurnCommandError::Unsupported(
                    "The response did not answer a question in this request.",
                ));
            }
            let question_text: Map<_, _> = questions
                .iter()
                .filter_map(|question| {
                    Some((
                        question["id"].as_str()?.to_owned(),
                        question["question"].clone(),
                    ))
                })
                .collect();
            item.insert(
                "questionAnswer".into(),
                json!({
                    "requestId": request_id,
                    "answers": answers,
                    "attachmentsByQuestionId": {},
                    "questionTextById": question_text,
                }),
            );
            Ok(())
        }
        _ => Err(TurnCommandError::Unsupported(
            "The response does not match the Codex request kind.",
        )),
    }
}

pub(super) const REQUEST_BINDING: &str = "
    SELECT node.node_id, provider_thread.provider_thread_id, session.provider_session_id, run.ordinal
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId') AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = attempt.provider_thread_id AND provider_thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_provider_turns AS provider_turn
      ON provider_turn.run_attempt_id = attempt.attempt_id AND provider_turn.thread_id = run.thread_id
      AND provider_turn.provider_thread_id = provider_thread.provider_thread_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id
    JOIN orchestration_v2_projection_provider_session_bindings AS binding
      ON binding.provider_session_id = provider_thread.provider_session_id AND binding.thread_id = run.thread_id
    JOIN orchestration_v2_projection_provider_sessions AS session
      ON session.provider_session_id = binding.provider_session_id
    WHERE run.thread_id = ?1 AND run.run_id = ?2 AND provider_turn.provider_turn_id = ?3
      AND run.status = 'running' AND attempt.status = 'running' AND provider_turn.status = ?6
      AND session.status = 'ready' AND session.driver = 'codex'
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND json_extract(provider_thread.payload_json,'$.nativeThreadRef.nativeId') = ?4
      AND json_extract(provider_turn.payload_json,'$.nativeTurnRef.nativeId') = ?5
";

const TARGET_RESPONSE: &str = "
    SELECT run.run_id, node.node_id, request.kind, session.provider_session_id,
           request.payload_json, node.payload_json, item.payload_json
    FROM orchestration_v2_projection_runtime_requests AS request
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = request.thread_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = request.node_id AND node.thread_id = thread.thread_id
    JOIN orchestration_v2_projection_runs AS run ON run.run_id = node.run_id AND run.thread_id = thread.thread_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId') AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_turns AS provider_turn
      ON provider_turn.provider_turn_id = request.provider_turn_id AND provider_turn.run_attempt_id = attempt.attempt_id
    JOIN orchestration_v2_projection_turn_items AS item
      ON item.node_id = node.node_id AND item.thread_id = thread.thread_id
        AND json_extract(item.payload_json, '$.requestId') = request.runtime_request_id
    JOIN orchestration_v2_projection_provider_session_bindings AS binding ON binding.thread_id = thread.thread_id
      AND binding.provider_session_id = json_extract(request.payload_json, '$.responseCapability.providerSessionId')
    JOIN orchestration_v2_projection_provider_sessions AS session ON session.provider_session_id = binding.provider_session_id
    WHERE thread.thread_id = ?1 AND request.runtime_request_id = ?2 AND request.status = 'pending'
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND json_extract(request.payload_json, '$.responseCapability.type') = 'live'
      AND run.status = 'running' AND attempt.status = 'running' AND provider_turn.status = 'running'
      AND session.status = 'ready' AND session.driver = 'codex'
      AND node.status = 'waiting' AND item.status = 'waiting'
";
