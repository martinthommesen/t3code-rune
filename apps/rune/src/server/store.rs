use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};
use serde::Serialize;
use serde_json::{Value, json};

mod command_detail;
mod fork;
mod older_settled;
mod shell_delta;
mod shell_snapshot;
mod thread_projection;

pub(super) use command_detail::CommandDetail;
pub(super) use older_settled::Request as OlderSettledRequest;
pub(super) use shell_delta::ShellDeltaBatch;
pub(super) use thread_projection::{
    append_import_event, decode_legacy_attachment, import_shell_previews, parse_timestamp,
};

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    projects: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shell {
    pub snapshot_sequence: u64,
    pub projects: Vec<Project>,
    pub threads: Vec<ThreadSummary>,
    pub archived_threads: Vec<ThreadSummary>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub title: String,
    pub workspace_root: String,
    pub archived_at: Option<String>,
    pub default_model_selection: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub archived_at: Option<String>,
    pub worktree_path: Option<String>,
    pub updated_at: String,
    pub status: String,
    pub latest_user_message_at: Option<String>,
    pub latest_run_requested_at: Option<String>,
    pub latest_run_started_at: Option<String>,
    pub latest_run_completed_at: Option<String>,
    pub provider_instance_id: Option<String>,
    pub model_selection: Option<Value>,
    pub last_visited_at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetail {
    pub snapshot_sequence: u64,
    pub thread: ThreadSummary,
    pub messages: Vec<Message>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub text: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

const PROJECTS: &str = "
    SELECT project_id, title, workspace_root, archived_at, default_model_selection_json
    FROM projection_projects
    WHERE deleted_at IS NULL
    ORDER BY created_at, project_id";

const PROJECTS_BEFORE_ARCHIVING: &str = "
    SELECT project_id, title, workspace_root, NULL, default_model_selection_json
    FROM projection_projects
    WHERE deleted_at IS NULL
    ORDER BY created_at, project_id";

const THREADS: &str = include_str!("store/thread_summaries.sql");

const SHELL_SEQUENCE: &str = "
    SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events
       INDEXED BY idx_orchestration_events_application_high_water
    WHERE aggregate_kind = 'project'
       OR (application_event_version = 2 AND aggregate_kind = 'thread')";

const THREAD_SEQUENCE: &str = "
    SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events
    WHERE application_event_version = 2 AND aggregate_kind = 'thread' AND stream_id = ?1";

// Pending legacy hydration can have committed part of the transcript already.
const THREAD_MESSAGES: &str = "
    SELECT message.message_id, message.role,
           json_extract(message.payload_json, '$.text') AS text,
           message.created_at, message.updated_at,
           json_extract(message.payload_json, '$.createdBy')
    FROM orchestration_v2_projection_messages AS message
    LEFT JOIN orchestration_v2_projection_runs AS run ON run.run_id = message.run_id
    WHERE message.thread_id = ?1
      AND message.role IN ('user', 'assistant') AND message.streaming = 0
      AND run.status IS NOT 'rolled_back'
      AND json_extract(message.payload_json, '$.workflowScript') IS NOT 1
      AND json_extract(message.payload_json, '$.idleCompaction') IS NULL
      AND NOT (
          message.role = 'user' AND (run.status IS 'queued' OR run.status IS 'cancelled')
          AND EXISTS (
              SELECT 1 FROM orchestration_v2_projection_turn_items AS item
              WHERE item.run_id = run.run_id AND item.type = 'user_message'
                AND json_extract(item.payload_json, '$.messageId') = message.message_id
                AND json_extract(item.payload_json, '$.inputIntent') IS 'queued_turn'
          )
      )
    UNION ALL
    SELECT message.message_id, message.role, message.text, message.created_at, message.updated_at, NULL
    FROM projection_thread_messages AS message
    WHERE message.thread_id = ?1 AND message.role IN ('user', 'assistant')
      AND EXISTS (
          SELECT 1 FROM orchestration_v2_legacy_imports AS legacy
          WHERE legacy.thread_id = ?1 AND legacy.transcript_imported_at IS NULL
      )
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_messages AS imported
          WHERE imported.message_id = message.message_id
      )
    ORDER BY created_at, message_id";

impl Store {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let database = open_readonly(&path)?;
        let has_project_archiving: bool = database.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('projection_projects') WHERE name = 'archived_at')",
            [],
            |row| row.get(0),
        )?;
        let projects = if has_project_archiving {
            PROJECTS
        } else {
            PROJECTS_BEFORE_ARCHIVING
        };
        for query in [
            projects,
            THREADS,
            SHELL_SEQUENCE,
            THREAD_SEQUENCE,
            THREAD_MESSAGES,
        ] {
            database.prepare(query)?;
        }
        fork::verify_schema(&database)?;
        Ok(Self { path, projects })
    }

    pub fn shell(&self) -> rusqlite::Result<Shell> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let snapshot_sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let projects = read
            .prepare(self.projects)?
            .query_map([], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    workspace_root: row.get(2)?,
                    archived_at: row.get(3)?,
                    default_model_selection: model_selection(row, 4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let threads = read
            .prepare(&format!(
                "{THREADS} ORDER BY thread.updated_at DESC, thread.thread_id"
            ))?
            .query_map([], thread_summary)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let (threads, archived_threads) = threads
            .into_iter()
            .partition(|thread| thread.archived_at.is_none());
        read.commit()?;
        Ok(Shell {
            snapshot_sequence,
            projects,
            threads,
            archived_threads,
        })
    }

    pub(super) fn live_subscriber_shell_snapshot(
        &self,
        omit_idle_subagents: bool,
        omit_older_settled: bool,
    ) -> rusqlite::Result<(Value, HashSet<String>)> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let snapshot = shell_snapshot::read_subscriber(
            &read,
            sequence,
            omit_idle_subagents,
            omit_older_settled,
        )?;
        read.commit()?;
        Ok(snapshot)
    }

    pub(super) fn live_project_metadata(&self) -> rusqlite::Result<(u64, Vec<Value>)> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let projects = shell_snapshot::read_projects(&read)?;
        read.commit()?;
        Ok((sequence, projects))
    }

    pub(super) fn can_resume_shell(&self, cursor: u64) -> rusqlite::Result<bool> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let resumable = shell_delta::can_resume(&read, cursor, sequence)?;
        read.commit()?;
        Ok(resumable)
    }

    pub(super) fn live_archived_shell_snapshot(&self) -> rusqlite::Result<Value> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let snapshot = shell_snapshot::read_archive(&read, sequence)?;
        read.commit()?;
        Ok(snapshot)
    }

    pub(super) fn thread_family(&self, thread_id: &str) -> rusqlite::Result<Value> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence: u64 = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let threads = shell_snapshot::read_family_threads(&read, thread_id)?;
        read.commit()?;
        Ok(json!({"snapshotSequence": sequence, "threads": threads}))
    }

    pub(super) fn worktree_thread_ids(&self, path: &str) -> rusqlite::Result<Vec<String>> {
        let database = open_readonly(&self.path)?;
        shell_snapshot::read_worktree_thread_ids(&database, path)
    }

    pub(super) fn older_settled_threads(
        &self,
        input: &OlderSettledRequest,
    ) -> rusqlite::Result<Value> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let page = older_settled::read(&read, sequence, input)?;
        read.commit()?;
        Ok(page)
    }

    /// Read canonical changes and their durable cursor in one SQLite snapshot.
    pub(super) fn live_shell_deltas(
        &self,
        after_sequence: u64,
        known_subagents: Option<&HashSet<String>>,
    ) -> rusqlite::Result<Option<ShellDeltaBatch>> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let sequence = read.query_row(SHELL_SEQUENCE, [], |row| row.get(0))?;
        let batch = shell_delta::read(&read, after_sequence, sequence, known_subagents)?;
        read.commit()?;
        Ok(batch)
    }

    pub fn thread(&self, thread_id: &str) -> rusqlite::Result<Option<ThreadDetail>> {
        let mut database = open_readonly(&self.path)?;
        let read = database.transaction()?;
        let Some(thread) = read
            .query_row(
                &format!("{THREADS} AND thread.thread_id = ?1"),
                [thread_id],
                thread_summary,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let snapshot_sequence = read.query_row(THREAD_SEQUENCE, [thread_id], |row| row.get(0))?;
        let mut messages = fork::read_messages(&read, thread_id)?;
        let mut query = read.prepare(THREAD_MESSAGES)?;
        for row in query.query_map([thread_id], message)? {
            messages.push(row?);
        }
        drop(query);
        read.commit()?;
        Ok(Some(ThreadDetail {
            snapshot_sequence,
            thread,
            messages,
        }))
    }

    pub(super) fn live_thread_snapshot(&self, thread_id: &str) -> rusqlite::Result<Option<Value>> {
        let mut database =
            Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        database.busy_timeout(Duration::from_secs(2))?;
        database.pragma_update(None, "foreign_keys", true)?;
        thread_projection::hydrate_legacy(&mut database, thread_id)?;
        let read = database.transaction()?;
        let snapshot = thread_projection::snapshot_for_wire(&read, thread_id)?;
        read.commit()?;
        Ok(snapshot)
    }

    pub(super) fn terminal_workspace(
        &self,
        thread_id: &str,
    ) -> rusqlite::Result<Option<(PathBuf, Option<PathBuf>)>> {
        open_readonly(&self.path)?.query_row(
            "SELECT project.workspace_root, json_extract(thread.payload_json, '$.worktreePath')
             FROM orchestration_v2_projection_threads AS thread
             JOIN projection_projects AS project ON project.project_id = thread.project_id
             WHERE thread.thread_id = ?1 AND thread.deleted_at IS NULL AND project.deleted_at IS NULL",
            [thread_id],
            |row| {
                let workspace: String = row.get(0)?;
                let worktree: Option<String> = row.get(1)?;
                Ok((workspace.into(), worktree.map(PathBuf::from)))
            },
        ).optional()
    }
}

pub(super) fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    database.busy_timeout(Duration::from_secs(2))?;
    Ok(database)
}

fn thread_summary(row: &Row<'_>) -> rusqlite::Result<ThreadSummary> {
    Ok(ThreadSummary {
        id: row.get(0)?,
        project_id: row.get(1)?,
        title: row.get(2)?,
        archived_at: row.get(3)?,
        worktree_path: row.get(5)?,
        updated_at: row.get(6)?,
        status: row.get(4)?,
        latest_user_message_at: row.get(7)?,
        latest_run_requested_at: row.get(8)?,
        latest_run_started_at: row.get(9)?,
        latest_run_completed_at: row.get(10)?,
        provider_instance_id: row.get(11)?,
        model_selection: model_selection(row, 12)?,
        last_visited_at: row.get(13)?,
    })
}

fn model_selection(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<Value>> {
    let raw: Option<String> = row.get(column)?;
    Ok(raw.and_then(|json| serde_json::from_str(&json).ok()))
}

fn message(row: &Row<'_>) -> rusqlite::Result<Message> {
    let role = match row.get_ref(1)?.as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(Message {
        id: row.get(0)?,
        role,
        text: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        created_by: row.get(5)?,
    })
}

#[cfg(test)]
mod tests;
