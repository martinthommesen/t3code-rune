mod git;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::{broadcast, watch};

use self::git::{Outcome, Stage, Update};
use super::ProjectionChange;
use super::project_commands::{ProjectCommands, workspace_path};
use super::turn_commands::timestamp;

const DONE_RETENTION: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StartInput {
    project_id: String,
    title: String,
    created_at: String,
    remote_url: Option<String>,
    destination_path: String,
    provider: Option<String>,
    repository: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StartResult {
    project_id: String,
    cwd: String,
    remote_url: String,
    repository: Option<Value>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Snapshot {
    project_id: String,
    remote_url: String,
    destination_path: String,
    repository: Option<Value>,
    phase: Phase,
    stage: Stage,
    percent: Option<u8>,
    detail: Option<String>,
    stalled: bool,
    error: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    sequence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Running,
    Done,
    Failed,
    Cancelled,
}

struct Job {
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
}

struct Entry {
    snapshot: Snapshot,
    clone_url: String,
    job: Option<Job>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    sequence: u64,
    closing: bool,
}

#[derive(Clone)]
pub(super) struct ProjectClones(Arc<Tracker>);

struct Tracker {
    projects: ProjectCommands,
    projections: broadcast::Sender<ProjectionChange>,
    actions: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    published: watch::Sender<Vec<Snapshot>>,
}

#[derive(Serialize)]
#[serde(tag = "_tag")]
pub(super) enum CloneError {
    SourceControlRepositoryError {
        provider: String,
        operation: &'static str,
        detail: String,
    },
    OrchestrationDispatchCommandError {
        message: &'static str,
    },
}

impl CloneError {
    fn clone_failed(provider: &str, detail: impl Into<String>) -> Self {
        Self::SourceControlRepositoryError {
            provider: provider.to_owned(),
            operation: "cloneRepository",
            detail: detail.into(),
        }
    }

    fn create_project() -> Self {
        Self::OrchestrationDispatchCommandError {
            message: "Failed to create clone project.",
        }
    }

    fn discard(detail: &'static str) -> Self {
        Self::SourceControlRepositoryError {
            provider: "unknown".into(),
            operation: "discardClone",
            detail: detail.into(),
        }
    }
}

impl ProjectClones {
    pub(super) fn new(
        projects: ProjectCommands,
        projections: broadcast::Sender<ProjectionChange>,
    ) -> Self {
        Self(Arc::new(Tracker {
            projects,
            projections,
            actions: tokio::sync::Mutex::new(()),
            state: Mutex::new(State::default()),
            published: watch::channel(Vec::new()).0,
        }))
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Vec<Snapshot>> {
        self.0.published.subscribe()
    }

    pub(super) async fn start(&self, input: StartInput) -> Result<StartResult, CloneError> {
        let tracker = self.clone();
        tokio::spawn(async move { tracker.start_owned(input).await })
            .await
            .map_err(|_| CloneError::create_project())?
    }

    async fn start_owned(&self, input: StartInput) -> Result<StartResult, CloneError> {
        let _action = self.0.actions.lock().await;
        if self.0.state.lock().expect("clone state lock").closing {
            return Err(CloneError::create_project());
        }
        let provider = input.provider.as_deref().unwrap_or("unknown").trim();
        let project_id = input.project_id.trim();
        let title = input.title.trim();
        if project_id.is_empty()
            || title.is_empty()
            || OffsetDateTime::parse(&input.created_at, &Rfc3339).is_err()
        {
            return Err(CloneError::create_project());
        }
        let destination = workspace_path(input.destination_path.trim())
            .map_err(|error| CloneError::clone_failed(provider, error.to_string()))?;
        if input.destination_path.trim().is_empty() || Path::new(&destination).file_name().is_none()
        {
            return Err(CloneError::clone_failed(
                provider,
                "Choose a destination path before cloning.",
            ));
        }
        let preflight_path = destination.clone();
        let requested_provider = provider.to_owned();
        tokio::task::spawn_blocking(move || {
            prepare_destination(Path::new(&preflight_path), &requested_provider)
        })
        .await
        .map_err(|_| {
            CloneError::clone_failed(provider, "Could not prepare the clone destination.")
        })??;
        if input.provider.is_some() && input.repository.is_some() {
            return Err(CloneError::clone_failed(
                provider,
                "This environment does not support repository lookup.",
            ));
        }
        let remote = input
            .remote_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                CloneError::clone_failed(
                    provider,
                    "Enter a repository path or clone URL before cloning.",
                )
            })?;
        let started_at = now();
        if self.is_running(project_id, &destination) {
            return Err(CloneError::clone_failed(
                provider,
                "A clone into this destination is already in progress.",
            ));
        }
        let projects = self.0.projects.clone();
        let project_command = json!({
            "type": "project.create",
            "commandId": format!("project-clone-create:{project_id}"),
            "projectId": project_id,
            "title": title,
            "workspaceRoot": destination,
            "createWorkspaceRootIfMissing": true,
        });
        let requested_id = project_id.to_owned();
        let workspace = PathBuf::from(&destination);
        let (project_id, committed) = tokio::task::spawn_blocking(move || {
            if let Some(owner) = projects
                .workspace_owner(&workspace)
                .map_err(|_| CloneError::create_project())?
            {
                return Ok((owner, false));
            }
            match projects.dispatch(project_command) {
                Ok(project) => Ok((requested_id, project.committed)),
                Err(_) => projects
                    .workspace_owner(&workspace)
                    .map_err(|_| CloneError::create_project())?
                    .map(|owner| (owner, false))
                    .ok_or_else(CloneError::create_project),
            }
        })
        .await
        .map_err(|_| CloneError::create_project())??;
        if committed {
            let _ = self.0.projections.send(ProjectionChange::Project);
        }
        let public_remote = git::public_url(remote);
        {
            let mut state = self.0.state.lock().expect("clone state lock");
            state.entries.retain(|entry| {
                entry.snapshot.project_id != project_id
                    && entry.snapshot.destination_path != destination
            });
            state.sequence += 1;
            let snapshot = Snapshot {
                project_id: project_id.clone(),
                remote_url: public_remote.clone(),
                destination_path: destination.clone(),
                repository: None,
                phase: Phase::Running,
                stage: Stage::Connecting,
                percent: None,
                detail: None,
                stalled: false,
                error: None,
                started_at,
                ended_at: None,
                sequence: state.sequence,
            };
            self.start_job(&mut state, snapshot, remote.to_owned());
        }
        Ok(StartResult {
            project_id,
            cwd: destination,
            remote_url: public_remote,
            repository: None,
        })
    }

    pub(super) async fn cancel(&self, project_id: String) -> bool {
        let tracker = self.clone();
        tokio::spawn(async move {
            let _action = tracker.0.actions.lock().await;
            tracker.cancel_running(&project_id).await
        })
        .await
        .unwrap_or(false)
    }

    async fn cancel_running(&self, project_id: &str) -> bool {
        let (mut finished, destination) = {
            let state = self.0.state.lock().expect("clone state lock");
            let Some(entry) = state
                .entries
                .iter()
                .find(|entry| entry.snapshot.project_id == project_id)
            else {
                return false;
            };
            let Some(job) = &entry.job else { return false };
            job.cancel.send_replace(true);
            (
                job.finished.clone(),
                entry.snapshot.destination_path.clone(),
            )
        };
        let _ = finished.wait_for(|finished| *finished).await;
        let done = self
            .0
            .state
            .lock()
            .expect("clone state lock")
            .entries
            .iter()
            .any(|entry| {
                entry.snapshot.project_id == project_id && entry.snapshot.phase == Phase::Done
            });
        if !done {
            let _ = discard_destination(destination).await;
        }
        true
    }

    pub(super) async fn retry(&self, project_id: String) -> Result<bool, CloneError> {
        let tracker = self.clone();
        tokio::spawn(async move {
            let _action = tracker.0.actions.lock().await;
            let (mut snapshot, clone_url) = {
                let state = tracker.0.state.lock().expect("clone state lock");
                if state.closing {
                    return Ok(false);
                }
                let Some(entry) = state.entries.iter().find(|entry| {
                    entry.snapshot.project_id == project_id
                        && matches!(entry.snapshot.phase, Phase::Failed | Phase::Cancelled)
                }) else {
                    return Ok(false);
                };
                (entry.snapshot.clone(), entry.clone_url.clone())
            };
            discard_destination(snapshot.destination_path.clone()).await?;
            let mut state = tracker.0.state.lock().expect("clone state lock");
            state.sequence += 1;
            snapshot.sequence = state.sequence;
            snapshot.phase = Phase::Running;
            snapshot.stage = Stage::Connecting;
            snapshot.percent = None;
            snapshot.detail = None;
            snapshot.stalled = false;
            snapshot.error = None;
            snapshot.started_at = now();
            snapshot.ended_at = None;
            tracker.start_job(&mut state, snapshot, clone_url);
            Ok(true)
        })
        .await
        .map_err(|_| CloneError::discard("The clone could not be restarted."))?
    }

    pub(super) async fn discard(&self, project_id: &str) {
        let _action = self.0.actions.lock().await;
        if !self.cancel_running(project_id).await {
            let incomplete = {
                let state = self.0.state.lock().expect("clone state lock");
                state
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.snapshot.project_id == project_id
                            && entry.snapshot.phase != Phase::Done
                    })
                    .map(|entry| entry.snapshot.destination_path.clone())
            };
            if let Some(destination) = incomplete {
                let _ = discard_destination(destination).await;
            }
        }
        let mut state = self.0.state.lock().expect("clone state lock");
        let count = state.entries.len();
        state
            .entries
            .retain(|entry| entry.snapshot.project_id != project_id);
        if count != state.entries.len() {
            self.publish(&state);
        }
    }

    pub(super) async fn shutdown(&self) {
        let _action = self.0.actions.lock().await;
        let active = {
            let mut state = self.0.state.lock().expect("clone state lock");
            state.closing = true;
            state
                .entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .job
                        .as_ref()
                        .map(|_| entry.snapshot.project_id.clone())
                })
                .collect::<Vec<_>>()
        };
        for project_id in active {
            self.cancel_running(&project_id).await;
        }
    }

    fn is_running(&self, project_id: &str, destination: &str) -> bool {
        self.0
            .state
            .lock()
            .expect("clone state lock")
            .entries
            .iter()
            .any(|entry| {
                entry.snapshot.phase == Phase::Running
                    && (entry.snapshot.project_id == project_id
                        || entry.snapshot.destination_path == destination)
            })
    }

    fn start_job(&self, state: &mut State, snapshot: Snapshot, clone_url: String) {
        let (cancel, cancelled) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        let project_id = snapshot.project_id.clone();
        let destination = PathBuf::from(&snapshot.destination_path);
        let entry = Entry {
            snapshot,
            clone_url: clone_url.clone(),
            job: Some(Job { cancel, finished }),
        };
        if let Some(slot) = state
            .entries
            .iter_mut()
            .find(|entry| entry.snapshot.project_id == project_id)
        {
            *slot = entry;
        } else {
            state.entries.push(entry);
        }
        self.publish(state);
        let tracker = self.clone();
        tokio::spawn(async move {
            let outcome = git::run(&clone_url, &destination, cancelled, |update| {
                tracker.progress(&project_id, update)
            })
            .await;
            let sequence = tracker.finish(&project_id, &outcome);
            done.send_replace(true);
            if let (Outcome::Done, Some(sequence)) = (outcome, sequence) {
                tracker.update_project(&project_id).await;
                tokio::time::sleep(DONE_RETENTION).await;
                let mut state = tracker.0.state.lock().expect("clone state lock");
                let old_len = state.entries.len();
                state.entries.retain(|entry| {
                    entry.snapshot.project_id != project_id || entry.snapshot.sequence != sequence
                });
                if old_len != state.entries.len() {
                    tracker.publish(&state);
                }
            }
        });
    }

    fn progress(&self, project_id: &str, update: Update) {
        self.update_snapshot(project_id, |entry| match update {
            Update::Progress(progress) => {
                entry.snapshot.stage = progress.stage;
                entry.snapshot.percent = progress.percent;
                entry.snapshot.detail = progress.detail.map(|detail| clamp(&detail, 200));
                entry.snapshot.stalled = false;
            }
            Update::Stalled => entry.snapshot.stalled = true,
        });
    }

    fn finish(&self, project_id: &str, outcome: &Outcome) -> Option<u64> {
        self.update_snapshot(project_id, |entry| {
            entry.job = None;
            entry.snapshot.ended_at = Some(now());
            entry.snapshot.stalled = false;
            entry.snapshot.phase = match outcome {
                Outcome::Done => {
                    entry.snapshot.percent = Some(100);
                    Phase::Done
                }
                Outcome::Failed(error) => {
                    entry.snapshot.error = Some(clamp(error, 1000));
                    Phase::Failed
                }
                Outcome::Cancelled => Phase::Cancelled,
            };
        })
    }

    fn update_snapshot(&self, project_id: &str, change: impl FnOnce(&mut Entry)) -> Option<u64> {
        let mut state = self.0.state.lock().expect("clone state lock");
        let index = state
            .entries
            .iter()
            .position(|entry| entry.snapshot.project_id == project_id)?;
        state.sequence += 1;
        let sequence = state.sequence;
        let entry = &mut state.entries[index];
        change(entry);
        entry.snapshot.sequence = sequence;
        self.publish(&state);
        Some(sequence)
    }

    fn publish(&self, state: &State) {
        self.0.published.send_replace(
            state
                .entries
                .iter()
                .map(|entry| entry.snapshot.clone())
                .collect(),
        );
    }

    async fn update_project(&self, project_id: &str) {
        let projects = self.0.projects.clone();
        let command = json!({
            "type": "project.update",
            "commandId": format!("project-clone-done:{project_id}"),
            "projectId": project_id,
        });
        if tokio::task::spawn_blocking(move || projects.dispatch(command))
            .await
            .is_ok_and(|result| result.is_ok_and(|project| project.committed))
        {
            let _ = self.0.projections.send(ProjectionChange::Project);
        }
    }
}

fn prepare_destination(path: &Path, provider: &str) -> Result<(), CloneError> {
    match fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(CloneError::clone_failed(
                    provider,
                    "Destination path already exists and is not empty.",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path.parent().expect("absolute destination with basename")).map_err(
                |_| CloneError::clone_failed(provider, "Could not prepare the clone destination."),
            )
        }
        Err(_) => Err(CloneError::clone_failed(
            provider,
            "Destination path already exists and is not a directory.",
        )),
    }
}

async fn discard_destination(destination: String) -> Result<(), CloneError> {
    tokio::task::spawn_blocking(move || discard(Path::new(&destination)))
        .await
        .map_err(|_| CloneError::discard("The incomplete checkout could not be removed."))?
}

fn discard(path: &Path) -> Result<(), CloneError> {
    match fs::read_dir(path) {
        Ok(mut entries) => {
            let populated = entries.next().is_some();
            if !populated {
                return Ok(());
            }
            if !path.join(".git").is_dir() {
                return Err(CloneError::discard(
                    "Destination path contains files that are not from the clone.",
                ));
            }
            fs::remove_dir_all(path)
                .and_then(|()| fs::create_dir(path))
                .map_err(|_| CloneError::discard("The incomplete checkout could not be removed."))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|_| CloneError::discard("The clone destination could not be prepared.")),
        Err(_) => Err(CloneError::discard(
            "The clone destination could not be inspected.",
        )),
    }
}

fn now() -> String {
    timestamp(OffsetDateTime::now_utc())
}

fn clamp(value: &str, max_units: usize) -> String {
    if value.encode_utf16().count() <= max_units {
        return value.to_owned();
    }
    let mut units = 0;
    let end = value
        .char_indices()
        .find_map(|(index, character)| {
            units += character.len_utf16();
            (units >= max_units).then_some(index)
        })
        .unwrap_or(value.len());
    format!("{}…", &value[..end])
}
