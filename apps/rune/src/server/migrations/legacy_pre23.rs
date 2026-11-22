use rusqlite::Transaction;

use super::{Migration, add_column};

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration::sql(
        16,
        "CanonicalizeModelSelections",
        include_str!("legacy_pre23/model_selections.sql"),
    ),
    Migration::scripted(17, "ProjectionThreadsArchivedAt", |db| {
        add_column(db, "projection_threads", "archived_at", "TEXT")
    }),
    Migration::sql(
        18,
        "ProjectionThreadsArchivedAtIndex",
        include_str!("legacy_pre23/thread_archive_index.sql"),
    ),
    Migration::sql(
        19,
        "ProjectionSnapshotLookupIndexes",
        include_str!("legacy_pre23/snapshot_lookup_indexes.sql"),
    ),
    Migration::sql(
        20,
        "AuthAccessManagement",
        include_str!("legacy_pre23/role_auth_tables.sql"),
    ),
    Migration::scripted(21, "AuthSessionClientMetadata", add_auth_client_metadata),
    Migration::scripted(22, "AuthSessionLastConnectedAt", |db| {
        add_column(db, "auth_sessions", "last_connected_at", "TEXT")
    }),
    Migration::scripted(23, "ProjectionThreadShellSummary", add_thread_shell_summary),
];

fn add_auth_client_metadata(db: &Transaction<'_>) -> rusqlite::Result<()> {
    add_column(db, "auth_pairing_links", "label", "TEXT")?;
    for (column, definition) in [
        ("client_label", "TEXT"),
        ("client_ip_address", "TEXT"),
        ("client_user_agent", "TEXT"),
        ("client_device_type", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("client_os", "TEXT"),
        ("client_browser", "TEXT"),
    ] {
        add_column(db, "auth_sessions", column, definition)?;
    }
    Ok(())
}

fn add_thread_shell_summary(db: &Transaction<'_>) -> rusqlite::Result<()> {
    for statement in [
        "ALTER TABLE projection_threads ADD COLUMN latest_user_message_at TEXT",
        "ALTER TABLE projection_threads ADD COLUMN pending_approval_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE projection_threads ADD COLUMN pending_user_input_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE projection_threads ADD COLUMN has_actionable_proposed_plan INTEGER NOT NULL DEFAULT 0",
    ] {
        let _ = db.execute_batch(statement);
    }
    Ok(())
}
