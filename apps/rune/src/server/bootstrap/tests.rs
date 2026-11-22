use std::fs;
use std::path::PathBuf;

use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("t3-bootstrap-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn path(&self) -> PathBuf {
        self.0.join("statev2.sqlite")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fresh_database_has_the_current_migration_ledger_and_empty_projections() {
    let fixture = Fixture::new();
    let path = fixture.path();
    initialize(&path).unwrap();
    let database = Connection::open(&path).unwrap();
    let (count, head, head_label): (u32, u32, String) = database
        .query_row(
            "SELECT count(*), max(migration_id),
                (SELECT name FROM effect_sql_migrations ORDER BY migration_id DESC LIMIT 1)
         FROM effect_sql_migrations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (count, head, head_label.as_str()),
        (105, 105, "TaskFolders")
    );
    let (version, last_sequence, updated_at): (u32, u32, String) = database
        .query_row(
            "SELECT schema_version, last_sequence, updated_at
         FROM orchestration_v2_projection_metadata WHERE projection_name = 'thread-projections'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((version, last_sequence), (1, 0));
    assert_eq!(updated_at.len(), 24);
    assert!(updated_at.ends_with('Z'));
    let empty: bool = database
        .query_row(
            "SELECT (SELECT count(*) FROM projection_projects) = 0
            AND (SELECT count(*) FROM orchestration_v2_projection_threads) = 0
            AND (SELECT count(*) FROM orchestration_events) = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(empty);
    let integrity: String = database
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let auto_vacuum: u32 = database
        .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .unwrap();
    let journal: String = database
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!((auto_vacuum, journal.as_str()), (2, "wal"));
    drop(database);

    let previous = fs::read(&path).unwrap();
    initialize(&path).unwrap();
    assert_eq!(fs::read(path).unwrap(), previous);
}

#[test]
fn bootstrap_leaves_an_existing_database_to_the_upgrade_path() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let database = Connection::open(&path).unwrap();
    database
        .execute_batch(
            "CREATE TABLE effect_sql_migrations (migration_id INTEGER, name TEXT);
         INSERT INTO effect_sql_migrations VALUES (69, 'ThreadMessageSearch');
         CREATE TABLE existing_data (title TEXT);
         INSERT INTO existing_data VALUES ('Existing project');",
        )
        .unwrap();
    drop(database);
    let prior = fs::read(&path).unwrap();
    initialize(&path).unwrap();
    assert_eq!(fs::read(path).unwrap(), prior);
}
