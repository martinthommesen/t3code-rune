use serde::Serialize;
use serde_json::{Map, Value, json};

use super::encode_id_part;
use super::settlement_events::SettlementEvent;

pub(super) struct RootCheckpoint<'a> {
    scope_id: &'a str,
    driver: &'a str,
    run_ordinal: i64,
    checkpoint_ref: &'a str,
    parent_checkpoint_id: String,
    checkpoint_id: String,
    turn_item_id: String,
}

impl<'a> RootCheckpoint<'a> {
    pub(super) fn new(
        scope_id: &'a str,
        driver: &'a str,
        run_ordinal: i64,
        checkpoint_ref: &'a str,
    ) -> Self {
        let parent_checkpoint_id = scope_checkpoint_id(scope_id, run_ordinal - 1);
        let checkpoint_id = scope_checkpoint_id(scope_id, run_ordinal);
        let turn_item_id = format!(
            "turn-item:provider:{}:native-item:{}",
            encode_id_part(driver),
            encode_id_part(&format!("checkpoint:{checkpoint_id}")),
        );
        Self {
            scope_id,
            driver,
            run_ordinal,
            checkpoint_ref,
            parent_checkpoint_id,
            checkpoint_id,
            turn_item_id,
        }
    }

    pub(super) fn turn_item_id(&self) -> &str {
        &self.turn_item_id
    }
}

fn scope_checkpoint_id(scope_id: &str, ordinal: i64) -> String {
    format!(
        "checkpoint:scope:{}:name:{ordinal}",
        encode_id_part(scope_id)
    )
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CheckpointStatus {
    Ready,
    Missing,
    Error,
}

pub(super) struct CheckpointBaseline<'a> {
    pub(super) ordinal: i64,
    pub(super) reference: &'a str,
    pub(super) status: CheckpointStatus,
}

pub(super) struct CheckpointInput<'a> {
    pub(super) thread_id: &'a str,
    pub(super) run_id: &'a str,
    pub(super) root_node_id: &'a str,
    pub(super) provider_instance_id: &'a str,
    pub(super) provider_thread_id: &'a str,
    pub(super) requested_at: &'a str,
    pub(super) captured_at: &'a str,
    pub(super) turn_item_ordinal: i64,
    pub(super) baselines: &'a [CheckpointBaseline<'a>],
    pub(super) status: CheckpointStatus,
    pub(super) files: Value,
    pub(super) run: Map<String, Value>,
    pub(super) root_node: Map<String, Value>,
}

pub(super) struct CheckpointEvents {
    pub(super) command_id: String,
    pub(super) events: Vec<SettlementEvent>,
}

pub(super) fn checkpoint_events(
    target: RootCheckpoint<'_>,
    input: CheckpointInput<'_>,
) -> CheckpointEvents {
    let RootCheckpoint {
        scope_id,
        driver,
        run_ordinal,
        checkpoint_ref,
        parent_checkpoint_id,
        checkpoint_id,
        turn_item_id,
    } = target;
    let CheckpointInput {
        thread_id,
        run_id,
        root_node_id,
        provider_instance_id,
        provider_thread_id,
        requested_at,
        captured_at,
        turn_item_ordinal,
        baselines,
        status,
        files,
        mut run,
        mut root_node,
    } = input;
    let stopped = matches!(
        run.get("status").and_then(Value::as_str),
        Some("interrupted" | "cancelled")
    );
    run.remove("delegatedCompletion");
    run.insert("checkpointId".into(), json!(checkpoint_id));
    if !stopped {
        run.insert("status".into(), json!("completed"));
        run.insert("completedAt".into(), json!(captured_at));
        root_node.insert("status".into(), json!("completed"));
        root_node.insert("completedAt".into(), json!(captured_at));
        root_node.insert("checkpointScopeId".into(), json!(scope_id));
    }

    let checkpoint_metadata = json!({
        "runId": run_id,
        "nodeId": root_node_id,
        "driver": driver,
        "providerInstanceId": provider_instance_id,
    });
    let run_metadata = json!({
        "runId": run_id,
        "nodeId": root_node_id,
        "providerInstanceId": provider_instance_id,
    });
    let mut events = Vec::with_capacity(4 + baselines.len());
    for baseline in baselines {
        events.push(SettlementEvent {
            event_type: "checkpoint.captured",
            metadata: json!({
                "nodeId": root_node_id,
                "driver": driver,
                "providerInstanceId": provider_instance_id,
            }),
            payload: json!({
                "id": scope_checkpoint_id(scope_id, baseline.ordinal),
                "threadId": thread_id,
                "scopeId": scope_id,
                "runId": null,
                "nodeId": root_node_id,
                "parentCheckpointId": null,
                "ordinalWithinScope": baseline.ordinal,
                "appRunOrdinal": null,
                "ref": baseline.reference,
                "status": baseline.status,
                "files": [],
                "capturedAt": requested_at,
            }),
        });
    }
    events.extend([
        SettlementEvent {
            event_type: "checkpoint.captured",
            metadata: checkpoint_metadata.clone(),
            payload: json!({
                "id": checkpoint_id,
                "threadId": thread_id,
                "scopeId": scope_id,
                "runId": run_id,
                "nodeId": root_node_id,
                "parentCheckpointId": parent_checkpoint_id,
                "ordinalWithinScope": run_ordinal,
                "appRunOrdinal": run_ordinal,
                "ref": checkpoint_ref,
                "status": status,
                "files": files,
                "capturedAt": captured_at,
            }),
        },
        SettlementEvent {
            event_type: "turn-item.updated",
            metadata: checkpoint_metadata,
            payload: json!({
                "id": turn_item_id,
                "threadId": thread_id,
                "runId": run_id,
                "nodeId": root_node_id,
                "providerThreadId": provider_thread_id,
                "providerTurnId": root_node.get("providerTurnId"),
                "nativeItemRef": null,
                "parentItemId": null,
                "ordinal": turn_item_ordinal,
                "status": "completed",
                "title": null,
                "startedAt": captured_at,
                "completedAt": captured_at,
                "updatedAt": captured_at,
                "type": "checkpoint",
                "checkpointId": checkpoint_id,
                "scopeId": scope_id,
                "files": files,
            }),
        },
        SettlementEvent {
            event_type: "run.updated",
            metadata: run_metadata.clone(),
            payload: Value::Object(run),
        },
    ]);
    if !stopped {
        events.push(SettlementEvent {
            event_type: "node.updated",
            metadata: run_metadata,
            payload: Value::Object(root_node),
        });
    }
    CheckpointEvents {
        command_id: format!("command:effect:checkpoint.capture:{run_id}"),
        events,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "checkpoint-scope:thread:app:name:root";
    const BASELINE_ID: &str =
        "checkpoint:scope:checkpoint-scope%3Athread%3Aapp%3Aname%3Aroot:name:0";
    const CHECKPOINT_ID: &str =
        "checkpoint:scope:checkpoint-scope%3Athread%3Aapp%3Aname%3Aroot:name:1";
    const ITEM_ID: &str = "turn-item:provider:codex:native-item:checkpoint%3Acheckpoint%3Ascope%3Acheckpoint-scope%253Athread%253Aapp%253Aname%253Aroot%3Aname%3A1";
    const REQUESTED_AT: &str = "2026-09-25T00:16:44.100Z";
    const CAPTURED_AT: &str = "2026-09-25T00:16:49.900Z";

    fn capture(
        run_ordinal: i64,
        baselines: &[CheckpointBaseline<'_>],
        run_status: &str,
    ) -> CheckpointEvents {
        let reference = format!("refs/test/{run_ordinal}");
        let run_id = format!("run:thread:app:ordinal:{run_ordinal}");
        let node_id = format!("node:run:run%3Athread%3Aapp%3Aordinal%3A{run_ordinal}:root");
        let target = RootCheckpoint::new(SCOPE, "codex", run_ordinal, &reference);
        checkpoint_events(
            target,
            CheckpointInput {
                thread_id: "app",
                run_id: &run_id,
                root_node_id: &node_id,
                provider_instance_id: "codex",
                provider_thread_id: "local-provider-thread",
                requested_at: REQUESTED_AT,
                captured_at: CAPTURED_AT,
                turn_item_ordinal: run_ordinal * 1_000_000 + 3,
                baselines,
                status: CheckpointStatus::Missing,
                files: json!([]),
                run: Map::from_iter([
                    ("delegatedCompletion".into(), json!({})),
                    ("status".into(), json!(run_status)),
                    ("completedAt".into(), json!(REQUESTED_AT)),
                ]),
                root_node: Map::from_iter([("providerTurnId".into(), Value::Null)]),
            },
        )
    }

    #[test]
    fn missing_checkpoints_complete_the_run_with_a_linked_baseline() {
        let result = capture(
            1,
            &[CheckpointBaseline {
                ordinal: 0,
                reference: "refs/test/0",
                status: CheckpointStatus::Missing,
            }],
            "waiting",
        );
        assert_eq!(
            result.command_id,
            "command:effect:checkpoint.capture:run:thread:app:ordinal:1"
        );
        let [baseline, checkpoint, item, run, node] = result.events.as_slice() else {
            panic!("expected the baseline, checkpoint, turn item, run, and node");
        };
        assert_eq!(baseline.event_type, "checkpoint.captured");
        assert_eq!(checkpoint.event_type, "checkpoint.captured");
        assert_eq!(item.event_type, "turn-item.updated");
        assert_eq!(run.event_type, "run.updated");
        assert_eq!(node.event_type, "node.updated");
        assert!(baseline.metadata.get("runId").is_none());
        assert_eq!(baseline.payload["id"], BASELINE_ID);
        assert_eq!(baseline.payload["capturedAt"], REQUESTED_AT);
        assert_eq!(checkpoint.payload["id"], CHECKPOINT_ID);
        assert_eq!(checkpoint.payload["parentCheckpointId"], BASELINE_ID);
        assert_eq!(checkpoint.payload["capturedAt"], CAPTURED_AT);
        for record in [baseline, checkpoint] {
            assert_eq!(record.payload["status"], "missing");
            assert_eq!(record.payload["files"], json!([]));
        }
        assert_eq!(item.payload["id"], ITEM_ID);
        assert_eq!(item.payload["checkpointId"], CHECKPOINT_ID);
        assert_eq!(item.payload["ordinal"], 1_000_003);
        assert_eq!(item.payload["providerTurnId"], Value::Null);
        assert_eq!(run.payload["status"], "completed");
        assert_eq!(run.payload["checkpointId"], CHECKPOINT_ID);
        assert!(run.payload.get("delegatedCompletion").is_none());
        assert_eq!(node.payload["status"], "completed");
        assert_eq!(node.payload["completedAt"], CAPTURED_AT);
    }

    #[test]
    fn capture_references_the_preserved_baseline_without_rewriting_it() {
        let result = capture(1, &[], "waiting");
        assert_eq!(result.events.len(), 4);
        assert_eq!(result.events[0].payload["id"], CHECKPOINT_ID);
        assert_eq!(result.events[0].payload["parentCheckpointId"], BASELINE_ID);
    }

    #[test]
    fn consecutive_run_links_the_existing_checkpoint_and_rematerializes_ordinal_zero() {
        let result = capture(
            2,
            &[CheckpointBaseline {
                ordinal: 0,
                reference: "refs/test/0",
                status: CheckpointStatus::Missing,
            }],
            "waiting",
        );
        let [initial, checkpoint, item, _, _] = result.events.as_slice() else {
            panic!("expected the rematerialized ordinal-zero baseline and the new run");
        };
        assert_eq!(initial.payload["id"], BASELINE_ID);
        assert_eq!(initial.payload["runId"], Value::Null);
        assert_eq!(checkpoint.payload["parentCheckpointId"], CHECKPOINT_ID);
        assert_eq!(checkpoint.payload["ordinalWithinScope"], 2);
        assert_eq!(checkpoint.payload["appRunOrdinal"], 2);
        assert_eq!(checkpoint.payload["ref"], "refs/test/2");
        assert_eq!(item.payload["ordinal"], 2_000_003);
        assert_eq!(item.payload["checkpointId"], checkpoint.payload["id"]);
    }

    #[test]
    fn consecutive_stopped_run_keeps_terminal_state_when_both_baselines_are_unpreserved() {
        let result = capture(
            3,
            &[
                CheckpointBaseline {
                    ordinal: 0,
                    reference: "refs/test/0",
                    status: CheckpointStatus::Missing,
                },
                CheckpointBaseline {
                    ordinal: 2,
                    reference: "refs/test/2",
                    status: CheckpointStatus::Ready,
                },
            ],
            "interrupted",
        );
        let [initial, previous, checkpoint, item, run] = result.events.as_slice() else {
            panic!(
                "expected both baselines, the new checkpoint, its item, and the unchanged terminal run"
            );
        };
        assert_eq!(initial.payload["ordinalWithinScope"], 0);
        assert_eq!(previous.payload["ordinalWithinScope"], 2);
        assert_eq!(previous.payload["appRunOrdinal"], Value::Null);
        assert_eq!(previous.payload["ref"], "refs/test/2");
        assert_eq!(previous.payload["status"], "ready");
        assert_eq!(
            checkpoint.payload["parentCheckpointId"],
            previous.payload["id"]
        );
        assert_eq!(checkpoint.payload["ordinalWithinScope"], 3);
        assert_eq!(item.payload["ordinal"], 3_000_003);
        assert_eq!(run.payload["status"], "interrupted");
        assert_eq!(run.payload["completedAt"], REQUESTED_AT);
        assert_eq!(run.payload["checkpointId"], checkpoint.payload["id"]);
    }
}
