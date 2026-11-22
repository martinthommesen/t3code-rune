use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::server::settings::SettingsFile;
use crate::server::turn_commands::{ThreadEventStream, record_projected_sequence, timestamp};

use super::execute::{CommitObserver, schedule_preparation};
use super::input::LaunchMessage;
use super::{
    LaunchCommit, LaunchContext, LaunchInput, PreparedLaunch, Trimmed, WorkspaceStrategy, state,
};

pub(in crate::server) const COMMAND: &str = "prepared-run.retry";
const PREPARATION_INPUT: &str = "Preparing workspace";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct RetryInput {
    command_id: Trimmed,
    thread_id: Trimmed,
    run_id: Trimmed,
}

impl RetryInput {
    fn missing(&self) -> String {
        format!(
            "Run {} has no failed workspace preparation to retry.",
            self.run_id.0
        )
    }

    fn error_message(&self) -> String {
        format!(
            "Failed to dispatch orchestration command {COMMAND} ({}).",
            self.command_id.0
        )
    }

    fn error(&self, detail: &str, cause: Value) -> Value {
        json!({
            "_tag": "OrchestrationV2DispatchCommandError",
            "commandId": self.command_id.0,
            "commandType": COMMAND,
            "message": detail,
            "detail": detail,
            "cause": cause,
        })
    }

    fn rejection(&self, detail: &str) -> Value {
        self.error(
            detail,
            json!({
                "name": "OrchestratorDispatchError",
                "message": self.error_message(),
                "cause": detail,
            }),
        )
    }

    fn replay_rejection(&self, detail: &str) -> Value {
        let message = format!(
            "Command {} was previously rejected: {detail}",
            self.command_id.0
        );
        self.error(
            &message,
            json!({
                "name": "OrchestratorCommandPreviouslyRejectedError",
                "message": message,
            }),
        )
    }
}

pub(in crate::server) async fn retry_preparation(
    input: RetryInput,
    context: LaunchContext<'_>,
    on_commit: impl Fn(&str, bool) + Send + Sync + 'static,
) -> Result<u64, Value> {
    let database = context.database.to_owned();
    let scratch_root = context.scratch_root.to_owned();
    let settings_path = context.settings_path.map(Path::to_owned);
    let terms = context.terminals;
    let codex_enabled = context.codex_enabled;
    let interrupted = input.rejection("Workspace preparation retry could not be recorded.");
    let on_commit: CommitObserver = Arc::new(on_commit);
    tokio::task::spawn_blocking(move || {
        let retry =
            record_retry(&database, &input).map_err(|error| input.rejection(&error.to_string()))?;
        let (sequence, preparing, committed) = match retry {
            Outcome::Accepted {
                sequence,
                preparing,
                committed,
            } => (sequence, preparing, committed),
            Outcome::Rejected(detail) => return Err(input.rejection(&detail)),
            Outcome::PreviouslyRejected(detail) => return Err(input.replay_rejection(&detail)),
        };
        if committed {
            on_commit(&input.thread_id.0, false);
        }
        if let Some(preparing) = preparing {
            let resumed = !committed;
            let run_id = input.run_id.0.clone();
            let plan = (*preparing).launch(
                input,
                database,
                scratch_root.with_file_name("worktrees"),
                settings_path.clone(),
                terms,
            );
            let codex_enabled = codex_enabled
                && settings_path.is_none_or(|path| {
                    SettingsFile::new(path).read().is_ok_and(|settings| {
                        let (project, instance) = plan.input.provider_project();
                        settings.codex_instance(Some(project), instance).is_ok()
                    })
                });
            let preparation = LaunchCommit {
                thread_id: plan.thread_id.clone(),
                run_id: Some(run_id),
                schedule_preparation: true,
                sequence: Some(sequence),
                resumed,
            };
            schedule_preparation(plan, preparation, codex_enabled, on_commit);
        }
        Ok(sequence)
    })
    .await
    .map_err(|_| interrupted)?
}

enum Outcome {
    Accepted {
        sequence: u64,
        preparing: Option<Box<Preparation>>,
        committed: bool,
    },
    Rejected(String),
    PreviouslyRejected(String),
}

struct Preparation {
    project_root: PathBuf,
    thread: Value,
    run: Value,
    attempt: Value,
    node: Value,
    item: Value,
    failure: Option<Value>,
    message: Value,
}

impl Preparation {
    fn load(
        db: &Transaction<'_>,
        input: &RetryInput,
        status: &str,
    ) -> rusqlite::Result<Option<Self>> {
        db.query_row(
            include_str!("retry/preparation.sql"),
            params![input.thread_id.0, input.run_id.0, status],
            |row| {
                Ok(Self {
                    project_root: PathBuf::from(row.get::<_, String>(0)?),
                    thread: payload(row, 1)?,
                    run: payload(row, 2)?,
                    attempt: payload(row, 3)?,
                    node: payload(row, 4)?,
                    item: payload(row, 5)?,
                    failure: row
                        .get::<_, Option<String>>(6)?
                        .map(|text| serde_json::from_str(&text))
                        .transpose()
                        .map_err(json_error)?,
                    message: payload(row, 7)?,
                })
            },
        )
        .optional()
    }

    fn launch(
        self,
        retry: RetryInput,
        database: PathBuf,
        default_worktrees_root: PathBuf,
        settings_path: Option<PathBuf>,
        terminals: Option<Arc<crate::server::terminal::TerminalManager>>,
    ) -> PreparedLaunch {
        let branch = self.thread["branch"].as_str().map(str::to_owned);
        let context = |key: &str| self.thread[key].clone();
        let prompt = self.message["text"].as_str().unwrap_or_default();
        let input = LaunchInput {
            command_id: retry.command_id,
            project_id: Trimmed(context("projectId").as_str().unwrap_or_default().to_owned()),
            thread_id: None,
            reuse_existing_thread: None,
            title: Trimmed(context("title").as_str().unwrap_or_default().to_owned()),
            generate_title: None,
            model_selection: serde_json::from_value(self.run["modelSelection"].clone())
                .expect("validated prepared model selection"),
            runtime_mode: serde_json::from_value(context("runtimeMode"))
                .expect("validated prepared runtime mode"),
            interaction_mode: serde_json::from_value(context("interactionMode"))
                .expect("validated prepared interaction mode"),
            workspace_strategy: serde_json::from_value(self.run["workspacePreparation"].clone())
                .expect("validated workspace preparation"),
            initial_message: Some(LaunchMessage {
                message_id: None,
                text: prompt.to_owned(),
                context: None,
                attachments: Vec::new(),
            }),
            creation_source: None,
        };
        PreparedLaunch {
            input,
            database,
            settings_path,
            terminals,
            default_worktrees_root,
            thread_id: retry.thread_id.0,
            workspace_root: self.project_root,
            branch,
            initial_message_id: None,
            had_launch_receipt: true,
        }
    }

    fn validate(&self) -> rusqlite::Result<bool> {
        let strategy =
            serde_json::from_value::<WorkspaceStrategy>(self.run["workspacePreparation"].clone())
                .map_err(json_error)?;
        Ok(
            matches!(strategy, WorkspaceStrategy::Worktree { repositories, .. }
                if repositories.as_ref().is_none_or(|items| items.is_empty()))
                && self.thread["projectId"].is_string()
                && self.thread["title"].is_string()
                && self.message["text"].is_string()
                && serde_json::from_value::<super::input::ModelSelection>(
                    self.run["modelSelection"].clone(),
                )
                .is_ok()
                && serde_json::from_value::<super::input::RuntimeMode>(
                    self.thread["runtimeMode"].clone(),
                )
                .is_ok()
                && serde_json::from_value::<super::input::InteractionMode>(
                    self.thread["interactionMode"].clone(),
                )
                .is_ok(),
        )
    }
}

fn record_retry(path: &Path, input: &RetryInput) -> rusqlite::Result<Outcome> {
    let mut database = state::open_writer(path)?;
    let tx = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let command_id = &input.command_id.0;
    let thread_id = &input.thread_id.0;
    if let Some((receipt_thread, command, status, sequence, error)) = tx
        .query_row(
            "SELECT aggregate_id, command_type, status, result_sequence, error
         FROM orchestration_command_receipts WHERE command_id=?1 AND aggregate_kind='thread'",
            [command_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?
    {
        if status == "rejected" {
            return Ok(Outcome::PreviouslyRejected(
                error.unwrap_or_else(|| "Previously rejected.".to_owned()),
            ));
        }
        if receipt_thread != *thread_id || command != COMMAND {
            return Ok(Outcome::Rejected(format!(
                "Command {command_id} already has a conflicting receipt."
            )));
        }
        let preparing = Preparation::load(&tx, input, "preparing")?;
        let preparing = match preparing {
            Some(preparing) if preparing.validate()? => Some(Box::new(preparing)),
            _ => None,
        };
        return Ok(Outcome::Accepted {
            sequence,
            preparing,
            committed: false,
        });
    }
    let Some(mut preparing) = Preparation::load(&tx, input, "failed")? else {
        return reject(tx, input, input.missing());
    };
    let Some(mut failure) = preparing.failure.take() else {
        return reject(tx, input, input.missing());
    };
    if !preparing.validate()? {
        return reject(tx, input, input.missing());
    }
    let active: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_runs
         WHERE thread_id=?1 AND run_id<>?2 AND status IN ('preparing','starting','running','waiting'))",
        params![thread_id,input.run_id.0], |row| row.get(0),
    )?;
    if active {
        return reject(
            tx,
            input,
            "Another run is active on this thread.".to_owned(),
        );
    }
    let owns_queue =
        latest_executed_run(&tx, thread_id)?.as_deref() == Some(input.run_id.0.as_str());
    let now = timestamp(OffsetDateTime::now_utc());
    failure["status"] = json!("cancelled");
    failure["updatedAt"] = json!(now);
    if let Some(fields) = preparing.item.as_object_mut() {
        fields.remove("output");
        fields.remove("exitCode");
        fields.remove("outputIndicatesFailure");
    }
    preparing.item["status"] = json!("running");
    preparing.item["title"] = json!(PREPARATION_INPUT);
    preparing.item["completedAt"] = Value::Null;
    preparing.item["updatedAt"] = json!(now);
    for item in [&mut preparing.attempt, &mut preparing.node] {
        item["status"] = json!("pending");
        item["completedAt"] = Value::Null;
    }
    preparing.run["status"] = json!("preparing");
    preparing.run["completedAt"] = Value::Null;
    let metadata = json!({
        "runId": input.run_id.0,
        "nodeId": preparing.node["id"],
        "providerInstanceId": preparing.run["providerInstanceId"],
    });
    let stream = ThreadEventStream::command(&tx, thread_id, command_id);
    let mut sequence = 0;
    for (kind, payload) in [
        ("turn-item.updated", &failure),
        ("turn-item.updated", &preparing.item),
        ("run-attempt.updated", &preparing.attempt),
        ("node.updated", &preparing.node),
        ("run.updated", &preparing.run),
    ] {
        sequence = stream.append(kind, &now, &metadata, payload)?;
    }
    if owns_queue && let Some(unheld) = change_queued_holds(&tx, &stream, thread_id, &now, false)? {
        sequence = unheld;
    }
    record_projected_sequence(&tx, sequence, &now)?;
    let sequence = u64::try_from(sequence).map_err(|_| rusqlite::Error::InvalidQuery)?;
    state::record_receipt(&tx, command_id, thread_id, COMMAND, sequence, &now, None)?;
    tx.commit()?;
    Ok(Outcome::Accepted {
        sequence,
        preparing: Some(Box::new(preparing)),
        committed: true,
    })
}

fn reject(tx: Transaction<'_>, input: &RetryInput, detail: String) -> rusqlite::Result<Outcome> {
    let sequence = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM orchestration_events
         WHERE aggregate_kind='thread' AND application_event_version=2 AND stream_id=?1",
        [&input.thread_id.0],
        |row| row.get(0),
    )?;
    let now = timestamp(OffsetDateTime::now_utc());
    state::record_receipt(
        &tx,
        &input.command_id.0,
        &input.thread_id.0,
        COMMAND,
        sequence,
        &now,
        Some(&input.error_message()),
    )?;
    tx.commit()?;
    Ok(Outcome::Rejected(detail))
}

pub(super) fn latest_executed_run(
    db: &Transaction<'_>,
    thread_id: &str,
) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT run_id FROM orchestration_v2_projection_runs
         WHERE thread_id=?1 AND status <> 'queued'
           AND NOT (status='cancelled' AND json_extract(payload_json,'$.startedAt') IS NULL)
         ORDER BY completed_at IS NULL DESC, julianday(completed_at) DESC, ordinal DESC LIMIT 1",
        [thread_id],
        |row| row.get(0),
    )
    .optional()
}

pub(super) fn change_queued_holds(
    db: &Transaction<'_>,
    stream: &ThreadEventStream<'_>,
    thread_id: &str,
    now: &str,
    held: bool,
) -> rusqlite::Result<Option<i64>> {
    let mut statement = db.prepare(
        "SELECT payload_json FROM orchestration_v2_projection_runs
         WHERE thread_id=?1 AND status='queued' AND (json_type(payload_json,'$.queueHeld') IS 'true') <> ?2
         ORDER BY ordinal",
    )?;
    let queued = statement
        .query_map(params![thread_id, held], |row| payload(row, 0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut sequence = None;
    for mut run in queued {
        run["queueHeld"] = json!(held);
        let metadata = json!({"runId":run["id"],"providerInstanceId":run["providerInstanceId"]});
        sequence = Some(stream.append("run.updated", now, &metadata, &run)?);
    }
    Ok(sequence)
}

fn payload(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let raw: String = row.get(index)?;
    serde_json::from_str(&raw).map_err(json_error)
}

fn json_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
