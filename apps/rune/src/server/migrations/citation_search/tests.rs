use rusqlite::Connection;
use serde_json::json;

use super::*;

const QUOTE: &str = "[Assistant quote](t3-citation://v1/environment%2Flocal/thread%3Aquoted/quoted-answer?text=The+kittiwake%E2%80%99s+r%C3%A9sum%C3%A9&start=0&end=22&prefix=beacon01&suffix=&comment=from+the+harbor)";
const READABLE_QUOTE: &str = "The kittiwake’s résumé\nComment: from the harbor";

fn fixture(indexed: bool) -> Connection {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_messages (
           message_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, role TEXT NOT NULL,
           streaming INTEGER NOT NULL, payload_json TEXT NOT NULL
         );
         CREATE TABLE orchestration_v2_projection_threads (
           thread_id TEXT PRIMARY KEY, payload_json TEXT NOT NULL
         );
         INSERT INTO orchestration_v2_projection_threads VALUES
           ('subagent', '{\"lineage\": {\"relationshipToParent\": \"subagent\"}}');",
    )
    .unwrap();
    if indexed {
        db.execute_batch(
            "CREATE VIRTUAL TABLE orchestration_v2_message_search USING fts5(
               text, content = '', contentless_delete = 1, tokenize = 'trigram remove_diacritics 1'
             );
             CREATE TRIGGER orchestration_v2_message_search_insert
             AFTER INSERT ON orchestration_v2_projection_messages
             BEGIN
               INSERT INTO orchestration_v2_message_search (rowid, text)
               VALUES (new.rowid, json_extract(new.payload_json, '$.text'));
             END;
             CREATE TRIGGER orchestration_v2_message_search_update
             AFTER UPDATE ON orchestration_v2_projection_messages BEGIN SELECT 1; END;
             CREATE TRIGGER orchestration_v2_message_search_delete
             AFTER DELETE ON orchestration_v2_projection_messages BEGIN SELECT 1; END;",
        )
        .unwrap();
    }
    if !indexed {
        let transaction = db.transaction().unwrap();
        migrate_74(&transaction).unwrap();
        transaction.commit().unwrap();
    }
    db
}

fn insert(db: &Connection, id: &str, thread: &str, streaming: bool, payload: serde_json::Value) {
    db.execute(
        "INSERT INTO orchestration_v2_projection_messages VALUES (?1, ?2, 'user', ?3, ?4)",
        params![id, thread, streaming, payload.to_string()],
    )
    .unwrap();
}

fn indexed(db: &Connection, query: &str) -> Vec<String> {
    db.prepare(
        "SELECT messages.message_id FROM orchestration_v2_message_search AS search
         JOIN orchestration_v2_projection_messages AS messages ON messages.rowid = search.rowid
         WHERE orchestration_v2_message_search MATCH ?1 ORDER BY messages.message_id",
    )
    .unwrap()
    .query_map([query], |row| row.get(0))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

#[test]
fn migration_74_preserves_the_old_index_and_filters_new_attachment_placeholders() {
    let mut db = fixture(true);
    insert(
        &db,
        "legacy",
        "thread",
        false,
        json!({"text": ATTACHMENT_PLACEHOLDER}),
    );
    let tx = db.transaction().unwrap();
    migrate_74(&tx).unwrap();
    tx.commit().unwrap();
    insert(
        &db,
        "attachment",
        "thread",
        false,
        json!({
            "text": format!("uLtrAtHink:\u{feff}\r\n{ATTACHMENT_PLACEHOLDER}")
        }),
    );
    insert(
        &db,
        "subagent",
        "subagent",
        false,
        json!({"text": "search phrase"}),
    );
    insert(
        &db,
        "generated",
        "thread",
        false,
        json!({"text": "search phrase", "createdBy": "service"}),
    );
    insert(
        &db,
        "streaming",
        "thread",
        true,
        json!({"text": "search phrase"}),
    );
    insert(
        &db,
        "saved",
        "thread",
        false,
        json!({"text": "search phrase"}),
    );
    assert_eq!(indexed(&db, "\"attached one\""), ["legacy"]);
    assert_eq!(indexed(&db, "\"search phrase\""), ["saved"]);

    db.execute("UPDATE orchestration_v2_projection_messages SET streaming = 0 WHERE message_id = 'streaming'", []).unwrap();
    assert_eq!(indexed(&db, "\"search phrase\""), ["saved", "streaming"]);
    db.execute(
        "DELETE FROM orchestration_v2_projection_messages WHERE message_id = 'saved'",
        [],
    )
    .unwrap();
    assert_eq!(indexed(&db, "\"search phrase\""), ["streaming"]);
}

#[test]
fn migration_81_backfills_readable_citations_across_pages_and_replaces_index_entries() {
    let mut db = fixture(true);
    let tx = db.transaction().unwrap();
    migrate_74(&tx).unwrap();
    tx.commit().unwrap();
    let message = format!("{QUOTE} [\\_\\_init\\_\\_.py](file:///init.py)");
    insert(&db, "quoted", "thread", false, json!({"text": message}));
    insert(
        &db,
        "plain",
        "thread",
        false,
        json!({"text": "Unchanged text"}),
    );
    insert(
        &db,
        "invalid",
        "thread",
        false,
        json!({"text": "[Assistant quote](t3-citation://v1/not-valid)"}),
    );
    for i in 0..501 {
        insert(
            &db,
            &format!("page-{i:03}"),
            "thread",
            false,
            json!({"text": QUOTE}),
        );
    }
    let tx = db.transaction().unwrap();
    migrate_81(&tx).unwrap();
    tx.commit().unwrap();

    let row = |id: &str| -> (Option<String>, String) {
        db.query_row(
            "SELECT search_text, json_extract(payload_json, '$.text')
             FROM orchestration_v2_projection_messages WHERE message_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(
        row("quoted"),
        (
            Some(format!("{READABLE_QUOTE} [__init__.py](file:///init.py)")),
            message
        )
    );
    assert_eq!(row("plain").0, None);
    assert_eq!(row("invalid").0, None);
    assert_eq!(row("page-500").0.as_deref(), Some(READABLE_QUOTE));
    assert!(indexed(&db, "\"beacon01\"").is_empty());
    assert_eq!(indexed(&db, "\"kittiwake's résumé\"").len(), 502);
    assert_eq!(indexed(&db, "\"__init__.py\""), ["quoted"]);
}

#[test]
fn migration_81_backfills_citations_without_an_fts_index() {
    let mut db = fixture(false);
    insert(&db, "quoted", "thread", false, json!({"text": QUOTE}));
    db.execute(
        "INSERT INTO orchestration_v2_projection_messages VALUES ('legacy', 'thread', 'user', 0, ?1)",
        [format!(r#"{{"text":"\ud800{QUOTE}"}}"#)],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    migrate_81(&tx).unwrap();
    tx.commit().unwrap();
    let readable = db
        .prepare("SELECT search_text FROM orchestration_v2_projection_messages ORDER BY message_id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        readable,
        [format!("���{READABLE_QUOTE}"), READABLE_QUOTE.to_owned()]
    );
}
