use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("t3-rune-{}.sqlite", uuid::Uuid::new_v4()));
        let fixture = Self(path);
        let db = Connection::open(&fixture.0).unwrap();
        db.execute_batch(
            "CREATE TABLE projection_projects (
                 project_id TEXT PRIMARY KEY, title TEXT, workspace_root TEXT,
                 created_at TEXT, deleted_at TEXT, archived_at TEXT,
                 default_model_selection_json TEXT
             );
             CREATE TABLE orchestration_v2_projection_threads (
                 thread_id TEXT PRIMARY KEY, project_id TEXT, title TEXT, payload_json TEXT,
                 updated_at TEXT, deleted_at TEXT, archived_at TEXT,
                 created_at TEXT NOT NULL DEFAULT '2026-01-01'
             );
             CREATE TABLE orchestration_events (
                 sequence INTEGER PRIMARY KEY, aggregate_kind TEXT, stream_id TEXT,
                 application_event_version INTEGER
             );
             CREATE INDEX idx_orchestration_events_application_high_water
             ON orchestration_events(sequence)
             WHERE aggregate_kind = 'project'
                OR (application_event_version = 2 AND aggregate_kind = 'thread');
             CREATE TABLE orchestration_v2_projection_runs (
                 run_id TEXT PRIMARY KEY, thread_id TEXT, status TEXT, ordinal INTEGER,
                 completed_at TEXT, requested_at TEXT, payload_json TEXT
             );
             CREATE TABLE orchestration_v2_projection_run_attempts (run_id TEXT, status TEXT);
             CREATE TABLE orchestration_v2_projection_turn_items (
                 turn_item_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT, ordinal INTEGER,
                 type TEXT, updated_at TEXT, payload_json TEXT, status TEXT
             );
             CREATE TABLE orchestration_v2_projection_messages (
                 message_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT, role TEXT,
                 streaming INTEGER, created_at TEXT, updated_at TEXT, payload_json TEXT
             );
             CREATE TABLE projection_thread_messages (
                 message_id TEXT PRIMARY KEY, thread_id TEXT, role TEXT, text TEXT,
                 created_at TEXT, updated_at TEXT
             );
             CREATE TABLE orchestration_v2_legacy_imports (
                 thread_id TEXT PRIMARY KEY, transcript_imported_at TEXT
             );
             INSERT INTO projection_projects VALUES
                 ('project', 'Project', '/work/project', '2026-01-01', NULL, NULL,
                  '{\"instanceId\":\"codex\",\"model\":\"gpt-5.4\"}'),
                 ('deleted', 'Deleted project', '/work/deleted', '2026-01-02', '2026-01-04', NULL, NULL);
             INSERT INTO orchestration_v2_projection_threads (
                 thread_id, project_id, title, payload_json, updated_at, deleted_at, archived_at
             ) VALUES
                 ('active', 'project', 'Active', '{}', '2026-01-03', NULL, NULL),
                 ('archived', 'project', 'Archived', '{}', '2026-01-02', NULL, '2026-01-04'),
                 ('deleted-thread', 'project', 'Deleted', '{}', '2026-01-04', '2026-01-04', NULL),
                 ('orphaned', 'deleted', 'Orphaned', '{}', '2026-01-04', NULL, NULL),
                 ('side', 'project', 'Side', '{ \"lineage\": { \"sideChat\": true } }', '2026-01-04', NULL, NULL);
             INSERT INTO orchestration_events VALUES
                 (1, 'project', 'project', 2),
                 (3, 'thread', 'active', 2),
                 (5, 'thread', 'archived', 2),
                 (6, 'thread', 'active', 1);"
        ).unwrap();
        fixture
    }

    fn store(&self) -> Store {
        Store::open(self.0.clone()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn reads_the_packaged_desktop_launch_fixture() {
    let fixture = Fixture(
        std::env::temp_dir().join(format!("t3-rune-release-{}.sqlite", uuid::Uuid::new_v4())),
    );
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/fixtures/desktop-launch/statev2.sqlite");
    std::fs::copy(source, &fixture.0).unwrap();
    let store = fixture.store();
    let shell = store.shell().unwrap();
    let thread = shell
        .threads
        .iter()
        .find(|thread| thread.title == "Release launch check")
        .unwrap();
    assert!(
        shell
            .projects
            .iter()
            .any(|project| project.id == thread.project_id)
    );
    let detail = store.thread(&thread.id).unwrap().unwrap();
    assert!(detail.messages.iter().any(|message| {
        message
            .text
            .contains("The release opens and shows this thread.")
    }));
}

#[test]
fn reads_listed_projects_and_threads_with_their_snapshot_sequences() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let shell = serde_json::to_value(store.shell().unwrap()).unwrap();
    assert_eq!(shell["snapshotSequence"], 5);
    assert_eq!(shell["projects"].as_array().unwrap().len(), 1);
    assert_eq!(shell["projects"][0]["workspaceRoot"], "/work/project");
    assert_eq!(
        shell["projects"][0]["defaultModelSelection"],
        serde_json::json!({"instanceId": "codex", "model": "gpt-5.4"})
    );
    assert_eq!(shell["threads"].as_array().unwrap().len(), 1);
    assert_eq!(shell["threads"][0]["id"], "active");
    assert_eq!(shell["archivedThreads"].as_array().unwrap().len(), 1);
    assert_eq!(shell["archivedThreads"][0]["id"], "archived");
    assert_eq!(
        store.thread("active").unwrap().unwrap().snapshot_sequence,
        3
    );
    assert!(store.thread("archived").unwrap().is_some());
    assert!(store.thread("deleted-thread").unwrap().is_none());
    assert!(store.thread("side").unwrap().is_none());
}

#[test]
fn reads_completed_text_in_order_during_partial_legacy_import() {
    let fixture = Fixture::new();
    let db = Connection::open(&fixture.0).unwrap();
    db.execute_batch(
        "INSERT INTO orchestration_v2_legacy_imports VALUES ('active', NULL);
         INSERT INTO projection_thread_messages VALUES
             ('legacy-b', 'active', 'user', 'Legacy question', '2026-01-01', '2026-01-01'),
             ('legacy-a', 'active', 'assistant', 'Old copy', '2026-01-02', '2026-01-02');
         INSERT INTO orchestration_v2_projection_runs (run_id, thread_id, status)
             VALUES ('rolled-back', 'active', 'rolled_back');
         INSERT INTO orchestration_v2_projection_messages VALUES
             ('legacy-a', 'active', NULL, 'assistant', 0, '2026-01-02', '2026-01-02', '{ \"text\": \"Current copy\" }'),
             ('rollback', 'active', 'rolled-back', 'assistant', 0, '2026-01-02', '2026-01-02', '{ \"text\": \"Removed\" }'),
             ('streaming', 'active', NULL, 'assistant', 1, '2026-01-03', '2026-01-03', '{ \"text\": \"Incomplete\" }'),
             ('workflow', 'active', NULL, 'user', 0, '2026-01-04', '2026-01-04', '{ \"text\": \"Script\", \"workflowScript\": true }');",
    )
    .unwrap();
    drop(db);

    let detail = fixture.store().thread("active").unwrap().unwrap();
    let observed: Vec<_> = detail
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect();
    assert_eq!(observed, ["Legacy question", "Current copy"]);
}

#[test]
fn updates_the_live_sidebar_without_counting_held_runs_or_compaction() {
    let fixture = Fixture::new();
    let db = Connection::open(&fixture.0).unwrap();
    db.execute_batch(
        "INSERT INTO orchestration_v2_projection_runs VALUES
            ('live-run', 'active', 'running', 1, NULL, '2026-01-04T00:00:01Z', '{\"startedAt\":\"2026-01-04T00:00:02Z\"}'),
            ('held-run', 'active', 'queued', 2, NULL, '2026-01-04T00:00:03Z', '{\"queueHeld\":true}');
         INSERT INTO orchestration_v2_projection_messages VALUES
            ('prompt', 'active', 'live-run', 'user', 0, '2026-01-04T00:00:01Z', '2026-01-04T00:00:01Z', '{\"text\":\"Hello\"}'),
            ('compaction', 'active', NULL, 'user', 0, '2026-01-04T00:00:05Z', '2026-01-04T00:00:05Z',
             '{\"text\":\" /compact \",\"createdBy\":\"agent\",\"creationSource\":\"server\"}');",
    ).unwrap();
    let store = fixture.store();
    let shell = store.shell().unwrap();
    let summary = &shell.threads[0];
    assert_eq!(summary.status, "running");
    assert_eq!(
        summary.latest_user_message_at.as_deref(),
        Some("2026-01-04T00:00:01Z")
    );
    assert_eq!(
        summary.latest_run_requested_at.as_deref(),
        Some("2026-01-04T00:00:01Z")
    );
    assert_eq!(
        summary.latest_run_started_at.as_deref(),
        Some("2026-01-04T00:00:02Z")
    );
}

#[test]
fn inherits_only_message_turn_items_through_each_fork_boundary() {
    let fixture = Fixture::new();
    let db = Connection::open(&fixture.0).unwrap();
    db.execute_batch(
        "UPDATE orchestration_v2_projection_threads SET deleted_at = '2026-01-06'
         WHERE thread_id = 'active';
         INSERT INTO orchestration_v2_projection_threads (
             thread_id, project_id, title, payload_json, updated_at, created_at
         ) VALUES
             ('first-fork', 'project', 'First fork',
                 '{\"forkedFrom\":{\"type\":\"run\",\"threadId\":\"active\",\"runId\":\"retained\"}}', '2026-01-03', '2026-01-03'),
             ('nested-fork', 'project', 'Nested fork',
                 '{\"forkedFrom\":{\"type\":\"run\",\"threadId\":\"first-fork\",\"runId\":\"fork-turn\"}}', '2026-01-04', '2026-01-04');
         INSERT INTO orchestration_v2_projection_runs (run_id, thread_id, status, ordinal, completed_at) VALUES
             ('removed', 'active', 'rolled_back', 1, '2026-01-02'),
             ('retained', 'active', 'rolled_back', 2, '2026-01-05'),
             ('too-late', 'active', 'completed', 3, '2026-01-04'),
             ('fork-turn', 'first-fork', 'completed', 1, '2026-01-03');
         INSERT INTO orchestration_v2_projection_run_attempts VALUES ('retained', 'cancelled');
         INSERT INTO orchestration_v2_projection_turn_items (turn_item_id, thread_id, run_id, ordinal, type, updated_at, payload_json) VALUES
             ('t1', 'active', 'removed', 1, 'assistant_message', '2026-01-02',
                 '{\"messageId\":\"m1\",\"text\":\"Rolled back before the fork\"}'),
             ('t2', 'active', 'retained', 2, 'assistant_message', '2026-01-02',
                 '{\"messageId\":\"m2\",\"text\":\"Inherited reply\"}'),
             ('t3', 'active', 'retained', 3, 'user_message', '2026-01-02',
                 '{\"messageId\":\"m3\",\"text\":\"Cancelled input\",\"inputIntent\":\"queued_turn\"}'),
             ('t4', 'active', 'too-late', 4, 'assistant_message', '2026-01-04',
                 '{\"messageId\":\"m4\",\"text\":\"Outside the fork boundary\"}'),
             ('t5', 'first-fork', 'fork-turn', 1, 'user_message', '2026-01-03',
                 '{\"messageId\":\"m5\",\"text\":\"First fork question\"}');
         INSERT INTO orchestration_v2_projection_messages VALUES
             ('m6', 'nested-fork', NULL, 'user', 0, '2026-01-04', '2026-01-04',
                 '{\"text\":\"Nested fork question\"}');",
    )
    .unwrap();
    drop(db);

    let detail = fixture.store().thread("nested-fork").unwrap().unwrap();
    let observed: Vec<_> = detail
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect();
    assert_eq!(
        observed,
        [
            "Inherited reply",
            "First fork question",
            "Nested fork question"
        ]
    );
}
