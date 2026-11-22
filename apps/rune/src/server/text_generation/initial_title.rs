mod prompt;
mod storage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::server::ProjectionChange;
use crate::server::settings::{CodexInstanceError, SettingsFile};
use crate::server::turn_worker::{TurnQueue, launch::process_config};

use super::structured_codex;
use storage::{Claim, Completion, Context};

const LIVENESS_POLL: Duration = Duration::from_secs(30);
const GENERATION_DEADLINE: Duration = Duration::from_secs(180);
const MAX_WORKERS: usize = 3;

pub(in crate::server) fn recover(database: &Path) -> rusqlite::Result<()> {
    storage::recover(database)
}

pub(in crate::server) async fn run(
    database: PathBuf,
    settings: SettingsFile,
    changes: broadcast::Sender<ProjectionChange>,
    turn_queue: TurnQueue,
) -> Result<(), String> {
    let worker = Arc::new(Worker {
        database,
        settings,
        changes,
        turn_queue,
    });
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..MAX_WORKERS {
        let worker = Arc::clone(&worker);
        tasks.spawn(async move { worker.consume().await });
    }
    while let Some(task) = tasks.join_next().await {
        task.map_err(|error| error.to_string())??;
    }
    Ok(())
}

struct Worker {
    database: PathBuf,
    settings: SettingsFile,
    changes: broadcast::Sender<ProjectionChange>,
    turn_queue: TurnQueue,
}

impl Worker {
    async fn consume(&self) -> Result<(), String> {
        let worker = format!("rune-title:{}", Uuid::new_v4());
        let mut changes = self.changes.subscribe();
        loop {
            let database = self.database.clone();
            let owner = worker.clone();
            let claim = storage_task(move || storage::claim(&database, &owner)).await?;
            let Some(claim) = claim else {
                tokio::select! {
                    _ = tokio::time::sleep(LIVENESS_POLL) => {},
                    updated = changes.recv() => {
                        if matches!(updated, Err(broadcast::error::RecvError::Closed)) {
                            return Ok(());
                        }
                    },
                }
                continue;
            };
            let claim = Arc::new(claim);
            let outcome = self.generate(&claim).await?;
            let database = self.database.clone();
            let owner = worker.clone();
            let claimed = Arc::clone(&claim);
            if let Some(changed) =
                storage_task(move || storage::complete(&database, &owner, &claimed, outcome))
                    .await?
            {
                let _ = self
                    .changes
                    .send(ProjectionChange::Thread(claim.thread_id.clone()));
                if changed.apply_title {
                    self.turn_queue.wake();
                }
            }
        }
    }

    async fn generate(&self, claim: &Arc<Claim>) -> Result<Completion, String> {
        for delay in [0, 2, 4] {
            if delay > 0 {
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            let database = self.database.clone();
            let claimed = Arc::clone(claim);
            let context = storage_task(move || storage::load(&database, &claimed)).await?;
            let Some(context) = context else {
                return Ok(Completion::Stale);
            };
            if context.workspace_root.is_none()
                || context.message.is_empty()
                || prompt::attachment_only(&context.message)
            {
                return Ok(Completion::Complete(None));
            }
            if let Ok(title) = generate_title(&self.settings, &context).await {
                return Ok(Completion::Complete(title));
            }
        }
        Ok(Completion::Complete(None))
    }
}

async fn storage_task<T: Send + 'static>(
    operation: impl FnOnce() -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

async fn generate_title(settings: &SettingsFile, context: &Context) -> Result<Option<String>, ()> {
    let snapshot = settings.read().map_err(|_| ())?;
    let public = snapshot.for_client();
    let overrides = &public["projectSettingsOverrides"][&context.project_id];
    let environment_model = &public["textGenerationModelSelection"];
    let project_model = overrides.get("textGenerationModelSelection");
    let mut selection = project_model.unwrap_or(environment_model);
    let project = Some(context.project_id.as_str());
    let instance_id = selection["instanceId"].as_str().ok_or(())?;
    let instance = match snapshot.codex_instance(project, instance_id) {
        Err(CodexInstanceError::Disabled) if project_model.is_some() => {
            selection = environment_model;
            let instance = selection["instanceId"].as_str().ok_or(())?;
            snapshot.codex_instance(project, instance)
        }
        matched => matched,
    }
    .map_err(|_| ())?;
    let workspace = context.workspace_root.as_ref().ok_or(())?;
    let cwd = context
        .worktree
        .as_deref()
        .filter(|path| Path::new(path).exists())
        .unwrap_or(workspace);
    let launch = process_config(instance, PathBuf::from(cwd)).map_err(|_| ())?;
    let instructions = overrides["threadTitleInstructions"]
        .as_str()
        .or_else(|| public["threadTitleInstructions"].as_str())
        .unwrap_or_default();
    let prompt = prompt::initial(
        &context.message,
        context.project_title.as_deref().unwrap_or_default(),
        instructions,
    );
    let result = structured_codex::run(
        &launch,
        structured_codex::Request {
            prompt: &prompt,
            schema: &prompt::output_schema(),
            model: selection["model"].as_str().ok_or(())?,
            reasoning_effort: option(selection, "reasoningEffort").unwrap_or("low"),
            service_tier: option(selection, "serviceTier"),
        },
        GENERATION_DEADLINE,
    )
    .await
    .map_err(|_| ())?;
    prompt::generated(result)
}

fn option<'a>(selection: &'a Value, id: &str) -> Option<&'a str> {
    selection["options"].as_array()?.iter().find_map(|option| {
        (option["id"] == id)
            .then(|| option["value"].as_str())
            .flatten()
    })
}
