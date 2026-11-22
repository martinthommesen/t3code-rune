use std::fs;
use std::path::PathBuf;

use super::*;
use crate::server::bootstrap;

#[test]
fn visibility_hides_rolled_back_runs_queued_inputs_and_unpaired_superseded_stops() {
    let runs = [
        json!({"id":"rolled-back","status":"rolled_back"}),
        json!({"id":"queued","status":"queued"}),
        json!({"id":"superseded-single","status":"running"}),
        json!({"id":"superseded-pair","status":"running"}),
    ];
    let attempts = [
        json!({"runId":"superseded-single","rootNodeId":"superseded-node","status":"superseded"}),
        json!({"runId":"superseded-pair","rootNodeId":"superseded-node","status":"superseded"}),
    ];
    let items = [
        json!({"id":"rolled-back-item","threadId":"chosen","type":"assistant_message","runId":"rolled-back"}),
        json!({"id":"queued-input","threadId":"chosen","type":"user_message","runId":"queued","inputIntent":"queued_turn"}),
        json!({"id":"legacy-result","threadId":"chosen","type":"run_interrupt_result","runId":"superseded-single","nodeId":"superseded-node"}),
        json!({"id":"keep-request","threadId":"chosen","type":"run_interrupt_request","runId":"superseded-pair"}),
        json!({"id":"keep-result","threadId":"chosen","type":"run_interrupt_result","runId":"superseded-pair","nodeId":"superseded-node"}),
        json!({"id":"keep-normal","threadId":"chosen","type":"assistant_message","runId":null}),
    ];
    let actual = timeline::visible(&runs, &attempts, &items).unwrap();
    let expected = [
        json!({"position":0,"visibility":"local","sourceThreadId":"chosen","sourceItemId":"keep-request","item":items[3]}),
        json!({"position":1,"visibility":"local","sourceThreadId":"chosen","sourceItemId":"keep-result","item":items[4]}),
        json!({"position":2,"visibility":"local","sourceThreadId":"chosen","sourceItemId":"keep-normal","item":items[5]}),
    ];
    assert_eq!(actual, expected);
}

#[test]
fn message_order_uses_earliest_item_ordinal_before_time() {
    let mut messages = [
        json!({"id":"created-first","createdAt":"2026-11-22T00:00:00.000Z"}),
        json!({"id":"without-item","createdAt":"2026-11-22T00:00:00.001Z"}),
        json!({"id":"shown-first","createdAt":"2026-11-22T00:00:00.002Z"}),
    ];
    let items = [
        json!({"type":"assistant_message","messageId":"shown-first","ordinal":10}),
        json!({"type":"user_message","messageId":"created-first","ordinal":2}),
        json!({"type":"assistant_message","messageId":"shown-first","ordinal":1}),
    ];
    timeline::sort_messages(&mut messages, &items).unwrap();
    assert_eq!(
        messages.map(|message| message["id"].as_str().unwrap().to_owned()),
        ["shown-first", "created-first", "without-item"]
    );
}

#[test]
fn fork_prefix_includes_later_rollbacks_and_inherited_markers() {
    let item = |id, run, intent| {
        json!({
            "id":id,"threadId":"source","type":"user_message","runId":run,"inputIntent":intent,
        })
    };
    let source = json!({
        "thread":{"id":"source","historyOrigin":"v1_import"},
        "runs":[
            {"id":"rewound-before","status":"rolled_back","ordinal":1,"completedAt":"2026-10-01T00:00:00.000Z"},
            {"id":"rewound-after","status":"rolled_back","ordinal":2,"completedAt":"2026-12-01T00:00:00.000Z"},
            {"id":"cancelled","status":"rolled_back","ordinal":3,"completedAt":"2026-12-01T00:00:00.000Z"},
            {"id":"later-run","status":"completed","ordinal":4},
        ],
        "attempts":[{"runId":"cancelled","rootNodeId":"cancelled-root","status":"cancelled"}],
        "turnItems":[
            item("imported", None, "turn_start"),
            item("exclude-earlier-rewind", Some("rewound-before"), "turn_start"),
            item("keep-later-rewind", Some("rewound-after"), "turn_start"),
            item("exclude-undelivered", Some("cancelled"), "queued_turn"),
            item("exclude-beyond-cutoff", Some("later-run"), "turn_start"),
        ],
        "visibleTurnItems":[
            {"sourceThreadId":"ancestor","sourceItemId":"old-item","visibility":"inherited","position":0,"item":{"threadId":"ancestor","type":"user_message"}},
            {"sourceThreadId":"ancestor","sourceItemId":"old-boundary","visibility":"synthetic","position":1,"item":{"threadId":"source","type":"fork"}},
        ],
    });
    let mut target = json!({
        "thread":{"id":"target","createdAt":"2026-11-01T00:00:00.000Z","forkedFrom":{"type":"run","threadId":"source","runId":"cancelled"}},
        "visibleTurnItems":[{"sourceThreadId":"target","sourceItemId":"own-item","visibility":"local","position":0,"item":{"threadId":"target","type":"assistant_message"}}],
    });
    inherited::merge(&mut target, &source).unwrap();
    let entries = target["visibleTurnItems"].as_array().unwrap();
    let observed = entries
        .iter()
        .map(|row| {
            (
                row["sourceItemId"].as_str().unwrap(),
                row["visibility"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        [
            ("old-item", "inherited"),
            ("old-boundary", "inherited"),
            ("imported", "inherited"),
            ("keep-later-rewind", "inherited"),
            ("turn-item:fork:target", "synthetic"),
            ("own-item", "local"),
        ]
    );
    assert!(
        entries
            .iter()
            .enumerate()
            .all(|(position, row)| row["position"] == position)
    );
}

#[test]
fn hydration_leaves_completed_or_unknown_imports_alone() {
    let mut database = Connection::open_in_memory().unwrap();
    database.execute_batch(
        "CREATE TABLE orchestration_v2_legacy_imports (
             thread_id TEXT PRIMARY KEY, transcript_imported_at TEXT
         );
         INSERT INTO orchestration_v2_legacy_imports VALUES ('complete', '2026-11-01T00:00:00.000Z');",
    ).unwrap();
    for thread in ["complete", "unknown"] {
        let result = hydrate_legacy(&mut database, thread).unwrap();
        assert_eq!(result.imported_thread_count, 0);
        assert_eq!(result.imported_message_count, 0);
    }
}

#[test]
fn pending_transcript_hydrates_and_reads_from_the_bootstrapped_store() {
    let fixture = Fixture::new();
    let path = fixture.0.join("statev2.sqlite");
    bootstrap::initialize(&path).unwrap();
    let mut database = Connection::open(&path).unwrap();
    let now = "2026-11-01T00:00:00.000Z";
    let thread = json!({
        "createdBy":"user","creationSource":"server","id":"legacy-thread","projectId":"project",
        "title":"Legacy thread","providerInstanceId":"codex",
        "modelSelection":{"instanceId":"codex","model":"gpt-5.4"},"runtimeMode":"full-access",
        "interactionMode":"default","branch":null,"worktreePath":null,
        "activeProviderThreadId":null,"historyOrigin":"v1_import",
        "lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"legacy-thread"},
        "forkedFrom":null,"createdAt":now,"updatedAt":now,"archivedAt":null,"deletedAt":null,
    });
    database
        .execute(
            "INSERT INTO orchestration_v2_projection_threads (
            thread_id, project_id, title, default_provider, provider_instance_id, runtime_mode,
            interaction_mode, created_at, updated_at, payload_json
         ) VALUES ('legacy-thread','project','Legacy thread','codex','codex','full-access',
            'default', ?1, ?1, ?2)",
            [now, &thread.to_string()],
        )
        .unwrap();
    database.execute(
        "INSERT INTO orchestration_v2_legacy_imports (thread_id, source_updated_at, shell_imported_at)
         VALUES ('legacy-thread', ?1, ?1)",
        [now],
    ).unwrap();
    let message_time = "2026-11-01T00:00:01.125Z";
    database
        .execute(
            "INSERT INTO projection_thread_messages (
            message_id, thread_id, role, text, is_streaming, created_at, updated_at
         ) VALUES ('answer', 'legacy-thread', 'assistant', 'Legacy answer', 1, ?1, ?1)",
            [message_time],
        )
        .unwrap();

    let pending_projection = read(&database, "legacy-thread").unwrap().unwrap();
    assert!(
        pending_projection["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let imported = hydrate_legacy(&mut database, "legacy-thread").unwrap();
    assert_eq!(imported.imported_thread_count, 1);
    assert_eq!(imported.imported_message_count, 1);
    let transaction = database.transaction().unwrap();
    let projection = read(&transaction, "legacy-thread").unwrap().unwrap();
    assert_eq!(projection["thread"]["updatedAt"], message_time);
    assert_eq!(projection["messages"][0]["text"], "Legacy answer");
    assert_eq!(projection["messages"][0]["streaming"], false);
    assert_eq!(
        projection["visibleTurnItems"][0]["item"]["status"],
        "interrupted"
    );
    let snapshot = snapshot_for_wire(&transaction, "legacy-thread")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["snapshotSequence"], 2);
    assert_eq!(snapshot["projection"], projection);
    transaction.commit().unwrap();

    let repeated = hydrate_legacy(&mut database, "legacy-thread").unwrap();
    assert_eq!(repeated.imported_thread_count, 0);
    assert_eq!(repeated.imported_message_count, 0);
    let events: String = database
        .query_row(
            "SELECT group_concat(event_type, ',') FROM (
            SELECT event_type FROM orchestration_events
            WHERE stream_id = 'legacy-thread' ORDER BY stream_version
         )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, "message.updated,turn-item.updated");
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("t3-thread-projection-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
