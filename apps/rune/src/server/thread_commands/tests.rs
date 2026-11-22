use std::fs;
use std::sync::{Arc, Barrier};

use uuid::Uuid;

use super::*;
use crate::server::bootstrap;

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
    commands: ThreadCommands,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("t3-thread-create-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("statev2.sqlite");
        bootstrap::initialize(&path).unwrap();
        let commands = ThreadCommands::open(path.clone()).unwrap();
        Self {
            directory,
            path,
            commands,
        }
    }

    fn connection(&self) -> Connection {
        Connection::open(&self.path).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn create(command_id: &str, thread_id: &str) -> Value {
    json!({
        "type": "thread.create",
        "commandId": command_id,
        "threadId": thread_id,
        "projectId": "project:one",
        "createdBy": "agent",
        "creationSource": "web",
        "title": " New thread ",
        "modelSelection": {
            "instanceId": "codex-custom",
            "model": " gpt-5.4 ",
            "options": [{"id":" effort ","value":" high "},{"id":"fast","value":false}],
        },
        "runtimeMode": "approval-required",
        "interactionMode": "plan",
        "branch": " main ",
        "worktreePath": null,
    })
}

#[test]
fn refuses_a_new_thread_after_its_project_is_deleted() {
    let fixture = Fixture::new();
    let database = fixture.connection();
    database.execute_batch(
        "INSERT INTO projection_projects
          (project_id, title, workspace_root, scripts_json, created_at, updated_at, deleted_at)
         VALUES ('project:one', 'Removed', '/deleted', '[]', '2026-01-01', '2026-01-02', '2026-01-02');",
    ).unwrap();

    assert!(matches!(
        fixture.commands.dispatch(create("after-delete", "thread:orphan")),
        Err(ThreadCommandError::Rejected(message)) if message == "Project project:one was not found."
    ));
    let rows = database.query_row(
        "SELECT COUNT(*) FROM orchestration_v2_projection_threads WHERE thread_id = 'thread:orphan'",
        [],
        |row| row.get::<_, u64>(0),
    ).unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn visits_advance_the_watermark_and_keep_thread_activity_intact() {
    let fixture = Fixture::new();
    fixture
        .commands
        .dispatch(create("create", "thread:visit"))
        .unwrap();
    let database = fixture.connection();
    let updated_at = || {
        database.query_row(
            "SELECT updated_at FROM orchestration_v2_projection_threads WHERE thread_id = 'thread:visit'",
            [],
            |row| row.get::<_, String>(0),
        ).unwrap()
    };
    let original_activity = updated_at();
    let command = json!({
        "type": "thread.visit", "commandId": "visit", "threadId": "thread:visit",
        "visitedAt": "2026-01-01T02:02:03.000+01:00", "opened": true,
        "searchQuery": "“    Från   Örebro  ”",
    });
    let committed = fixture.commands.dispatch(command.clone()).unwrap();
    assert!(committed.committed);
    let replayed = fixture.commands.dispatch(command).unwrap();
    assert_eq!(replayed.sequence, committed.sequence);
    assert!(!replayed.committed);
    let stale = fixture
        .commands
        .dispatch(json!({
            "type": "thread.visit", "commandId": "stale", "threadId": "thread:visit",
            "visitedAt": "2026-01-01T01:02:02.000Z",
        }))
        .unwrap();
    assert_eq!(stale.sequence, committed.sequence);
    assert!(!stale.committed);
    assert_eq!(updated_at(), original_activity);

    let payload = database.query_row(
        "SELECT payload_json FROM orchestration_events WHERE event_type = 'thread.visit-recorded'",
        [],
        |row| row.get::<_, String>(0),
    ).unwrap();
    let event: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(event["lastVisitedAt"], "2026-01-01T01:02:03.000Z");
    assert_eq!(event["opens"]["picks"][0]["query"], "fran orebro");
    assert_eq!(event["opens"]["score"].as_f64(), Some(1.0));
    let count: u64 = database.query_row(
        "SELECT COUNT(*) FROM orchestration_command_receipts WHERE command_type = 'thread.visit'",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(count, 2);
}

#[test]
fn creates_an_empty_root_thread_and_replays_the_receipt_without_mutation() {
    let fixture = Fixture::new();
    let mut input = create("cmd/one", "thread:☃");
    input["titleRenamed"] = true.into();
    let result = fixture.commands.dispatch(input.clone()).unwrap();
    assert!(result.committed);
    assert!(result.sequence > 0);

    let database = fixture.connection();
    let (event_id, kind, event_type, version, sequence, metadata, payload): (
        String,
        String,
        String,
        u64,
        u64,
        String,
        String,
    ) = database
        .query_row(
            "SELECT event_id, actor_kind, event_type, application_event_version, sequence,
                metadata_json, payload_json
         FROM orchestration_events WHERE stream_id = 'thread:☃' AND stream_version = 0
         AND command_id = 'cmd/one' AND correlation_id = 'cmd/one' AND causation_event_id IS NULL",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    let prefix = "event:thread:thread%3A%E2%98%83:command:cmd%2Fone:";
    Uuid::parse_str(event_id.strip_prefix(prefix).unwrap()).unwrap();
    assert_eq!(kind, "server");
    assert_eq!(event_type, "thread.created");
    assert_eq!(version, 2);
    assert_eq!(sequence, result.sequence);
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap(),
        json!({"providerInstanceId":"codex-custom"})
    );

    let projection: String = database
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_threads
         WHERE thread_id = 'thread:☃' AND project_id = 'project:one' AND title = 'New thread'
           AND default_provider = 'codex-custom' AND provider_instance_id = 'codex-custom'
           AND runtime_mode = 'approval-required' AND interaction_mode = 'plan'
           AND created_at = updated_at AND archived_at IS NULL AND deleted_at IS NULL
           AND active_provider_thread_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projection, payload);
    let thread: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(thread["createdBy"], "user");
    assert_eq!(thread["creationSource"], "web");
    assert_eq!(thread["titleRenamed"], true);
    assert_eq!(thread["branch"], "main");
    assert_eq!(thread["worktreePath"], Value::Null);
    assert_eq!(
        thread["lineage"],
        json!({
            "rootThreadId":"thread:☃", "parentThreadId":null, "relationshipToParent":null,
        })
    );
    assert_eq!(
        thread["modelSelection"],
        json!({
            "instanceId": "codex-custom", "model": "gpt-5.4",
            "options": [{"id":"effort","value":"high"},{"id":"fast","value":false}],
        })
    );

    input["title"] = "Different title".into();
    let replay = fixture.commands.dispatch(input).unwrap();
    assert!(!replay.committed);
    assert_eq!(replay.sequence, result.sequence);
    assert_eq!(
        database
            .query_row(
                "SELECT count(*) FROM orchestration_events WHERE stream_id = ?1",
                ["thread:☃"],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        1,
    );
    assert_eq!(
        database
            .query_row(
                "SELECT result_sequence FROM orchestration_command_receipts
         WHERE command_id = 'cmd/one' AND aggregate_kind = 'thread' AND aggregate_id = 'thread:☃'
           AND command_type = 'thread.create' AND status = 'accepted' AND error IS NULL",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        result.sequence
    );
    assert_eq!(
        database
            .query_row(
                "SELECT last_sequence FROM orchestration_v2_projection_metadata
         WHERE projection_name = 'thread-projections' AND schema_version = 2",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        result.sequence
    );
}

#[test]
fn concurrent_creates_reserve_the_thread_id_and_persist_the_rejection() {
    let fixture = Fixture::new();
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["first", "second"].map(|command_id| {
        let commands = fixture.commands.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            (
                command_id,
                commands.dispatch(create(command_id, "thread:race")),
            )
        })
    });
    let mut results = handles.map(|handle| handle.join().unwrap());
    results.sort_by_key(|(_, result)| result.is_err());
    let [(winner, committed), (loser, rejected)] = results;
    let committed = committed.unwrap();
    assert!(committed.committed);
    assert_eq!(
        rejected.unwrap_err().to_string(),
        "Thread thread:race already exists. Use a new thread id."
    );
    assert_eq!(
        fixture
            .commands
            .dispatch(create(winner, "thread:different"))
            .unwrap_err()
            .to_string(),
        format!(
            "Command {winner} was already handled for thread thread:race and cannot be replayed for thread:different."
        )
    );
    assert_eq!(
        fixture
            .commands
            .dispatch(create(loser, "thread:race"))
            .unwrap_err()
            .to_string(),
        format!(
            "Command {loser} was previously rejected: Thread thread:race already exists. Use a new thread id."
        )
    );
    let database = fixture.connection();
    let rejected_sequence: u64 = database.query_row(
        "SELECT result_sequence FROM orchestration_command_receipts WHERE command_id = ?1 AND status = 'rejected'",
        [loser], |row| row.get(0),
    ).unwrap();
    assert_eq!(rejected_sequence, committed.sequence);

    database.execute("UPDATE orchestration_v2_projection_threads SET deleted_at = updated_at WHERE thread_id = 'thread:race'", []).unwrap();
    assert_eq!(
        fixture
            .commands
            .dispatch(create("tombstone", "thread:race"))
            .unwrap_err()
            .to_string(),
        "Thread thread:race already exists. Use a new thread id."
    );
}

#[test]
fn receipt_failure_rolls_back_the_event_projection_and_cursor() {
    let fixture = Fixture::new();
    let database = fixture.connection();
    database
        .execute_batch(
            "CREATE TRIGGER fail_receipt BEFORE INSERT ON orchestration_command_receipts
         BEGIN SELECT RAISE(ABORT, 'forced receipt failure'); END;",
        )
        .unwrap();
    let input = create("interrupted", "thread:retry");
    let failure = fixture.commands.dispatch(input.clone()).unwrap_err();
    assert!(matches!(failure, ThreadCommandError::Database(_)));
    assert_eq!(database.query_row(
        "SELECT (SELECT count(*) FROM orchestration_events WHERE stream_id = 'thread:retry'),
                (SELECT count(*) FROM orchestration_v2_projection_threads WHERE thread_id = 'thread:retry'),
                (SELECT last_sequence FROM orchestration_v2_projection_metadata WHERE projection_name = 'thread-projections')",
        [], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?, row.get::<_, u64>(2)?)),
    ).unwrap(), (0, 0, 0));
    database.execute_batch("DROP TRIGGER fail_receipt").unwrap();
    assert!(fixture.commands.dispatch(input).unwrap().committed);
}

#[test]
fn rejects_worktree_and_server_owned_launch_fields_before_persistence() {
    let fixture = Fixture::new();
    for (key, value) in [
        ("worktreePath", json!("/unowned")),
        ("importedNativeThread", json!({})),
        ("initialMessage", json!({})),
        ("linkOrigin", json!({})),
        ("delegatedFrom", json!({})),
        ("titleRenamed", Value::Null),
    ] {
        let mut input = create("invalid", "thread:invalid");
        input[key] = value;
        assert!(matches!(
            fixture.commands.dispatch(input),
            Err(ThreadCommandError::InvalidRequest(_))
        ));
    }
    let mut missing_branch = create("missing-branch", "thread:invalid");
    missing_branch.as_object_mut().unwrap().remove("branch");
    assert!(matches!(
        fixture.commands.dispatch(missing_branch),
        Err(ThreadCommandError::InvalidRequest(_))
    ));
    assert!(
        fixture
            .commands
            .dispatch(create("invalid", "thread:invalid"))
            .unwrap()
            .committed
    );
}

#[test]
fn legacy_model_options_keep_javascript_property_order() {
    let fixture = Fixture::new();
    let mut input = create("legacy-options", "thread:legacy-options");
    input["modelSelection"] = serde_json::from_str(
        r#"{"provider":"custom-codex","model":"gpt-5.4","options":{
            "fast":true, "10":"ten", "049":"leading zero", "2":"two",
            " effort ":" high ", " effort-invalid ":123, "effort":"low", "":"ignored",
            "4294967295":"above index range", "3":false
        }}"#,
    )
    .unwrap();
    fixture.commands.dispatch(input).unwrap();

    let payload: String = fixture.connection().query_row(
        "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = 'thread:legacy-options'",
        [], |row| row.get(0),
    ).unwrap();
    let thread: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        thread["modelSelection"],
        json!({
            "instanceId":"custom-codex", "model":"gpt-5.4",
            "options":[
                {"id":"2", "value":"two"},
                {"id":"3", "value":false},
                {"id":"10", "value":"ten"},
                {"id":"fast", "value":true},
                {"id":"049", "value":"leading zero"},
                {"id":"effort", "value":"high"},
                {"id":"effort", "value":"low"},
                {"id":"4294967295", "value":"above index range"},
            ],
        })
    );
}
