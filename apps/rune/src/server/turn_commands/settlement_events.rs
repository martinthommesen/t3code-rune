use serde_json::{Map, Value, json};

const CHECKPOINT_CAPTURE: &str = "checkpoint.capture";

pub(super) struct CompletedRun<'a> {
    pub(super) thread_id: &'a str,
    pub(super) run_id: &'a str,
    pub(super) root_node_id: &'a str,
    pub(super) provider_instance_id: &'a str,
    pub(super) checkpoint_scope_id: &'a str,
    pub(super) completed_at: &'a str,
    pub(super) run: Map<String, Value>,
    pub(super) attempt: Map<String, Value>,
    pub(super) root_node: Map<String, Value>,
    pub(super) provider_thread: Map<String, Value>,
}

pub(super) struct SettlementEvent {
    pub(super) event_type: &'static str,
    pub(super) metadata: Value,
    pub(super) payload: Value,
}

pub(super) struct CheckpointEffect {
    pub(super) id: String,
    pub(super) command_id: String,
    pub(super) thread_id: String,
    pub(super) effect_type: &'static str,
    pub(super) request: Value,
}

pub(super) struct CheckpointWaiting {
    pub(super) events: [SettlementEvent; 4],
    pub(super) effect: CheckpointEffect,
}

pub(super) fn checkpoint_waiting(input: CompletedRun<'_>) -> CheckpointWaiting {
    let CompletedRun {
        thread_id,
        run_id,
        root_node_id,
        provider_instance_id,
        checkpoint_scope_id,
        completed_at,
        mut run,
        mut attempt,
        mut root_node,
        mut provider_thread,
    } = input;
    attempt.insert("status".into(), json!("completed"));
    attempt.insert("completedAt".into(), json!(completed_at));
    run.remove("delegatedCompletion");
    run.insert("status".into(), json!("waiting"));
    run.insert("completedAt".into(), Value::Null);
    root_node.insert("status".into(), json!("waiting"));
    root_node.insert("completedAt".into(), Value::Null);
    root_node.insert("checkpointScopeId".into(), json!(checkpoint_scope_id));
    provider_thread.insert("status".into(), json!("idle"));
    provider_thread.insert("updatedAt".into(), json!(completed_at));

    let run_metadata = json!({
        "runId": run_id,
        "nodeId": root_node_id,
        "providerInstanceId": provider_instance_id,
    });
    CheckpointWaiting {
        events: [
            SettlementEvent {
                event_type: "run-attempt.updated",
                metadata: run_metadata.clone(),
                payload: Value::Object(attempt),
            },
            SettlementEvent {
                event_type: "run.updated",
                metadata: run_metadata.clone(),
                payload: Value::Object(run),
            },
            SettlementEvent {
                event_type: "node.updated",
                metadata: run_metadata,
                payload: Value::Object(root_node),
            },
            SettlementEvent {
                event_type: "provider-thread.updated",
                metadata: json!({ "providerInstanceId": provider_instance_id }),
                payload: Value::Object(provider_thread),
            },
        ],
        effect: CheckpointEffect {
            id: format!("effect:{CHECKPOINT_CAPTURE}:{run_id}"),
            command_id: format!("command:effect:{CHECKPOINT_CAPTURE}:{run_id}"),
            thread_id: thread_id.to_owned(),
            effect_type: CHECKPOINT_CAPTURE,
            request: json!({
                "type": CHECKPOINT_CAPTURE,
                "runId": run_id,
                "scopeId": checkpoint_scope_id,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_provider_run_waits_for_checkpoint_capture() {
        let now = "2026-09-25T00:16:49.800Z";
        let pending = checkpoint_waiting(CompletedRun {
            thread_id: "app-thread",
            run_id: "run:thread:app-thread:ordinal:1",
            root_node_id: "node:run:run%3Athread%3Aapp-thread%3Aordinal%3A1:root",
            provider_instance_id: "codex",
            checkpoint_scope_id: "checkpoint-scope:thread:app-thread:name:root",
            completed_at: now,
            run: Map::from_iter([("delegatedCompletion".into(), json!({}))]),
            attempt: Map::from_iter([("providerTurnId".into(), Value::Null)]),
            root_node: Map::from_iter([("providerTurnId".into(), Value::Null)]),
            provider_thread: Map::from_iter([("pendingBackgroundTasks".into(), json!([]))]),
        });
        let [attempt, run, node, provider_thread] = pending.events;
        assert_eq!(attempt.event_type, "run-attempt.updated");
        assert_eq!(run.event_type, "run.updated");
        assert_eq!(node.event_type, "node.updated");
        assert_eq!(provider_thread.event_type, "provider-thread.updated");
        assert_eq!(attempt.metadata, run.metadata);
        assert_eq!(attempt.metadata, node.metadata);
        assert_eq!(
            provider_thread.metadata,
            json!({"providerInstanceId": "codex"})
        );
        assert_eq!(
            attempt.payload,
            json!({
                "status": "completed", "completedAt": now, "providerTurnId": null,
            })
        );
        assert_eq!(
            run.payload,
            json!({"status": "waiting", "completedAt": null})
        );
        assert_eq!(
            node.payload,
            json!({
                "status": "waiting", "completedAt": null, "providerTurnId": null,
                "checkpointScopeId": "checkpoint-scope:thread:app-thread:name:root",
            })
        );
        assert_eq!(
            provider_thread.payload,
            json!({
                "status": "idle", "updatedAt": now, "pendingBackgroundTasks": [],
            })
        );
        assert_eq!(
            pending.effect.id,
            "effect:checkpoint.capture:run:thread:app-thread:ordinal:1"
        );
        assert_eq!(
            pending.effect.command_id,
            "command:effect:checkpoint.capture:run:thread:app-thread:ordinal:1"
        );
        assert_eq!(pending.effect.thread_id, "app-thread");
        assert_eq!(pending.effect.effect_type, "checkpoint.capture");
        assert_eq!(
            pending.effect.request,
            json!({
                "type": "checkpoint.capture",
                "runId": "run:thread:app-thread:ordinal:1",
                "scopeId": "checkpoint-scope:thread:app-thread:name:root",
            })
        );
    }
}
