use serde_json::{Value, json};
use t3_rune::providers::codex::AssistantPhase;

use super::{TurnIds, encode_id_part};

pub(super) struct NativeEvent {
    pub(super) event_type: &'static str,
    pub(super) node_id: String,
    pub(super) payload: Value,
}

pub(in crate::server) enum AssistantSnapshot<'a> {
    Streaming { visible_text: &'a str },
    Completed { text: &'a str },
}

pub(super) struct AssistantText<'a> {
    pub(super) native_turn_id: &'a str,
    pub(super) native_item_id: &'a str,
    pub(super) ordinal: i64,
    pub(super) started_at: &'a str,
    pub(super) updated_at: &'a str,
    pub(super) phase: Option<AssistantPhase>,
    pub(super) snapshot: AssistantSnapshot<'a>,
}

pub(super) fn assistant_text(
    turn: &TurnIds,
    thread_id: &str,
    item: AssistantText<'_>,
) -> Vec<NativeEvent> {
    let native_item_part = encode_id_part(item.native_item_id);
    let provider_turn_id = format!(
        "provider-turn:provider:codex:native-turn:{}",
        encode_id_part(item.native_turn_id)
    );
    let node_id = format!("node:provider:codex:native-item:{native_item_part}");
    let message_id = format!("message:provider:codex:native-item:{native_item_part}");
    let turn_item_id = format!("turn-item:provider:codex:native-item:{native_item_part}");
    let native_ref = json!({
        "driver": "codex",
        "nativeId": item.native_item_id,
        "strength": "strong",
    });
    let (text, status, completed_at) = match item.snapshot {
        AssistantSnapshot::Streaming { visible_text } => (visible_text, "running", None),
        AssistantSnapshot::Completed { text } => (text, "completed", Some(item.updated_at)),
    };
    let mut turn_item = json!({
        "id": turn_item_id,
        "threadId": thread_id,
        "runId": turn.run_id,
        "nodeId": node_id,
        "providerThreadId": turn.provider_thread_id,
        "providerTurnId": provider_turn_id,
        "nativeItemRef": native_ref,
        "parentItemId": null,
        "ordinal": item.ordinal,
        "status": status,
        "title": null,
        "startedAt": item.started_at,
        "completedAt": completed_at,
        "updatedAt": item.updated_at,
        "type": "assistant_message",
        "messageId": message_id,
        "text": text,
        "streaming": completed_at.is_none(),
    });
    if let Some(phase) = item.phase {
        turn_item["phase"] = json!(phase);
    }

    let mut events = Vec::new();
    if let Some(completed_at) = completed_at {
        events.push(NativeEvent {
            event_type: "node.updated",
            node_id: node_id.clone(),
            payload: json!({
                "id": node_id,
                "threadId": thread_id,
                "runId": turn.run_id,
                "parentNodeId": turn.root_node_id,
                "rootNodeId": turn.root_node_id,
                "kind": "assistant_message",
                "status": "completed",
                "countsForRun": false,
                "providerThreadId": turn.provider_thread_id,
                "providerTurnId": provider_turn_id,
                "nativeItemRef": native_ref,
                "runtimeRequestId": null,
                "checkpointScopeId": null,
                "startedAt": item.started_at,
                "completedAt": completed_at,
            }),
        });
        events.push(NativeEvent {
            event_type: "message.updated",
            node_id: node_id.clone(),
            payload: json!({
                "createdBy": "agent",
                "creationSource": "provider",
                "id": message_id,
                "threadId": thread_id,
                "runId": turn.run_id,
                "nodeId": node_id,
                "role": "assistant",
                "text": text,
                "attachments": [],
                "streaming": false,
                "createdAt": item.started_at,
                "updatedAt": item.updated_at,
            }),
        });
    }
    events.push(NativeEvent {
        event_type: "turn-item.updated",
        node_id,
        payload: turn_item,
    });
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_CODEX: &str = include_str!(
        "../../../../server/src/orchestration-v2/testkit/fixtures/simple/codex_transcript.ndjson"
    );

    #[test]
    fn completes_canonical_assistant_records_from_native_reply() {
        let completion = SIMPLE_CODEX
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid fixture record"))
            .map(|record| record["frame"]["params"].clone())
            .find(|params| {
                params["item"]["type"] == "agentMessage" && params.get("completedAtMs").is_some()
            })
            .expect("native assistant completion");
        let item = &completion["item"];
        let turn = TurnIds::derive("app-thread", "user-message");
        let events = assistant_text(
            &turn,
            "app-thread",
            AssistantText {
                native_turn_id: completion["turnId"].as_str().unwrap(),
                native_item_id: item["id"].as_str().unwrap(),
                ordinal: 1_000_002,
                started_at: "2026-09-25T00:16:49.066Z",
                updated_at: "2026-09-25T00:16:49.182Z",
                phase: serde_json::from_value(item["phase"].clone()).unwrap(),
                snapshot: AssistantSnapshot::Completed {
                    text: item["text"].as_str().unwrap(),
                },
            },
        );
        let [node, message, item] = events.as_slice() else {
            panic!("expected the completed node, message, and turn item");
        };
        let node_id = "node:provider:codex:native-item:msg_08fda0295ef7a774016ab5bd70c40887d080d27158f71dce50";
        let message_id = "message:provider:codex:native-item:msg_08fda0295ef7a774016ab5bd70c40887d080d27158f71dce50";
        let item_id = "turn-item:provider:codex:native-item:msg_08fda0295ef7a774016ab5bd70c40887d080d27158f71dce50";
        let provider_turn_id =
            "provider-turn:provider:codex:native-turn:01a0d5eb-f176-71d0-8e1c-a459700443de";
        assert_eq!(node.event_type, "node.updated");
        assert_eq!(message.event_type, "message.updated");
        assert_eq!(item.event_type, "turn-item.updated");
        assert!(events.iter().all(|event| event.node_id == node_id));
        assert_eq!(node.payload["id"], node_id);
        assert_eq!(node.payload["kind"], "assistant_message");
        assert_eq!(node.payload["parentNodeId"], turn.root_node_id.as_str());
        assert_eq!(node.payload["rootNodeId"], turn.root_node_id.as_str());
        assert_eq!(node.payload["providerTurnId"], provider_turn_id);
        assert_eq!(node.payload["nativeItemRef"]["strength"], "strong");
        assert_eq!(node.payload["status"], "completed");
        assert_eq!(node.payload["countsForRun"], false);
        assert_eq!(message.payload["id"], message_id);
        assert_eq!(message.payload["nodeId"], node_id);
        assert_eq!(message.payload["createdBy"], "agent");
        assert_eq!(message.payload["creationSource"], "provider");
        assert_eq!(message.payload["role"], "assistant");
        assert_eq!(message.payload["attachments"], json!([]));
        assert_eq!(message.payload["streaming"], false);
        assert_eq!(item.payload["id"], item_id);
        assert_eq!(
            item.payload["providerThreadId"],
            turn.provider_thread_id.as_str()
        );
        assert_eq!(item.payload["ordinal"], 1_000_002);
        assert_eq!(item.payload["phase"], "final_answer");
        assert_eq!(item.payload["messageId"], message_id);
        assert_eq!(item.payload["text"], "fixture simple ok");
        assert_eq!(item.payload["text"], message.payload["text"]);
        assert_eq!(item.payload["completedAt"], message.payload["updatedAt"]);
        assert_eq!(item.payload["startedAt"], message.payload["createdAt"]);
    }

    #[test]
    fn streams_only_the_visible_turn_item_with_stable_native_ids() {
        let turn = TurnIds::derive("app-thread", "user-message");
        let events = assistant_text(
            &turn,
            "app-thread",
            AssistantText {
                native_turn_id: "turn:1",
                native_item_id: "item:1/app",
                ordinal: 1_000_002,
                started_at: "2026-09-25T00:16:49.066Z",
                updated_at: "2026-09-25T00:16:49.100Z",
                phase: None,
                snapshot: AssistantSnapshot::Streaming {
                    visible_text: "A complete paragraph.\n\n",
                },
            },
        );
        let [item] = events.as_slice() else {
            panic!("streaming exposes only the turn item");
        };
        assert_eq!(item.event_type, "turn-item.updated");
        assert_eq!(
            item.node_id,
            "node:provider:codex:native-item:item%3A1%2Fapp"
        );
        assert_eq!(
            item.payload["id"],
            "turn-item:provider:codex:native-item:item%3A1%2Fapp"
        );
        assert_eq!(
            item.payload["providerTurnId"],
            "provider-turn:provider:codex:native-turn:turn%3A1"
        );
        assert_eq!(item.payload["nativeItemRef"]["nativeId"], "item:1/app");
        assert_eq!(item.payload["text"], "A complete paragraph.\n\n");
        assert_eq!(item.payload["status"], "running");
        assert_eq!(item.payload["streaming"], true);
        assert_eq!(item.payload["completedAt"], Value::Null);
        assert!(item.payload.get("phase").is_none());
    }
}
