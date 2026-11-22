use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::{Value, json};
use time::OffsetDateTime;

mod create;
mod rename;
mod unarchive;
mod visit;
pub(super) use create::CreateThread;
pub(super) use create::{normalize_model_selection, trim as trim_js};

const SELECT_RECEIPT: &str = "
    SELECT aggregate_kind, aggregate_id, result_sequence, status, error
    FROM orchestration_command_receipts
    WHERE command_id = ?1";

const THREAD_EXISTS: &str = "
    SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_threads WHERE thread_id = ?1)";

const LATEST_THREAD_SEQUENCE: &str = "
    SELECT COALESCE(MAX(sequence), 0)
    FROM orchestration_events INDEXED BY idx_orchestration_events_agent_stream_sequence
    WHERE application_event_version = 2 AND aggregate_kind = 'thread' AND stream_id = ?1";

const APPEND_CREATED: &str = "
    INSERT INTO orchestration_events (
        event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
        command_id, causation_event_id, correlation_id, actor_kind, payload_json,
        metadata_json, application_event_version
    ) VALUES (
        ?1, 'thread', ?2,
        COALESCE((
            SELECT stream_version + 1 FROM orchestration_events
            WHERE aggregate_kind = 'thread' AND stream_id = ?2
            ORDER BY stream_version DESC LIMIT 1
        ), 0),
        'thread.created', ?3, ?4, NULL, ?4, 'server', ?5, ?6, 2
    ) RETURNING sequence";

const INSERT_THREAD: &str = "
    INSERT INTO orchestration_v2_projection_threads (
        thread_id, project_id, title, default_provider, provider_instance_id, runtime_mode,
        interaction_mode, active_provider_thread_id, created_at, updated_at,
        archived_at, deleted_at, payload_json
    ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, NULL, ?7, ?7, NULL, NULL, ?8)";

const RECORD_PROJECTION_SEQUENCE: &str = "
    INSERT INTO orchestration_v2_projection_metadata (
        projection_name, schema_version, last_sequence, updated_at
    ) VALUES ('thread-projections', 2, ?1, ?2)
    ON CONFLICT(projection_name) DO UPDATE SET
        schema_version = excluded.schema_version,
        last_sequence = excluded.last_sequence,
        updated_at = excluded.updated_at";

const INSERT_RECEIPT: &str = "
    INSERT INTO orchestration_command_receipts (
        command_id, aggregate_kind, aggregate_id, command_type, accepted_at,
        result_sequence, status, error
    ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, ?6, ?7)";

#[derive(Clone)]
pub(super) struct ThreadCommands {
    path: PathBuf,
}

#[derive(Debug)]
pub(super) struct CommittedThread {
    pub(super) sequence: u64,
    pub(super) committed: bool,
}

#[derive(Debug)]
pub(super) enum ThreadCommandError {
    InvalidRequest(String),
    Rejected(String),
    MissingProjection(String),
    NotArchived(String),
    Database(rusqlite::Error),
}

impl fmt::Display for ThreadCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) | Self::Rejected(message) => formatter.write_str(message),
            Self::MissingProjection(thread_id) => write!(
                formatter,
                "No orchestration projection exists for thread {thread_id}."
            ),
            Self::NotArchived(thread_id) => {
                write!(formatter, "Thread {thread_id} is not archived.")
            }
            Self::Database(_) => formatter.write_str("Could not save the thread."),
        }
    }
}

impl Error for ThreadCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for ThreadCommandError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl ThreadCommands {
    pub(super) fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let commands = Self { path };
        let database = commands.connect()?;
        for statement in [
            SELECT_RECEIPT,
            THREAD_EXISTS,
            LATEST_THREAD_SEQUENCE,
            APPEND_CREATED,
            INSERT_THREAD,
            RECORD_PROJECTION_SEQUENCE,
            INSERT_RECEIPT,
            visit::SELECT_THREAD,
            visit::APPEND_VISIT,
            visit::UPDATE_WATERMARK,
            visit::UPDATE_OPEN,
        ] {
            database.prepare(statement)?;
        }
        Ok(commands)
    }

    pub(super) fn dispatch(&self, payload: Value) -> Result<CommittedThread, ThreadCommandError> {
        match payload["type"].as_str() {
            Some("thread.visit") => self.visit(payload),
            Some(rename::COMMAND) => self.rename(payload),
            Some(unarchive::COMMAND) => self.unarchive(payload),
            _ => self.create(CreateThread::parse(payload)?),
        }
    }

    pub(super) fn create(
        &self,
        command: CreateThread,
    ) -> Result<CommittedThread, ThreadCommandError> {
        let mut database = self.connect()?;
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = transaction
            .query_row(SELECT_RECEIPT, [&command.command_id], Receipt::read)
            .optional()?
        {
            return receipt.replay(&command.command_id, &command.thread_id);
        }

        let now = now_timestamp();
        if transaction.query_row(THREAD_EXISTS, [&command.thread_id], |row| row.get(0))? {
            let rejection = format!(
                "Thread {} already exists. Use a new thread id.",
                command.thread_id,
            );
            let sequence: u64 =
                transaction.query_row(LATEST_THREAD_SEQUENCE, [&command.thread_id], |row| {
                    row.get(0)
                })?;
            transaction.execute(
                INSERT_RECEIPT,
                params![
                    command.command_id,
                    command.thread_id,
                    "thread.create",
                    now,
                    sequence,
                    "rejected",
                    rejection
                ],
            )?;
            transaction.commit()?;
            return Err(ThreadCommandError::Rejected(rejection));
        }

        let (sequence, _) = append_thread_creation(&transaction, &command, &now)?;
        transaction.execute(RECORD_PROJECTION_SEQUENCE, params![sequence, now])?;
        transaction.execute(
            INSERT_RECEIPT,
            params![
                command.command_id,
                command.thread_id,
                "thread.create",
                now,
                sequence,
                "accepted",
                None::<String>
            ],
        )?;
        transaction.commit()?;
        Ok(CommittedThread {
            sequence,
            committed: true,
        })
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        let connection =
            Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }
}

pub(super) fn append_thread_creation(
    transaction: &Transaction<'_>,
    command: &CreateThread,
    now: &str,
) -> Result<(u64, Value), ThreadCommandError> {
    let project_deleted: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM projection_projects WHERE project_id = ?1 AND deleted_at IS NOT NULL)",
        [&command.project_id],
        |row| row.get(0),
    )?;
    if project_deleted {
        return Err(ThreadCommandError::Rejected(format!(
            "Project {} was not found.",
            command.project_id
        )));
    }
    let projection = command.projection(now);
    let payload = projection.to_string();
    let metadata = json!({ "providerInstanceId": command.model_selection.instance_id });
    let sequence = transaction.query_row(
        APPEND_CREATED,
        params![
            command.event_id(),
            command.thread_id,
            now,
            command.command_id,
            payload,
            metadata.to_string(),
        ],
        |row| row.get(0),
    )?;
    transaction.execute(
        INSERT_THREAD,
        params![
            command.thread_id,
            command.project_id,
            command.title,
            command.model_selection.instance_id,
            command.runtime_mode.as_str(),
            command.interaction_mode.as_str(),
            now,
            payload,
        ],
    )?;
    Ok((sequence, projection))
}

struct Receipt {
    aggregate_kind: String,
    aggregate_id: String,
    sequence: u64,
    status: String,
    error: Option<String>,
}

impl Receipt {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            aggregate_kind: row.get(0)?,
            aggregate_id: row.get(1)?,
            sequence: row.get(2)?,
            status: row.get(3)?,
            error: row.get(4)?,
        })
    }

    fn replay(
        self,
        command_id: &str,
        thread_id: &str,
    ) -> Result<CommittedThread, ThreadCommandError> {
        if self.aggregate_kind != "thread" {
            return Err(ThreadCommandError::Rejected(format!(
                "Command {} was already handled for {} {}.",
                command_id, self.aggregate_kind, self.aggregate_id,
            )));
        }
        if self.status == "rejected" {
            return Err(ThreadCommandError::Rejected(format!(
                "Command {} was previously rejected: {}",
                command_id,
                self.error.as_deref().unwrap_or("Previously rejected."),
            )));
        }
        if self.aggregate_id != thread_id {
            return Err(ThreadCommandError::Rejected(format!(
                "Command {} was already handled for thread {} and cannot be replayed for {}.",
                command_id, self.aggregate_id, thread_id,
            )));
        }
        Ok(CommittedThread {
            sequence: self.sequence,
            committed: false,
        })
    }
}

fn now_timestamp() -> String {
    timestamp(OffsetDateTime::now_utc())
}

fn timestamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond(),
    )
}

#[cfg(test)]
mod tests;
