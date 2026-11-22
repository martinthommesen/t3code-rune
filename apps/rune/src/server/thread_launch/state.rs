use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

pub(super) struct Receipt {
    pub(super) thread_id: String,
    pub(super) command_type: String,
    pub(super) accepted: bool,
}

pub(super) struct Thread {
    pub(super) project_id: String,
    pub(super) active: bool,
    pub(super) empty: bool,
    pub(super) worktree_path: Option<PathBuf>,
    pub(super) branch: Option<String>,
}

pub(super) fn open(path: &Path) -> rusqlite::Result<Connection> {
    let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    database.busy_timeout(Duration::from_secs(2))?;
    Ok(database)
}

pub(super) fn open_writer(path: &Path) -> rusqlite::Result<Connection> {
    let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    database.busy_timeout(Duration::from_secs(2))?;
    database.pragma_update(None, "foreign_keys", true)?;
    Ok(database)
}

pub(super) fn project_workspace(
    database: &Connection,
    project_id: &str,
) -> rusqlite::Result<Option<PathBuf>> {
    database
        .query_row(
            "SELECT workspace_root FROM projection_projects
             WHERE project_id = ?1 AND deleted_at IS NULL",
            [project_id],
            |row| row.get::<_, String>(0).map(PathBuf::from),
        )
        .optional()
}

pub(super) fn receipt(
    database: &Connection,
    command_id: &str,
) -> rusqlite::Result<Option<Receipt>> {
    database
        .query_row(
            "SELECT aggregate_id, command_type, status FROM orchestration_command_receipts
             WHERE aggregate_kind = 'thread' AND command_id = ?1",
            [command_id],
            |row| {
                Ok(Receipt {
                    thread_id: row.get(0)?,
                    command_type: row.get(1)?,
                    accepted: row.get::<_, String>(2)? == "accepted",
                })
            },
        )
        .optional()
}

pub(super) fn record_receipt(
    transaction: &Transaction<'_>,
    command_id: &str,
    thread_id: &str,
    command_type: &str,
    sequence: u64,
    accepted_at: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    let status = if error.is_some() {
        "rejected"
    } else {
        "accepted"
    };
    transaction.execute(
        "INSERT INTO orchestration_command_receipts (
            command_id, aggregate_kind, aggregate_id, command_type, accepted_at,
            result_sequence, status, error
         ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            command_id,
            thread_id,
            command_type,
            accepted_at,
            sequence,
            status,
            error
        ],
    )?;
    Ok(())
}

pub(super) fn thread(database: &Connection, thread_id: &str) -> rusqlite::Result<Option<Thread>> {
    database
        .query_row(
            "SELECT thread.project_id,
                    thread.archived_at IS NULL AND thread.deleted_at IS NULL,
                    NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_messages
                      WHERE thread_id = thread.thread_id
                    ) AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_runs
                      WHERE thread_id = thread.thread_id
                    ),
                    json_extract(thread.payload_json, '$.worktreePath'),
                    json_extract(thread.payload_json, '$.branch')
             FROM orchestration_v2_projection_threads AS thread WHERE thread.thread_id = ?1",
            [thread_id],
            |row| {
                Ok(Thread {
                    project_id: row.get(0)?,
                    active: row.get(1)?,
                    empty: row.get(2)?,
                    worktree_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    branch: row.get(4)?,
                })
            },
        )
        .optional()
}

pub(super) fn preparing_strategy(
    database: &Connection,
    thread_id: &str,
    run_id: &str,
) -> rusqlite::Result<Option<Option<super::WorkspaceStrategy>>> {
    database
        .query_row(
            "SELECT json_extract(payload_json, '$.workspacePreparation')
         FROM orchestration_v2_projection_runs
         WHERE thread_id = ?1 AND run_id = ?2 AND status = 'preparing'",
            [thread_id, run_id],
            |row| {
                row.get::<_, Option<String>>(0)?
                    .map(|encoded| {
                        serde_json::from_str(&encoded).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })
                    })
                    .transpose()
            },
        )
        .optional()
}
