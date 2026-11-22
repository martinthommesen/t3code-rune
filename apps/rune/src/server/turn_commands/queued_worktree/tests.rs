use rusqlite::{Transaction, params};
use serde_json::{Value, json};

use super::super::tests::{Fixture, THREAD};
use super::super::{
    PreparingDispatch, ThreadEventStream, TurnIds, append_preparing_dispatch, root_checkpoint_scope,
};
use super::ResumeOutcome;

const AT: &str = "2026-07-17T10:35:09.000Z";

#[test]
fn capability_uses_the_same_workspace_and_hold_checks_as_queue_intake() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    assert!(!commands.can_queue_worktree(THREAD).unwrap());
    let mut database = fixture.connection();
    let tx = database.transaction().unwrap();
    let (_, first_run) = append_preparing_dispatch(
        &tx,
        PreparingDispatch {
            provider: crate::server::turn_commands::NativeProvider::Codex,
            command_id: "launch",
            thread_id: THREAD,
            message_id: "message:first",
            text: "First prompt",
            model_selection: json!({"instanceId":"codex","model":"gpt-5.5"}),
            workspace_strategy: &json!({"type":"worktree","baseRef":"main"}),
            now: AT,
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let count = || {
        database
            .query_row("SELECT count(*) FROM orchestration_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    let before = count();
    assert!(commands.can_queue_worktree(THREAD).unwrap());
    assert_eq!(count(), before);
    let queued = commands.dispatch(serde_json::from_value(json!({
        "type":"message.dispatch","commandId":"queue","threadId":THREAD,"messageId":"message:next",
        "createdBy":"user","creationSource":"web","text":"Follow-up","attachments":[],
        "dispatchMode":{"type":"queue_after_active"}
    })).unwrap()).unwrap();
    database.execute("UPDATE orchestration_v2_projection_runs
        SET status = 'starting', payload_json = json_set(payload_json, '$.status', 'starting') WHERE run_id = ?1",
        [&first_run]).unwrap();
    assert!(!commands.can_queue_worktree(THREAD).unwrap());
    let workspace = fixture.directory.join("new-worktree");
    database
        .execute(
            "UPDATE orchestration_v2_projection_threads
        SET payload_json = json_set(payload_json, '$.worktreePath', ?2) WHERE thread_id = ?1",
            params![THREAD, workspace.to_str().unwrap()],
        )
        .unwrap();
    assert!(!commands.can_queue_worktree(THREAD).unwrap());
    std::fs::create_dir(workspace).unwrap();
    assert!(commands.can_queue_worktree(THREAD).unwrap());
    database
        .execute(
            "UPDATE orchestration_v2_projection_runs
        SET payload_json = json_set(payload_json, '$.queueHeld', json('true')) WHERE run_id = ?1",
            [&queued.run_id],
        )
        .unwrap();
    assert!(!commands.can_queue_worktree(THREAD).unwrap());
}

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
            provider: crate::server::turn_commands::NativeProvider::Codex,
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

    let stop = commands
        .stop_thread(
            serde_json::from_value(json!({
                "type":"thread.stop", "commandId":"stop", "threadId":THREAD,
            }))
            .unwrap(),
        )
        .unwrap();
    let held_at: i64 = connection
        .query_row(
            "SELECT sequence FROM orchestration_events WHERE command_id = 'stop'
         AND json_type(payload_json, '$.queueHeld') = 'true'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stop.sequence, held_at);
    let ResumeOutcome::Accepted {
        sequence,
        committed: true,
    } = commands.resume_worktree_queue(THREAD, "resume").unwrap()
    else {
        panic!("the attached worktree queue should resume");
    };
    let ResumeOutcome::Accepted {
        sequence: replay,
        committed: false,
    } = commands.resume_worktree_queue(THREAD, "resume").unwrap()
    else {
        panic!("the resume receipt should replay");
    };
    assert_eq!(sequence, replay);
    let successor_effect: String = connection
        .query_row(
            "SELECT effect_id FROM orchestration_v2_effect_outbox
         WHERE thread_id = ?1 AND effect_type = 'provider-turn.start' AND status = 'pending'",
            [THREAD],
            |row| row.get(0),
        )
        .unwrap();
    let successor = commands
        .claim_start(&successor_effect, "resuming")
        .unwrap()
        .unwrap();
    assert_eq!(successor.run_id, queued.run_id);
    assert_eq!(successor.turn.cwd, worktree);
    assert!(successor.native_thread_id.is_none());
    assert!(matches!(
        commands.record_session_ready(&successor, true).unwrap(),
        Some(super::super::provider_session::SessionAttachment::Attached(
            _
        ))
    ));
}

#[test]
fn stopping_preparation_holds_its_queue_and_rejects_resume_without_a_workspace() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let mut database = fixture.connection();
    let transaction = database.transaction().unwrap();
    append_preparing_dispatch(
        &transaction,
        PreparingDispatch {
            provider: crate::server::turn_commands::NativeProvider::Codex,
            command_id: "launch",
            thread_id: THREAD,
            message_id: "message:first",
            text: "First prompt",
            model_selection: json!({"instanceId":"codex","model":"gpt-5.5"}),
            workspace_strategy: &json!({"type":"worktree","baseRef":"main"}),
            now: AT,
        },
    )
    .unwrap();
    transaction.commit().unwrap();
    let followup = json!({"type":"message.dispatch","commandId":"queue","threadId":THREAD,
        "messageId":"message:next","createdBy":"user","creationSource":"web","text":"Next prompt",
        "attachments":[],"dispatchMode":{"type":"queue_after_active"}});
    let queued = commands
        .dispatch(serde_json::from_value(followup).unwrap())
        .unwrap();
    let stop = commands
        .stop_thread(
            serde_json::from_value(json!({
                "type":"thread.stop","commandId":"stop","threadId":THREAD,
            }))
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        commands.resume_worktree_queue(THREAD, "resume").unwrap(),
        ResumeOutcome::Unresolved
    ));
    assert!(matches!(
        commands.resume_worktree_queue(THREAD, "resume").unwrap(),
        ResumeOutcome::PreviouslyRejected(_)
    ));
    let (status, held): (String,bool) = database.query_row(
        "SELECT status, json_extract(payload_json, '$.queueHeld') FROM orchestration_v2_projection_runs WHERE run_id = ?1",
        [&queued.run_id],|row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(status, "queued");
    assert!(held);
    let (receipt, sequence): (String,i64)=database.query_row(
        "SELECT status,result_sequence FROM orchestration_command_receipts WHERE command_id = 'resume'",
        [],|row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(receipt, "rejected");
    assert_eq!(sequence, stop.sequence);
    let effects: i64 = database
        .query_row(
            "SELECT count(*) FROM orchestration_v2_effect_outbox",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(effects, 0);
}

#[test]
fn selecting_a_later_held_prompt_starts_only_that_prompt() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let workspace = fixture.directory.join("linked-worktree");
    std::fs::create_dir(&workspace).unwrap();
    let mut database = fixture.connection();
    let setup = database.transaction().unwrap();
    setup.execute(
        "UPDATE orchestration_v2_projection_threads SET payload_json = json_set(payload_json, '$.worktreePath', ?2)
         WHERE thread_id = ?1", params![THREAD, workspace.to_str().unwrap()],
    ).unwrap();
    append_preparing_dispatch(
        &setup,
        PreparingDispatch {
            provider: crate::server::turn_commands::NativeProvider::Codex,
            command_id: "launch",
            thread_id: THREAD,
            message_id: "message:first",
            text: "First prompt",
            model_selection: json!({"instanceId":"codex","model":"gpt-5.5"}),
            workspace_strategy: &json!({"type":"worktree","baseRef":"main"}),
            now: AT,
        },
    )
    .unwrap();
    setup.commit().unwrap();
    let enqueue = |command: &str, text: &str| {
        commands.dispatch(serde_json::from_value(json!({
            "type":"message.dispatch", "commandId":command, "threadId":THREAD, "messageId":format!("message:{command}"),
            "createdBy":"user", "creationSource":"web", "text":text, "attachments":[],
            "dispatchMode":{"type":"queue_after_active"},
        })).unwrap()).unwrap().run_id
    };
    let earlier = enqueue("earlier", "Wait in queue");
    let selected = enqueue("selected", "Start this prompt");
    assert!(matches!(
        commands
            .start_queued_worktree_run(THREAD, "too-early", &selected)
            .unwrap(),
        ResumeOutcome::InvalidSelection
    ));
    let stopped = commands
        .stop_thread(
            serde_json::from_value(json!({
                "type":"thread.stop", "commandId":"stop", "threadId":THREAD,
            }))
            .unwrap(),
        )
        .unwrap();
    let ResumeOutcome::Accepted {
        sequence,
        committed: true,
    } = commands
        .start_queued_worktree_run(THREAD, "selected-now", &selected)
        .unwrap()
    else {
        panic!("the selected run should start");
    };
    assert_eq!(sequence, stopped.sequence + 2);
    assert!(matches!(
        commands.start_queued_worktree_run(THREAD, "selected-now", &selected).unwrap(),
        ResumeOutcome::Accepted { sequence: replay, committed: false } if replay == sequence
    ));
    let root_status = |id: &str| -> (String, i64, bool) {
        database
            .query_row(
                "SELECT status, ordinal, json_extract(payload_json, '$.queueHeld')
             FROM orchestration_v2_projection_runs WHERE run_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    };
    assert_eq!(root_status(&earlier), ("queued".to_owned(), 2, false));
    assert_eq!(root_status(&selected), ("starting".to_owned(), 3, false));
    let effect: String = database
        .query_row(
            "SELECT effect_id FROM orchestration_v2_effect_outbox WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let claimed = commands
        .claim_start(&effect, "selected-worker")
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run_id, selected);
    assert_eq!(claimed.turn.text, "Start this prompt");
    assert_eq!(claimed.turn.cwd, workspace);
}

fn payload(transaction: &Transaction<'_>, query: &str, id: &str) -> Value {
    let encoded: String = transaction
        .query_row(query, [id], |row| row.get(0))
        .unwrap();
    serde_json::from_str(&encoded).unwrap()
}
