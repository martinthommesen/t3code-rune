use std::fs;
use std::path::PathBuf;

use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("t3-migrations-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("statev2.sqlite"),
            include_bytes!("../../../../../scripts/fixtures/desktop-launch/statev2.sqlite"),
        )
        .unwrap();
        Self(directory)
    }

    fn path(&self) -> PathBuf {
        self.0.join("statev2.sqlite")
    }

    fn connection(&self) -> Connection {
        Connection::open(self.path()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ids(database: &Connection, query: &str) -> Vec<String> {
    database
        .prepare(query)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn upgrades_the_packaged_database_with_all_canonical_receipts() {
    let fixture = Fixture::new();
    let projects = "SELECT project_id FROM projection_projects ORDER BY project_id";
    let threads = "SELECT thread_id FROM orchestration_v2_projection_threads ORDER BY thread_id";
    let messages =
        "SELECT message_id FROM orchestration_v2_projection_messages ORDER BY message_id";
    let before = fixture.connection();
    assert_eq!(validated_head(&before).unwrap(), 69);
    let identifiers = [projects, threads, messages].map(|query| ids(&before, query));
    drop(before);

    migrate(&fixture.path()).unwrap();
    migrate(&fixture.path()).unwrap();
    let after = fixture.connection();
    assert_eq!(validated_head(&after).unwrap(), 105);
    assert_eq!(
        [projects, threads, messages].map(|query| ids(&after, query)),
        identifiers
    );
    assert_eq!(ids(&after, "PRAGMA integrity_check"), ["ok"]);
    let columns = ids(
        &after,
        "SELECT name FROM pragma_table_info('projection_projects')",
    );
    for column in [
        "pinned_at",
        "archived_at",
        "has_custom_title",
        "tracked_folders_json",
    ] {
        assert!(columns.iter().any(|stored| stored == column));
    }
}

#[test]
fn failed_receipt_rolls_back_schema_changes_and_later_start_resumes() {
    let fixture = Fixture::new();
    fixture
        .connection()
        .execute_batch(
            "CREATE TRIGGER refuse_71 BEFORE INSERT ON effect_sql_migrations
         WHEN NEW.migration_id = 71 BEGIN SELECT RAISE(ABORT, 'cannot write receipt'); END;",
        )
        .unwrap();

    assert!(matches!(
        migrate(&fixture.path()),
        Err(MigrationError::Apply { id: 71, .. })
    ));
    let interrupted = fixture.connection();
    assert_eq!(validated_head(&interrupted).unwrap(), 70);
    assert!(
        !ids(
            &interrupted,
            "SELECT name FROM pragma_table_info('peer_environment_links')"
        )
        .iter()
        .any(|column| column == "fenced")
    );
    interrupted.execute_batch("DROP TRIGGER refuse_71").unwrap();

    migrate(&fixture.path()).unwrap();
    assert_eq!(validated_head(&interrupted).unwrap(), 105);
    assert!(
        ids(
            &interrupted,
            "SELECT name FROM pragma_table_info('peer_environment_links')"
        )
        .iter()
        .any(|column| column == "fenced")
    );
}

#[test]
fn rejects_incompatible_migration_history_before_writing() {
    let fixture = Fixture::new();
    let database = fixture.connection();
    database
        .execute(
            "UPDATE effect_sql_migrations SET name = 'ExperimentalSearch' WHERE migration_id = 69",
            [],
        )
        .unwrap();
    assert!(matches!(
        migrate(&fixture.path()),
        Err(MigrationError::InvalidReceipt { id: 69, .. })
    ));
    assert!(
        ids(
            &database,
            "SELECT name FROM sqlite_schema WHERE name = 'peer_environment_links'"
        )
        .is_empty()
    );
}
