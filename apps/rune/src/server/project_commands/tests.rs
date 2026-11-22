use std::sync::{Arc, Barrier};

use super::*;

const FIRST: &str = "2026-03-10T00:01:02.000Z";
const SECOND: &str = "2026-03-10T00:02:00.009Z";
const THIRD: &str = "2026-03-10T00:03:00.001Z";

struct Fixture {
    dir: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("t3-project-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("statev2.sqlite");
        let fixture = Self { dir, db };
        fixture.connection().execute_batch(
            "CREATE TABLE orchestration_events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT NOT NULL UNIQUE,
                aggregate_kind TEXT NOT NULL, stream_id TEXT NOT NULL, stream_version INTEGER NOT NULL,
                event_type TEXT NOT NULL, occurred_at TEXT NOT NULL, command_id TEXT,
                causation_event_id TEXT, correlation_id TEXT, actor_kind TEXT NOT NULL,
                payload_json TEXT NOT NULL, metadata_json TEXT NOT NULL,
                application_event_version INTEGER NOT NULL DEFAULT 1
            );
            CREATE UNIQUE INDEX idx_orch_events_stream_version
            ON orchestration_events(aggregate_kind, stream_id, stream_version);
            CREATE INDEX idx_orchestration_events_application_high_water
            ON orchestration_events(sequence)
            WHERE aggregate_kind = 'project' OR (application_event_version = 2 AND aggregate_kind = 'thread');
            CREATE TABLE orchestration_command_receipts (
                command_id TEXT PRIMARY KEY, aggregate_kind TEXT NOT NULL, aggregate_id TEXT NOT NULL,
                accepted_at TEXT NOT NULL, result_sequence INTEGER NOT NULL, status TEXT NOT NULL,
                error TEXT, command_type TEXT NOT NULL DEFAULT 'legacy'
            );
            CREATE TABLE projection_projects (
                project_id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace_root TEXT NOT NULL,
                scripts_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                deleted_at TEXT, default_model_selection_json TEXT, default_thread_env_mode TEXT,
                favicon_path TEXT, auto_pull INTEGER NOT NULL DEFAULT 0, project_icon_json TEXT,
                pinned_at TEXT, archived_at TEXT, has_custom_title INTEGER NOT NULL DEFAULT 0,
                tracked_folders_json TEXT NOT NULL DEFAULT '[]'
            );"
        ).unwrap();
        fixture
    }

    fn connection(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn commands(&self) -> ProjectCommands {
        ProjectCommands::open(self.db.clone()).unwrap()
    }

    fn workspace(&self) -> PathBuf {
        self.dir.join("project")
    }

    fn create(&self, command_id: &str, project_id: &str) -> Value {
        json!({
            "type": "project.create", "commandId": command_id, "projectId": project_id,
            "title": " Project ", "workspaceRoot": self.dir.join("project/../project"),
            "createWorkspaceRootIfMissing": true, "defaultModelSelection": null,
        })
    }

    fn records(&self) -> (u64, u64, u64) {
        self.connection()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM orchestration_events),
                    (SELECT COUNT(*) FROM projection_projects),
                    (SELECT COUNT(*) FROM orchestration_command_receipts)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn project_mutations_preserve_metadata_and_publish_legacy_compatible_records() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let created = commands
        .dispatch_at(fixture.create("create", "p"), FIRST)
        .unwrap();
    assert!(created.committed);
    assert!(fixture.workspace().is_dir());
    assert_eq!(
        created.project.workspace_root,
        fixture.workspace().to_str().unwrap()
    );
    assert_eq!(created.project.title, "Project");
    fixture.connection().execute("UPDATE projection_projects SET scripts_json = ?1 WHERE project_id = 'p'", [
        r#"[{"id":"local","name":"Local script","command":"cargo build","icon":"build","runOnWorktreeCreate":false}]"#,
    ]).unwrap();

    let update = json!({
        "type": "project.update", "commandId": "archive", "projectId": "p",
        "title": "Renamed", "pinned": true, "archived": true,
    });
    let archived = commands.dispatch_at(update.clone(), SECOND).unwrap();
    assert!(archived.committed);
    assert_eq!(archived.project.pinned_at.as_deref(), Some(SECOND));
    assert_eq!(archived.project.archived_at.as_deref(), Some(SECOND));
    assert_eq!(archived.project.title, "Renamed");
    assert_eq!(archived.project.scripts[0]["id"], "local");

    let retry = commands.dispatch_at(update, THIRD).unwrap();
    assert!(!retry.committed);
    assert_eq!(retry.project.updated_at, SECOND);
    assert_eq!(fixture.records(), (2, 1, 2));

    let repeated_flag = commands
        .dispatch_at(
            json!({
                "type": "project.update", "commandId": "archive-again", "projectId": "p",
                "archived": true, "pinned": false,
            }),
            THIRD,
        )
        .unwrap();
    assert_eq!(repeated_flag.project.archived_at.as_deref(), Some(SECOND));
    assert!(repeated_flag.project.pinned_at.is_none());
    assert_eq!(repeated_flag.project.updated_at, THIRD);
    let (version, event, payload, status, receipt_type, custom_title): (
        u64,
        String,
        String,
        String,
        String,
        bool,
    ) = fixture
        .connection()
        .query_row(
            "SELECT event.stream_version, event.event_type, event.payload_json, receipt.status,
                receipt.command_type, project.has_custom_title
         FROM orchestration_events AS event
         JOIN orchestration_command_receipts AS receipt ON receipt.result_sequence = event.sequence
         JOIN projection_projects AS project ON project.project_id = event.stream_id
         WHERE event.command_id = 'archive-again' AND event.application_event_version = 2",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(event, "project.meta-updated");
    assert_eq!(status, "accepted");
    assert_eq!(receipt_type, "project.meta.update");
    assert!(custom_title);
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap(),
        json!({
            "projectId": "p", "pinnedAt": null, "archivedAt": SECOND, "updatedAt": THIRD,
        })
    );

    let unarchived = commands.dispatch_at(json!({
        "type": "project.update", "commandId": "unarchive", "projectId": "p", "archived": false,
    }), THIRD).unwrap();
    assert!(unarchived.project.archived_at.is_none());
    assert_eq!(fixture.records(), (4, 1, 4));
}

#[test]
fn concurrent_workspace_registration_is_serialized_and_rejection_is_replayed() {
    let fixture = Fixture::new();
    let ready = Arc::new(Barrier::new(2));
    let commands = fixture.commands();
    let first = fixture.create("first", "p1");
    let second = fixture.create("second", "p2");
    let (first_result, second_result) = std::thread::scope(|scope| {
        let thread_ready = Arc::clone(&ready);
        let first_ref = &first;
        let first_commands = &commands;
        let worker = scope.spawn(move || {
            thread_ready.wait();
            first_commands.dispatch_at(first_ref.clone(), FIRST)
        });
        ready.wait();
        let second_result = commands.dispatch_at(second.clone(), SECOND);
        (worker.join().unwrap(), second_result)
    });
    let (rejected, winner) = match (first_result, second_result) {
        (Ok(_), Err(CommandError::Rejected(_))) => (second, "p1"),
        (Err(CommandError::Rejected(_)), Ok(_)) => (first, "p2"),
        _ => panic!("one project must own the workspace"),
    };
    assert_eq!(fixture.records(), (1, 1, 2));
    let error: String = fixture
        .connection()
        .query_row(
            "SELECT error FROM orchestration_command_receipts WHERE status = 'rejected'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&error).unwrap()["conflictingProjectId"],
        winner
    );
    let error = commands.dispatch_at(rejected, THIRD).err().unwrap();
    assert!(matches!(error, CommandError::Rejected(_)));
    assert_eq!(fixture.records(), (1, 1, 2));
}

#[test]
fn a_failed_projection_rolls_back_the_event_and_receipt() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    fixture
        .connection()
        .execute_batch(
            "CREATE TRIGGER reject_projection BEFORE INSERT ON projection_projects
         BEGIN SELECT RAISE(ABORT, 'failing projection'); END;",
        )
        .unwrap();
    let payload = fixture.create("create", "p");
    assert!(matches!(
        commands.dispatch_at(payload.clone(), FIRST),
        Err(CommandError::Database(_))
    ));
    assert_eq!(fixture.records(), (0, 0, 0));

    fixture
        .connection()
        .execute_batch("DROP TRIGGER reject_projection")
        .unwrap();
    assert!(commands.dispatch_at(payload, SECOND).unwrap().committed);
    assert_eq!(fixture.records(), (1, 1, 1));
}

#[test]
fn public_commands_validate_invalid_optional_values_and_gate_folder_creation() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let mut create = fixture.create("create", "p");
    create["createWorkspaceRootIfMissing"] = json!(false);
    assert!(matches!(
        commands.dispatch(create),
        Err(CommandError::InvalidRequest(_))
    ));
    assert!(!fixture.workspace().exists());
    assert_eq!(fixture.records(), (0, 0, 0));
    assert!(
        commands
            .dispatch(fixture.create("create", "p"))
            .unwrap()
            .committed
    );
    assert!(matches!(
        commands.dispatch(json!({
            "type": "project.update", "commandId": "archive", "projectId": "p", "archived": null,
        })),
        Err(CommandError::InvalidRequest(_))
    ));
    assert_eq!(fixture.records(), (1, 1, 1));
}
