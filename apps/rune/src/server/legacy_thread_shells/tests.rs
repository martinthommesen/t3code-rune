use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use super::*;
use crate::server::{bootstrap, migrations};

#[test]
fn preview_import_is_atomic_and_can_be_retried_without_renumbering_transcript() {
    let profile = tempfile::tempdir().unwrap();
    let path = profile.path().join("statev2.sqlite");
    bootstrap::initialize(&path).unwrap();
    migrations::migrate(&path).unwrap();
    let mut db = Connection::open(&path).unwrap();
    db.execute_batch(
        "INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json, created_at, updated_at)
         VALUES ('project', 'Project', '/workspace/example', '[]', '2026-01-01', '2026-01-01');
         INSERT INTO projection_threads (
             thread_id, project_id, title, model_selection_json, runtime_mode,
             created_at, updated_at, archived_at, interaction_mode
         ) VALUES (
             'legacy', 'project', '  Request ', '{\"provider\":\"codex\",\"model\":\" gpt-5.4 \"}',
             'auto', '2026-01-01T01:00:00+01:00', '2026-01-02T02:00:00+01:00', 'stale', 'plan'
         );
         INSERT INTO projection_thread_messages (
             message_id, thread_id, role, text, is_streaming, created_at, updated_at
         ) VALUES
             ('old', 'legacy', 'user', 'Original question', 0, '2026-01-01', '2026-01-01'),
             ('latest-user', 'legacy', 'user', 'Followup', 0, '2026-01-02', '2026-01-02'),
             ('stream', 'legacy', 'assistant', 'Partial', 1, '2026-01-03', '2026-01-03'),
             ('system', 'legacy', 'system', 'Invisible', 0, '2026-01-04', '2026-01-04');
         CREATE TRIGGER reject_shell BEFORE INSERT ON orchestration_events
         WHEN NEW.event_type = 'thread.metadata-updated' BEGIN
             SELECT RAISE(ABORT, 'reject shell');
         END;",
    ).unwrap();
    let imported_at = OffsetDateTime::parse("2026-01-05T00:00:00.125Z", &Rfc3339).unwrap();
    assert!(reconcile_at(&mut db, imported_at).is_err());
    for check in [
        "SELECT COUNT(*) FROM orchestration_events WHERE stream_id = 'legacy'",
        "SELECT COUNT(*) FROM orchestration_v2_projection_turn_items",
        "SELECT COUNT(*) FROM orchestration_v2_turn_item_positions",
        "SELECT COUNT(*) FROM orchestration_v2_legacy_imports",
    ] {
        assert_eq!(
            db.query_row(check, [], |row| row.get::<_, i64>(0)).unwrap(),
            0
        );
    }
    db.execute_batch("DROP TRIGGER reject_shell").unwrap();
    assert_eq!(
        reconcile_at(&mut db, imported_at).unwrap(),
        Summary {
            imported_thread_count: 1,
            imported_message_count: 2,
        }
    );
    assert_eq!(
        reconcile_at(&mut db, imported_at).unwrap(),
        Summary::default()
    );
    let events: Vec<(i64, String)> = db
        .prepare(
            "SELECT stream_version, event_type FROM orchestration_events WHERE stream_id = 'legacy'
         ORDER BY sequence",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        events,
        [
            "thread.created",
            "message.updated",
            "turn-item.updated",
            "message.updated",
            "turn-item.updated",
            "thread.metadata-updated"
        ]
        .map(str::to_owned)
        .into_iter()
        .enumerate()
        .map(|(version, event)| (version as i64, event))
        .collect::<Vec<_>>()
    );
    let previews: Vec<(i64, String)> = db.prepare(
        "SELECT ordinal, status FROM orchestration_v2_projection_turn_items WHERE thread_id = 'legacy'
         ORDER BY ordinal"
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(
        previews,
        [(2, "completed".to_owned()), (3, "interrupted".to_owned())]
    );
    let payload: String = db.query_row(
        "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = 'legacy'",
        [], |row| row.get(0),
    ).unwrap();
    let shell: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(shell["title"], "Request");
    assert_eq!(
        shell["modelSelection"],
        json!({"instanceId":"codex","model":"gpt-5.4"})
    );
    assert_eq!(shell["createdAt"], "2026-01-01T00:00:00.000Z");
    assert_eq!(shell["updatedAt"], "2026-01-02T01:00:00.000Z");
    assert_eq!(shell["archivedAt"], shell["updatedAt"]);
    let marker: (String, String, Option<String>, i64) = db.query_row(
        "SELECT source_updated_at, shell_imported_at, transcript_imported_at, imported_message_count
         FROM orchestration_v2_legacy_imports WHERE thread_id = 'legacy'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(
        marker,
        (
            "2026-01-02T02:00:00+01:00".to_owned(),
            "2026-01-05T00:00:00.125Z".to_owned(),
            None,
            2
        )
    );
}

#[test]
fn repair_preserves_existing_choices_and_appends_once_without_reimporting() {
    let profile = tempfile::tempdir().unwrap();
    let path = profile.path().join("statev2.sqlite");
    bootstrap::initialize(&path).unwrap();
    migrations::migrate(&path).unwrap();
    let mut db = Connection::open(&path).unwrap();
    db.execute_batch(
        "INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json, created_at, updated_at)
         VALUES ('project', 'Project', '/workspace/example', '[]', '2026-01-01', '2026-01-01');
         INSERT INTO projection_threads (
             thread_id, project_id, title, runtime_mode, interaction_mode, created_at, updated_at,
             pinned_at, pin_order_key, linked_pull_request_json
         ) VALUES (
             'legacy', 'project', 'Legacy title', 'full-access', 'default', '2026-01-01', '2026-01-02',
             '2026-01-03', 'legacy-order', '{\"projectId\":\"project\",\"repository\":\"owner/repo\",\"number\":1,\"url\":\"https://github.com/owner/repo/pull/1\"}'
         );",
    ).unwrap();
    let imported = OffsetDateTime::parse("2026-01-05T00:00:00.125Z", &Rfc3339).unwrap();
    let repaired = OffsetDateTime::parse("2026-01-06T00:00:00.125Z", &Rfc3339).unwrap();
    assert_eq!(
        reconcile_at(&mut db, imported)
            .unwrap()
            .imported_thread_count,
        1
    );
    db.execute_batch(
        "UPDATE orchestration_v2_projection_threads
         SET payload_json = json_remove(json_set(payload_json,
             '$.title', 'Edited title', '$.pinnedAt', NULL, '$.linkedPullRequest', NULL,
             '$.providerInstanceId', 'alternate', '$.activeProviderThreadId', 'binding',
             '$.lastVisitedAt', '2026-01-04T00:00:00Z', '$.tags', json('[\"important\"]')),
             '$.pinOrderKey', '$.pullRequests', '$.snoozedUntil')
         WHERE thread_id = 'legacy';",
    )
    .unwrap();
    assert_eq!(
        reconcile_at(&mut db, repaired).unwrap(),
        Summary {
            imported_thread_count: 1,
            imported_message_count: 0,
        }
    );
    assert_eq!(reconcile_at(&mut db, repaired).unwrap(), Summary::default());
    let row: (i64, String, String, String, String) = db
        .query_row(
            "SELECT stream_version, event_id, occurred_at, payload_json, metadata_json
          FROM orchestration_events WHERE stream_id = 'legacy' ORDER BY sequence DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, 2);
    assert!(
        row.1
            .starts_with("migration:v1:thread:legacy:metadata-repair:")
    );
    assert_eq!(row.2, "2026-01-06T00:00:00.125Z");
    let payload: Value = serde_json::from_str(&row.3).unwrap();
    let meta: Value = serde_json::from_str(&row.4).unwrap();
    assert_eq!(meta, json!({"providerInstanceId":"alternate"}));
    assert_eq!(payload["title"], "Edited title");
    assert_eq!(payload["pinnedAt"], Value::Null);
    assert_eq!(payload["pinOrderKey"], "legacy-order");
    assert!(payload.as_object().unwrap().contains_key("snoozedUntil"));
    assert_eq!(payload["linkedPullRequest"], Value::Null);
    assert_eq!(payload["pullRequests"], json!([]));
    assert_eq!(payload["activeProviderThreadId"], "binding");
    assert_eq!(payload["lastVisitedAt"], "2026-01-04T00:00:00.000Z");
    assert_eq!(payload["tags"], json!(["important"]));
    let marker: (String, String, Option<String>, i64) = db.query_row(
        "SELECT source_updated_at, shell_imported_at, transcript_imported_at, imported_message_count
         FROM orchestration_v2_legacy_imports WHERE thread_id = 'legacy'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(
        marker,
        (
            "2026-01-02".to_owned(),
            "2026-01-05T00:00:00.125Z".to_owned(),
            None,
            0
        )
    );
}
