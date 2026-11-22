use std::fs;
use std::sync::{Arc, Barrier};

use super::*;

const AT: &str = "2026-07-17T10:35:09.000Z";
pub(in crate::server) const COMMAND: &str = "dispatch:command";
pub(in crate::server) const THREAD: &str = "thread:project:work é";
const MESSAGE: &str = "message:with spaces";
const TEXT: &str = "  first line\nsecond line  ";

pub(in crate::server) struct Fixture {
    pub(in crate::server) directory: PathBuf,
    database: PathBuf,
}

impl Fixture {
    pub(in crate::server) fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("t3-turn-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let database = directory.join("statev2.sqlite");
        let fixture = Self {
            directory,
            database,
        };
        let connection = fixture.connection();
        connection
            .execute_batch(include_str!("../bootstrap/schema.sql"))
            .unwrap();
        connection
            .execute(
                "INSERT INTO projection_projects
                    (project_id, title, workspace_root, scripts_json, created_at, updated_at)
                 VALUES ('work', 'Work', ?1, '[]', ?2, ?2)",
                params![fixture.directory.to_str().unwrap(), AT],
            )
            .unwrap();
        let payload = json!({
            "createdBy": "user",
            "creationSource": "web",
            "id": THREAD,
            "projectId": "work",
            "title": "New thread",
            "providerInstanceId": "codex",
            "modelSelection": { "instanceId": "codex", "model": "gpt-5.5" },
            "runtimeMode": "approval-required",
            "interactionMode": "default",
            "branch": null,
            "worktreePath": null,
            "pullRequests": [],
            "activeProviderThreadId": null,
            "lineage": {
                "parentThreadId": null,
                "relationshipToParent": null,
                "rootThreadId": THREAD,
            },
            "forkedFrom": null,
            "createdAt": AT,
            "updatedAt": AT,
            "archivedAt": null,
            "settledOverride": null,
            "settledAt": null,
            "snoozedUntil": null,
            "snoozedAt": null,
            "lastVisitedAt": AT,
            "deletedAt": null,
        });
        connection
            .execute(
                "INSERT INTO orchestration_v2_projection_threads (
                    thread_id, project_id, title, default_provider, provider_instance_id,
                    runtime_mode, interaction_mode, created_at, updated_at, payload_json
                 ) VALUES (?1, 'work', 'New thread', 'codex', 'codex',
                           'approval-required', 'default', ?2, ?2, ?3)",
                params![THREAD, AT, payload.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO orchestration_events (
                    event_id, aggregate_kind, stream_id, stream_version, event_type,
                    occurred_at, actor_kind, payload_json, metadata_json, application_event_version
                 ) VALUES ('event:thread-created', 'thread', ?1, 0, 'thread.created',
                           ?2, 'server', ?3, '{}', 2)",
                params![THREAD, AT, payload.to_string()],
            )
            .unwrap();
        fixture
    }

    pub(in crate::server) fn commands(&self) -> TurnCommands {
        TurnCommands::new(self.database.clone())
    }

    pub(in crate::server) fn connection(&self) -> Connection {
        Connection::open(&self.database).unwrap()
    }

    fn counts(&self) -> (u64, u64, u64, u64) {
        self.connection()
            .query_row(
                "SELECT (SELECT count(*) FROM orchestration_events),
                        (SELECT count(*) FROM orchestration_command_receipts),
                        (SELECT count(*) FROM orchestration_v2_turn_item_positions),
                        (SELECT count(*) FROM orchestration_v2_effect_outbox)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

pub(in crate::server) fn command_payload() -> Value {
    json!({
        "type": "message.dispatch",
        "commandId": COMMAND,
        "threadId": THREAD,
        "messageId": MESSAGE,
        "createdBy": "user",
        "creationSource": "web",
        "text": TEXT,
        "attachments": [],
        "dispatchMode": { "type": "start_immediately" },
        "deliveryIntent": "auto",
    })
}

pub(in crate::server) fn command() -> MessageDispatch {
    serde_json::from_value(command_payload()).unwrap()
}

#[test]
fn stores_the_first_text_run_in_the_canonical_stream_and_projections() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let result = commands.dispatch(command()).unwrap();
    assert!(result.committed);
    assert_eq!(result.sequence, 8);
    assert_eq!(
        result.run_id,
        "run:thread:thread%3Aproject%3Awork%20%C3%A9:ordinal:1"
    );
    assert_eq!(fixture.counts(), (8, 1, 1, 1));

    let connection = fixture.connection();
    let mut query = connection
        .prepare(
            "SELECT stream_version, event_type FROM orchestration_events
             WHERE command_id = ?1 AND correlation_id = ?1 AND aggregate_kind = 'thread'
               AND actor_kind = 'server' AND application_event_version = 2
             ORDER BY sequence",
        )
        .unwrap();
    let events = query
        .query_map([COMMAND], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|(version, event)| (*version, event.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, "provider-thread.updated"),
            (2, "run.created"),
            (3, "run-attempt.created"),
            (4, "node.updated"),
            (5, "checkpoint-scope.created"),
            (6, "message.updated"),
            (7, "turn-item.updated"),
        ]
    );
    let (text, ordinal, intended_user, last_sequence, effect_payload) = connection
        .query_row(
            "SELECT json_extract(message.payload_json, '$.text'), item.ordinal,
                    json_extract(item.payload_json, '$.createdBy'), metadata.last_sequence,
                    outbox.payload_json
             FROM orchestration_v2_projection_messages AS message
             JOIN orchestration_v2_projection_turn_items AS item ON message.run_id = item.run_id
             JOIN orchestration_v2_projection_metadata AS metadata ON metadata.projection_name = 'thread-projections'
             JOIN orchestration_v2_effect_outbox AS outbox ON outbox.thread_id = message.thread_id
             WHERE outbox.status = 'pending' AND outbox.attempt_count = 0",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?, row.get::<_, String>(4)?)),
        )
        .unwrap();
    assert_eq!(text, TEXT);
    assert_eq!(ordinal, 1_000_001);
    assert_eq!(intended_user, "user");
    assert_eq!(last_sequence, result.sequence);
    assert_eq!(
        serde_json::from_str::<Value>(&effect_payload).unwrap(),
        json!({ "type": EFFECT_TYPE, "runId": result.run_id }),
    );
}

#[test]
fn initial_unvisited_dispatch_records_the_read_watermark_first() {
    let fixture = Fixture::new();
    let connection = fixture.connection();
    connection
        .execute(
            "UPDATE orchestration_v2_projection_threads
             SET payload_json = json_set(payload_json, '$.lastVisitedAt', NULL)",
            [],
        )
        .unwrap();
    let result = fixture.commands().dispatch(command()).unwrap();
    let (event_type, occurred_at, payload, actual_visited_at) = connection
        .query_row(
            "SELECT event_type, occurred_at, events.payload_json,
                    json_extract(thread.payload_json, '$.lastVisitedAt')
             FROM orchestration_events AS events
             JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = events.stream_id
             WHERE events.command_id = ?1 ORDER BY sequence LIMIT 1",
            [COMMAND],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
        )
        .unwrap();
    let occurred_at =
        OffsetDateTime::parse(&occurred_at, &time::format_description::well_known::Rfc3339)
            .unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(event_type, "thread.metadata-updated");
    assert_eq!(payload["updatedAt"], AT);
    assert_eq!(payload["lastVisitedAt"], actual_visited_at);
    assert_eq!(
        actual_visited_at,
        timestamp(occurred_at - time::Duration::milliseconds(1))
    );
    assert_eq!(result.sequence, 9);
    assert_eq!(fixture.counts(), (9, 1, 1, 1));
}

#[test]
fn retried_dispatch_returns_the_original_receipt_without_duplicate_effects() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let start = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            commands.dispatch(command()).unwrap()
        });
        let second = scope.spawn(|| {
            start.wait();
            commands.dispatch(command()).unwrap()
        });
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(first.sequence, second.sequence);
        assert_eq!(first.run_id, second.run_id);
        assert_ne!(first.committed, second.committed);
    });
    assert_eq!(fixture.counts(), (8, 1, 1, 1));
    let mut next = command();
    next.command_id = "second-command".to_owned();
    assert!(matches!(
        commands.dispatch(next),
        Err(TurnCommandError::Unsupported(_))
    ));
    assert_eq!(fixture.counts(), (8, 1, 1, 1));
}

#[test]
fn outbox_write_failure_leaves_no_receipt_or_partial_turn() {
    let fixture = Fixture::new();
    fixture
        .connection()
        .execute_batch(
            "CREATE TRIGGER outbox_test_failure BEFORE INSERT ON orchestration_v2_effect_outbox
             BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        fixture.commands().dispatch(command()),
        Err(TurnCommandError::Database(_)),
    ));
    assert_eq!(fixture.counts(), (1, 0, 0, 0));
    let active_provider_thread: Option<String> = fixture
        .connection()
        .query_row(
            "SELECT active_provider_thread_id FROM orchestration_v2_projection_threads
             WHERE thread_id = ?1",
            [THREAD],
            |row| row.get(0),
        )
        .unwrap();
    assert!(active_provider_thread.is_none());
}

#[test]
fn only_one_worker_can_claim_a_committed_provider_start() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let effect_id = TurnIds::derive(THREAD, MESSAGE).start_effect_id(COMMAND);
    assert!(
        commands
            .claim_start(&effect_id, "worker-before-dispatch")
            .unwrap()
            .is_none()
    );
    let dispatch = commands.dispatch(command()).unwrap();
    let start = Arc::new(Barrier::new(2));
    let claimed = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            commands.claim_start(&effect_id, "worker-1").unwrap()
        });
        let second = scope.spawn(|| {
            start.wait();
            commands.claim_start(&effect_id, "worker-2").unwrap()
        });
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_ne!(first.is_some(), second.is_some());
        first.or(second).unwrap()
    });
    assert_eq!(claimed.effect_id, effect_id);
    assert_eq!(claimed.run_id, dispatch.run_id);
    assert_eq!(claimed.thread.app_thread_id.as_deref(), Some(THREAD));
    assert_eq!(claimed.thread.cwd, fixture.directory);
    assert_eq!(claimed.thread.model, "gpt-5.5");
    assert_eq!(claimed.turn.text, TEXT);
    assert_eq!(claimed.turn.cwd, fixture.directory);
    assert_eq!(claimed.turn.model, "gpt-5.5");
    assert_eq!(claimed.turn.runtime_mode, RuntimeMode::ApprovalRequired);
    assert_eq!(claimed.turn.interaction_mode, InteractionMode::Default);
    assert_eq!(claimed.turn.reasoning_effort, None);

    let (owner, attempts, lease_expires_at) = fixture
        .connection()
        .query_row(
            "SELECT lease_owner, attempt_count, lease_expires_at
             FROM orchestration_v2_effect_outbox WHERE effect_id = ?1 AND status = 'running'",
            [&effect_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .unwrap();
    assert!(matches!(owner.as_str(), "worker-1" | "worker-2"));
    assert_eq!(attempts, 1);
    assert!(lease_expires_at.is_none());
    assert!(
        fixture
            .commands()
            .claim_start(&effect_id, "worker-after-reopen")
            .unwrap()
            .is_none()
    );
}

#[test]
fn stores_assistant_text_only_for_the_bound_native_turn() {
    use super::native_events::AssistantSnapshot;
    use super::turn_text::AssistantUpdate;
    use t3_rune::providers::codex::{AssistantPhase, ThreadId, TurnId, TurnKey};
    use time::format_description::well_known::Rfc3339;

    let fixture = Fixture::new();
    let commands = fixture.commands();
    let dispatch = commands.dispatch(command()).unwrap();
    let ids = TurnIds::derive(THREAD, MESSAGE);
    let key = TurnKey {
        thread_id: ThreadId::new("native-thread"),
        turn_id: TurnId::new("native-turn"),
    };
    let first = OffsetDateTime::parse(AT, &Rfc3339).unwrap();
    let snapshot = |phase, snapshot, millis| AssistantUpdate {
        key: &key,
        native_item_id: "message:item",
        started_at: first,
        updated_at: first + time::Duration::milliseconds(millis),
        phase,
        snapshot,
    };
    let initial = || {
        snapshot(
            None,
            AssistantSnapshot::Streaming {
                visible_text: "First paragraph.",
            },
            1,
        )
    };
    assert!(
        commands
            .record_assistant(THREAD, &dispatch.run_id, initial())
            .unwrap()
            .is_none()
    );

    seed_bound_turn(&fixture, &dispatch.run_id, &ids, &key);
    let connection = fixture.connection();
    let wrong_key = TurnKey {
        turn_id: TurnId::new("unbound-native-turn"),
        thread_id: key.thread_id.clone(),
    };
    assert!(
        commands
            .record_assistant(
                THREAD,
                &dispatch.run_id,
                AssistantUpdate {
                    key: &wrong_key,
                    ..initial()
                }
            )
            .unwrap()
            .is_none()
    );

    let stream_sequence = commands
        .record_assistant(THREAD, &dispatch.run_id, initial())
        .unwrap()
        .unwrap();
    let complete_sequence = commands
        .record_assistant(
            THREAD,
            &dispatch.run_id,
            snapshot(
                Some(AssistantPhase::FinalAnswer),
                AssistantSnapshot::Completed {
                    text: "First paragraph. Second paragraph.",
                },
                2,
            ),
        )
        .unwrap()
        .unwrap();
    assert_eq!(stream_sequence, dispatch.sequence + 1);
    assert_eq!(complete_sequence, stream_sequence + 3);
    assert!(
        commands
            .record_assistant(THREAD, &dispatch.run_id, initial())
            .unwrap()
            .is_none(),
        "a stale snapshot cannot overwrite completed text",
    );

    let text_snapshot = connection
        .query_row(
            "SELECT item.ordinal, item.status, json_extract(item.payload_json, '$.phase'),
                    json_extract(item.payload_json, '$.text'),
                    json_extract(message.payload_json, '$.text'), node.kind, metadata.last_sequence
             FROM orchestration_v2_projection_turn_items AS item
             JOIN orchestration_v2_projection_messages AS message
                 ON message.message_id = json_extract(item.payload_json, '$.messageId')
             JOIN orchestration_v2_projection_nodes AS node ON node.node_id = message.node_id
             JOIN orchestration_v2_projection_metadata AS metadata ON metadata.projection_name = 'thread-projections'
             WHERE item.type = 'assistant_message' AND item.thread_id = ?1",
            [THREAD],
            |row| Ok((
                row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            )),
        )
        .unwrap();
    let (ordinal, status, phase, item_text, message_text, node_kind, cursor) = text_snapshot;
    assert_eq!(ordinal, 1_000_002);
    assert_eq!(status, "completed");
    assert_eq!(phase, "final_answer");
    assert_eq!(message_text, "First paragraph. Second paragraph.");
    assert_eq!(item_text, message_text);
    assert_eq!(node_kind, "assistant_message");
    assert_eq!(cursor, complete_sequence);
    assert_eq!(fixture.counts(), (12, 1, 2, 1));
}

fn seed_bound_turn(
    fixture: &Fixture,
    run_id: &str,
    ids: &TurnIds,
    key: &t3_rune::providers::codex::TurnKey,
) {
    let mut connection = fixture.connection();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "UPDATE orchestration_v2_projection_runs
             SET status = 'running', payload_json = json_set(payload_json, '$.status', 'running')
             WHERE run_id = ?1",
            [run_id],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE orchestration_v2_projection_run_attempts
             SET status = 'running', payload_json = json_set(payload_json, '$.status', 'running')
             WHERE attempt_id = ?1",
            [&ids.attempt_id],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE orchestration_v2_projection_provider_threads
             SET status = 'active', payload_json = json_set(payload_json, '$.status', 'active',
                 '$.nativeThreadRef', json_object('driver','codex','nativeId',?2,'strength','strong'))
             WHERE provider_thread_id = ?1",
            params![ids.provider_thread_id, key.thread_id.as_str()],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE orchestration_v2_projection_nodes
             SET status = 'running', payload_json = json_set(payload_json, '$.status', 'running')
             WHERE node_id = ?1",
            [&ids.root_node_id],
        )
        .unwrap();
    let provider_turn_id = format!(
        "provider-turn:provider:codex:native-turn:{}",
        encode_id_part(key.turn_id.as_str())
    );
    transaction
        .execute(
            "INSERT INTO orchestration_v2_projection_provider_turns
                 (provider_turn_id, thread_id, provider_thread_id, node_id,
                  run_attempt_id, ordinal, status, started_at, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 'running', ?6, ?7)",
            params![
                provider_turn_id, THREAD, ids.provider_thread_id, ids.root_node_id, ids.attempt_id, AT,
                json!({
                    "id": provider_turn_id,
                    "providerThreadId": ids.provider_thread_id,
                    "nodeId": ids.root_node_id,
                    "runAttemptId": ids.attempt_id,
                    "nativeTurnRef": { "driver":"codex", "nativeId":key.turn_id.as_str(), "strength":"strong" },
                    "ordinal": 1,
                    "status": "running",
                    "startedAt": AT,
                    "completedAt": null,
                }).to_string(),
            ],
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn interrupts_a_preparing_root_run_before_an_outbox_start_exists() {
    let fixture = Fixture::new();
    let mut connection = fixture.connection();
    let transaction = connection.transaction().unwrap();
    let (_, run_id) = append_preparing_dispatch(
        &transaction,
        PreparingDispatch {
            provider: crate::server::turn_commands::NativeProvider::Codex,
            command_id: "prepare-first",
            thread_id: THREAD,
            message_id: MESSAGE,
            text: TEXT,
            model_selection: json!({"instanceId": "codex", "model": "gpt-5.5"}),
            workspace_strategy: &json!({"type": "root"}),
            now: AT,
        },
    )
    .unwrap();
    transaction.commit().unwrap();
    let commands = fixture.commands();
    assert!(commands.can_stop_thread(THREAD).unwrap());
    let stopped = commands
        .stop_thread(
            serde_json::from_value(json!({
                "type": "thread.stop", "commandId": "stop-preparing", "threadId": THREAD,
                "reason": "Canceled during preparation",
            }))
            .unwrap(),
        )
        .unwrap();
    assert!(!commands.can_stop_thread(THREAD).unwrap());
    let events: Vec<_> = connection
        .prepare(
            "SELECT event_type, payload_json FROM orchestration_events
         WHERE command_id = 'stop-preparing' ORDER BY sequence",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (kind, payload) = row.unwrap();
            (kind, serde_json::from_str::<Value>(&payload).unwrap())
        })
        .collect();
    assert_eq!(
        events.iter().map(|(kind, _)| kind).collect::<Vec<_>>(),
        [
            "turn-item.updated",
            "turn-item.updated",
            "turn-item.updated",
            "run-attempt.updated",
            "node.updated",
            "run.updated",
        ]
    );
    assert_eq!(events[0].1["ordinal"], 1_000_003);
    assert_eq!(events[1].1["ordinal"], 1_000_004);
    assert_eq!(events[1].1["providerTurnId"], Value::Null);
    assert_eq!(events[1].1["parentItemId"], events[0].1["id"]);
    assert_eq!(
        events[1].1["message"],
        "Run interrupted before provider start"
    );
    assert_eq!(events[2].1["ordinal"], 1_000_002);
    assert_eq!(events[2].1["title"], "Workspace preparation interrupted");
    assert_eq!(events[2].1["output"], "Canceled during preparation");
    assert_eq!(events[5].1["id"], run_id);
    assert_eq!(events[5].1["status"], "interrupted");
    assert_eq!(events[5].1["checkpointId"], Value::Null);
    let (sequence, effects): (i64, i64) = connection
        .query_row(
            "SELECT result_sequence, (SELECT count(*) FROM orchestration_v2_effect_outbox)
         FROM orchestration_command_receipts WHERE command_id = 'stop-preparing'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(sequence, stopped.sequence);
    assert_eq!(effects, 0);
    let mut after = command_payload();
    after["messageId"] = "after-preparation-stop".into();
    let following = commands
        .dispatch(serde_json::from_value(after).unwrap())
        .unwrap();
    let effect_id = format!("effect:{COMMAND}:provider-turn.start:{}", following.run_id);
    let claim = commands
        .claim_start(&effect_id, "continuation-worker")
        .unwrap()
        .unwrap();
    assert_eq!(claim.run_ordinal, 2);
    assert!(claim.native_thread_id.is_none());
    let native = t3_rune::providers::codex::ThreadId::new("native-after-preparation-stop");
    assert!(
        commands
            .record_session_ready(&claim, false)
            .unwrap()
            .is_some()
    );
    assert!(
        commands
            .bind_native_thread(&claim, &native)
            .unwrap()
            .is_some()
    );
    let handoff = commands
        .prepare_unbound_history(&claim, THREAD, &native)
        .unwrap()
        .unwrap();
    assert_eq!(handoff.native_items.len(), 4);
    let workspace_context = handoff.native_items[2]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(workspace_context.ends_with(
        "\nCommand: Preparing workspace\nExit code: unknown\nCanceled during preparation"
    ));
}

#[test]
fn commits_the_bound_interrupt_effect_with_the_stop_receipt() {
    use super::interrupt_effect::ThreadStop;
    use super::provider_turn::NativeTurnUpdate;
    use t3_rune::providers::codex::{ThreadId, TurnId, TurnKey, TurnSnapshot, TurnStatus};

    let fixture = Fixture::new();
    let commands = fixture.commands();
    assert!(!commands.can_stop_thread(THREAD).unwrap());
    let dispatch = commands.dispatch(command()).unwrap();
    let ids = TurnIds::derive(THREAD, MESSAGE);
    let key = TurnKey {
        thread_id: ThreadId::new("native/thread"),
        turn_id: TurnId::new("native/turn"),
    };
    let startup = ids.start_effect_id(COMMAND);
    let claim = commands.claim_start(&startup, "worker").unwrap().unwrap();
    commands
        .record_session_ready(&claim, false)
        .unwrap()
        .unwrap();
    commands
        .bind_native_thread(&claim, &key.thread_id)
        .unwrap()
        .unwrap();
    let accepted = TurnSnapshot {
        key: key.clone(),
        status: TurnStatus::Running,
        started_at: Some(1_790_295_404),
        completed_at: None,
        error: None,
    };
    assert!(commands.accept_provider_start(&claim, &accepted).unwrap());
    let started = commands
        .record_provider_turn(
            THREAD,
            &dispatch.run_id,
            &key,
            NativeTurnUpdate::Started { started_at: None },
        )
        .unwrap()
        .unwrap();

    assert!(commands.can_stop_thread(THREAD).unwrap());
    assert!(!commands.can_stop_thread("unrelated").unwrap());
    let stop = |thread_id: &str, command_id: &str, reason: &str| {
        serde_json::from_value::<ThreadStop>(json!({
            "type":"thread.stop", "commandId": command_id, "threadId": thread_id, "reason": reason,
        }))
        .unwrap()
    };
    assert!(
        commands
            .stop_thread(stop("unrelated", "stop:wrong-thread", "wrong"))
            .is_err()
    );
    assert!(
        commands
            .stop_thread(stop(THREAD, COMMAND, "wrong type"))
            .is_err()
    );
    let receipt = commands
        .stop_thread(stop(THREAD, "stop:mine", "finish for today"))
        .unwrap();
    assert!(receipt.committed);
    assert_eq!(receipt.sequence, started + 1);
    let replay = commands
        .stop_thread(stop(THREAD, "stop:mine", "different reason"))
        .unwrap();
    assert!(!replay.committed);
    assert_eq!(replay.sequence, receipt.sequence);

    let connection = fixture.connection();
    let (run_status, event_payload, effect_id, effect_status, effect_payload, command_type): (String, String, String, String, String, String) = connection.query_row(
        "SELECT run.status, event.payload_json, effect.effect_id, effect.status, effect.payload_json, receipt.command_type
         FROM orchestration_command_receipts AS receipt
         JOIN orchestration_events AS event ON event.sequence = receipt.result_sequence
         JOIN orchestration_v2_effect_outbox AS effect ON effect.command_id = receipt.command_id
         JOIN orchestration_v2_projection_runs AS run ON run.run_id = ?1
         WHERE receipt.command_id = 'stop:mine' AND event.event_type = 'turn-item.updated'",
        [&dispatch.run_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    ).unwrap();
    assert_eq!(run_status, "running");
    assert_eq!(command_type, "thread.stop");
    assert_eq!(effect_status, "pending");
    assert_eq!(
        effect_id,
        "effect:stop:mine:provider-turn.interrupt:provider-turn:provider:codex:native-turn:native%2Fturn"
    );
    let item: Value = serde_json::from_str(&event_payload).unwrap();
    assert_eq!(item["message"], "finish for today");
    assert_eq!(item["type"], "run_interrupt_request");
    assert_eq!(item["ordinal"], 1_000_002);
    let effect: Value = serde_json::from_str(&effect_payload).unwrap();
    assert_eq!(effect["providerTurnId"], item["providerTurnId"]);
    assert_eq!(effect["providerThreadId"], item["providerThreadId"]);
    assert_eq!(effect["type"], "provider-turn.interrupt");

    let from_another_turn = TurnKey {
        thread_id: key.thread_id.clone(),
        turn_id: TurnId::new("other"),
    };
    assert!(
        commands
            .claim_interrupt(THREAD, &from_another_turn, "worker")
            .unwrap()
            .is_none()
    );
    let effect = commands
        .claim_interrupt(THREAD, &key, "worker")
        .unwrap()
        .unwrap();
    assert!(
        commands
            .claim_interrupt(THREAD, &key, "other worker")
            .unwrap()
            .is_none()
    );
    let timeout = commands
        .mark_interrupt_unresponsive(THREAD, &effect, "worker")
        .unwrap()
        .unwrap();
    assert_eq!(timeout, receipt.sequence + 1);
    let (status, source, outbox): (String, String, String) = connection.query_row(
        "SELECT run.status, json_extract(item.payload_json, '$.unresponsiveAt'), effect.status
         FROM orchestration_v2_projection_runs AS run
         JOIN orchestration_v2_projection_turn_items AS item ON item.run_id = run.run_id
         JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = run.thread_id
         WHERE run.run_id = ?1 AND item.type = 'run_interrupt_request' AND effect.command_id = 'stop:mine'",
        [&dispatch.run_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).unwrap();
    assert_eq!(status, "running");
    assert!(source.ends_with('Z'));
    assert_eq!(outbox, "succeeded");
    let again = commands
        .stop_thread(stop(THREAD, "stop:again", "finish for today"))
        .unwrap();
    assert_eq!(again.sequence, timeout + 1);
    let ordinal: i64 = connection
        .query_row(
            "SELECT ordinal FROM orchestration_v2_projection_turn_items
         WHERE run_id = ?1 AND type = 'run_interrupt_request'",
            [&dispatch.run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ordinal, 1_000_002);
    commands.cancel_process_bound_interrupts().unwrap();
    let effects: Vec<(String, String)> = connection
        .prepare(
            "SELECT command_id, status FROM orchestration_v2_effect_outbox
         WHERE effect_type = 'provider-turn.interrupt' ORDER BY rowid",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        effects,
        [
            ("stop:mine".into(), "succeeded".into()),
            ("stop:again".into(), "cancelled".into())
        ]
    );
    let replay = commands
        .stop_thread(stop(THREAD, "stop:again", "ignored"))
        .unwrap();
    assert!(!replay.committed);
    assert_eq!(replay.sequence, again.sequence);
}

#[test]
fn successful_turn_atomically_reaches_checkpoint_waiting() {
    use t3_rune::providers::codex::{ThreadId, TurnId, TurnKey};

    let fixture = Fixture::new();
    let commands = fixture.commands();
    let dispatch = commands.dispatch(command()).unwrap();
    let ids = TurnIds::derive(THREAD, MESSAGE);
    let key = TurnKey {
        thread_id: ThreadId::new("native-thread"),
        turn_id: TurnId::new("native-turn"),
    };
    seed_bound_turn(&fixture, &dispatch.run_id, &ids, &key);
    assert!(
        commands
            .await_checkpoint(THREAD, &dispatch.run_id, &key)
            .unwrap()
            .is_none()
    );

    let connection = fixture.connection();
    connection
        .execute(
            "UPDATE orchestration_v2_projection_provider_turns SET status = 'completed',
                 completed_at = ?1, payload_json = json_set(payload_json, '$.status', 'completed',
                  '$.completedAt', ?1, '$.turnTokenUsage', json_object('usageScope','main_agent',
                      'hasSubagents',json('false'),'usageStatus','unavailable'))",
            [AT],
        )
        .unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER checkpoint_test_failure BEFORE INSERT ON orchestration_v2_effect_outbox
             BEGIN SELECT RAISE(ABORT, 'injected checkpoint enqueue failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        commands.await_checkpoint(THREAD, &dispatch.run_id, &key),
        Err(TurnCommandError::Database(_)),
    ));
    assert_eq!(fixture.counts(), (8, 1, 1, 1));
    let still_running: String = connection
        .query_row(
            "SELECT status FROM orchestration_v2_projection_runs WHERE run_id = ?1",
            [&dispatch.run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(still_running, "running");

    connection
        .execute_batch("DROP TRIGGER checkpoint_test_failure")
        .unwrap();
    let sequence = commands
        .await_checkpoint(THREAD, &dispatch.run_id, &key)
        .unwrap()
        .unwrap();
    assert_eq!(sequence, dispatch.sequence + 4);
    assert!(
        commands
            .await_checkpoint(THREAD, &dispatch.run_id, &key)
            .unwrap()
            .is_none()
    );
    let (run_status, root_status, attempt_status, provider_status, effect_id, command_id, payload) =
        connection
            .query_row(
                "SELECT run.status, node.status, attempt.status, provider_thread.status,
                        effect.effect_id, effect.command_id, effect.payload_json
                 FROM orchestration_v2_projection_runs AS run
                 JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.run_id = run.run_id
                 JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id
                 JOIN orchestration_v2_projection_provider_threads AS provider_thread
                   ON provider_thread.provider_thread_id = attempt.provider_thread_id
                 JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = run.thread_id
                 WHERE run.run_id = ?1 AND effect.effect_type = 'checkpoint.capture'
                   AND effect.status = 'pending' AND effect.attempt_count = 0",
                [&dispatch.run_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?)),
            )
            .unwrap();
    assert_eq!(run_status, "waiting");
    assert_eq!(root_status, "waiting");
    assert_eq!(attempt_status, "completed");
    assert_eq!(provider_status, "idle");
    assert_eq!(
        effect_id,
        format!("effect:checkpoint.capture:{}", dispatch.run_id)
    );
    assert_eq!(command_id, format!("command:{effect_id}"));
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap(),
        json!({
            "type": "checkpoint.capture",
            "runId": dispatch.run_id,
            "scopeId": ids.checkpoint_scope_id,
        })
    );
    assert_eq!(fixture.counts(), (12, 1, 1, 2));
}

#[tokio::test]
async fn persists_native_usage_and_completes_checkpoints_for_a_plain_directory() {
    use super::provider_turn::NativeTurnUpdate;
    use t3_rune::providers::codex::{
        ThreadId, TokenUsageSnapshot, TurnId, TurnKey, TurnSnapshot, TurnStatus, TurnTokenUsage,
    };

    let fixture = Fixture::new();
    let commands = fixture.commands();
    let dispatch = commands.dispatch(command()).unwrap();
    let ids = TurnIds::derive(THREAD, MESSAGE);
    let key = TurnKey {
        thread_id: ThreadId::new("native-thread"),
        turn_id: TurnId::new("native-turn"),
    };
    let startup_effect = ids.start_effect_id(COMMAND);
    let claim = commands
        .claim_start(&startup_effect, "worker")
        .unwrap()
        .unwrap();
    assert!(
        commands
            .bind_native_thread(&claim, &key.thread_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        commands.record_session_ready(&claim, false).unwrap(),
        Some(super::provider_session::SessionAttachment::Attached(
            dispatch.sequence + 1
        ))
    );
    let activated = commands
        .bind_native_thread(&claim, &key.thread_id)
        .unwrap()
        .unwrap();
    assert_eq!(activated, dispatch.sequence + 6);
    let accepted = TurnSnapshot {
        key: key.clone(),
        status: TurnStatus::Running,
        started_at: Some(1_790_295_404),
        completed_at: None,
        error: None,
    };
    assert!(commands.accept_provider_start(&claim, &accepted).unwrap());
    assert!(!commands.accept_provider_start(&claim, &accepted).unwrap());
    let connection = fixture.connection();
    let started = commands
        .record_provider_turn(
            THREAD,
            &dispatch.run_id,
            &key,
            NativeTurnUpdate::Started {
                started_at: Some(OffsetDateTime::from_unix_timestamp(1_790_295_404).unwrap()),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(started, activated + 1);
    let snapshot: TokenUsageSnapshot = serde_json::from_value(json!({
        "total": {"totalTokens":22737,"inputTokens":22730,"cachedInputTokens":6912,
                  "cacheWriteInputTokens":0,"outputTokens":7,"reasoningOutputTokens":0},
        "last": {"totalTokens":22737,"inputTokens":22730,"cachedInputTokens":6912,
                 "cacheWriteInputTokens":0,"outputTokens":7,"reasoningOutputTokens":0},
        "modelContextWindow":258400,
    }))
    .unwrap();
    let usage_event = commands
        .record_provider_turn(
            THREAD,
            &dispatch.run_id,
            &key,
            NativeTurnUpdate::Usage(snapshot),
        )
        .unwrap()
        .unwrap();
    assert_eq!(usage_event, started + 1);
    let terminal: TurnTokenUsage = serde_json::from_value(json!({
        "usageScope": "main_agent",
        "usageStatus": "complete",
        "hasSubagents": false,
        "models": ["gpt-6-luna"],
        "inputTokens": 22730,
        "cachedInputTokens": 6912,
        "cacheCreationTokens": 0,
        "outputTokens": 7,
        "reasoningTokens": 0,
    }))
    .unwrap();
    let native_completed = commands
        .record_provider_turn(
            THREAD,
            &dispatch.run_id,
            &key,
            NativeTurnUpdate::Settled {
                status: t3_rune::providers::codex::SettlementStatus::Completed,
                completed_at: Some(OffsetDateTime::from_unix_timestamp(1_790_295_409).unwrap()),
                usage: &terminal,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(native_completed, usage_event + 1);
    let stored_native: Value = connection.query_row(
        "SELECT payload_json FROM orchestration_v2_projection_provider_turns WHERE thread_id = ?1",
        [THREAD],
        |row| row.get::<_, String>(0),
    ).map(|json| serde_json::from_str(&json).unwrap()).unwrap();
    assert_eq!(stored_native["status"], "completed");
    assert_eq!(stored_native["model"], "gpt-6-luna");
    assert_eq!(stored_native["tokenUsage"]["usedTokens"], 22737);
    assert_eq!(
        stored_native["turnTokenUsage"],
        serde_json::to_value(terminal).unwrap()
    );
    let waiting = commands
        .await_checkpoint(THREAD, &dispatch.run_id, &key)
        .unwrap()
        .unwrap();
    assert_eq!(waiting, native_completed + 4);
    let effect_id = format!("effect:checkpoint.capture:{}", dispatch.run_id);

    let capture = commands
        .capture_checkpoint(&effect_id, "checkpoint-worker")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(capture, waiting + 5);
    assert!(
        commands
            .capture_checkpoint(&effect_id, "checkpoint-worker")
            .await
            .unwrap()
            .is_none()
    );
    let (status, effect_status, attempts, receipt_status, receipt_sequence, baseline_count) =
        connection.query_row(
            "SELECT run.status, effect.status, effect.attempt_count, receipt.status,
                    receipt.result_sequence, (
                        SELECT count(*) FROM orchestration_v2_projection_checkpoints
                        WHERE thread_id = run.thread_id AND status = 'missing'
                          AND json_extract(payload_json, '$.ref') LIKE 'refs/t3/orchestration-v2/checkpoints/%'
                    )
             FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = run.thread_id
             JOIN orchestration_command_receipts AS receipt ON receipt.command_id = effect.command_id
             WHERE run.run_id = ?1 AND effect.effect_id = ?2 AND receipt.command_type = 'checkpoint.capture'",
             params![dispatch.run_id,effect_id],
             |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?,
                 row.get::<_, String>(3)?, row.get::<_, i64>(4)?, row.get::<_, u64>(5)?)),
        ).unwrap();
    assert_eq!(status, "completed");
    assert_eq!(effect_status, "succeeded");
    assert_eq!(attempts, 1);
    assert_eq!(receipt_status, "accepted");
    assert_eq!(receipt_sequence, capture);
    assert_eq!(baseline_count, 2);
}
