use rusqlite::{Transaction, params};
use serde_json::{Value, json};

use super::super::tests::{Fixture, THREAD};
use super::super::{
    PreparingDispatch, ThreadEventStream, TurnIds, append_preparing_dispatch, root_checkpoint_scope,
};

const AT: &str = "2026-07-17T10:35:09.000Z";

#[test]
fn queueing_before_the_start_claim_preserves_the_first_runs_workspace() {
    let fixture = Fixture::new();
    let worktree = fixture.directory.join("linked-worktree");
    std::fs::create_dir(&worktree).unwrap();
    let mut connection = fixture.connection();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "UPDATE orchestration_v2_projection_threads
         SET payload_json = json_set(payload_json, '$.worktreePath', ?2) WHERE thread_id = ?1",
            params![THREAD, worktree.to_str().unwrap()],
        )
        .unwrap();
    let first_message = "message:first";
    let first = TurnIds::derive(THREAD, first_message);
    append_preparing_dispatch(
        &transaction,
        PreparingDispatch {
            command_id: "launch",
            thread_id: THREAD,
            message_id: first_message,
            text: "First prompt",
            model_selection: json!({"instanceId":"codex","model":"gpt-5.5"}),
            workspace_strategy: &json!({"type":"worktree","baseRef":"main"}),
            now: AT,
        },
    )
    .unwrap();
    let stream = ThreadEventStream::command(&transaction, THREAD, "release");
    let metadata =
        json!({"runId":first.run_id,"nodeId":first.root_node_id,"providerInstanceId":"codex"});
    let mut node = payload(
        &transaction,
        "SELECT payload_json FROM orchestration_v2_projection_nodes WHERE node_id = ?1",
        &first.root_node_id,
    );
    let mut run = payload(
        &transaction,
        "SELECT payload_json FROM orchestration_v2_projection_runs WHERE run_id = ?1",
        &first.run_id,
    );
    node["checkpointScopeId"] = json!(first.checkpoint_scope_id);
    run["status"] = json!("starting");
    stream
        .append(
            "checkpoint-scope.created",
            AT,
            &metadata,
            &root_checkpoint_scope(&first, THREAD, &worktree, AT),
        )
        .unwrap();
    stream.append("node.updated", AT, &metadata, &node).unwrap();
    let sequence = stream.append("run.updated", AT, &metadata, &run).unwrap();
    let start_id = first.start_effect_id("release");
    transaction.execute(
        "INSERT INTO orchestration_v2_effect_outbox (effect_id, command_id, thread_id, effect_type,
            payload_json, status, attempt_count, available_at, created_at, updated_at)
         VALUES (?1, 'release', ?2, 'provider-turn.start', ?3, 'pending', 0, ?4, ?4, ?4)",
        params![start_id, THREAD, json!({"type":"provider-turn.start","runId":first.run_id}).to_string(), AT],
    ).unwrap();
    transaction
        .execute(
            "INSERT INTO orchestration_command_receipts (command_id, aggregate_kind, aggregate_id,
             accepted_at, result_sequence, status, command_type)
         VALUES ('release', 'thread', ?1, ?2, ?3, 'accepted', 'prepared-run.release')",
            params![THREAD, AT, sequence],
        )
        .unwrap();
    transaction.commit().unwrap();
    let followup = json!({"type":"message.dispatch","commandId":"queue","threadId":THREAD,
        "messageId":"message:next","createdBy":"user","creationSource":"web","text":"Next prompt",
        "attachments":[],"dispatchMode":{"type":"queue_after_active"}});
    let commands = fixture.commands();
    let queued = commands
        .dispatch(serde_json::from_value(followup.clone()).unwrap())
        .unwrap();
    let replay = commands
        .dispatch(serde_json::from_value(followup).unwrap())
        .unwrap();
    assert!(!replay.committed);
    assert_eq!(queued.sequence, replay.sequence);
    let scope_owner: String = connection
        .query_row(
            "SELECT run_id FROM orchestration_v2_projection_checkpoint_scopes WHERE scope_id = ?1",
            [&first.checkpoint_scope_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(scope_owner, queued.run_id);
    assert_eq!(commands.promote_queued_worktree(THREAD).unwrap(), None);
    let claim = commands
        .claim_start(&start_id, "still-starting")
        .unwrap()
        .unwrap();
    assert_eq!(claim.run_id, first.run_id);
    assert_eq!(claim.turn.cwd, worktree);
}

fn payload(transaction: &Transaction<'_>, query: &str, id: &str) -> Value {
    let encoded: String = transaction
        .query_row(query, [id], |row| row.get(0))
        .unwrap();
    serde_json::from_str(&encoded).unwrap()
}
