use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::watch;

type Key = (PathBuf, String);
type Channel = watch::Sender<State>;

const FINISHED_RETENTION: Duration = Duration::from_secs(30);
const TAIL_LIMIT: usize = 4;
static SETUPS: Mutex<BTreeMap<Key, Weak<Channel>>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum StageId {
    Fetch,
    Checkout,
    Submodules,
    SetupScript,
    Agent,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Status {
    Pending,
    Running,
    Done,
    Skipped,
    Failed,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Phase {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Stage {
    id: StageId,
    status: Status,
    started_at: Option<String>,
    ended_at: Option<String>,
    percent: Option<u8>,
    detail: Option<String>,
    tail: Vec<String>,
}

impl Stage {
    fn empty(id: StageId) -> Self {
        Self {
            id,
            status: Status::Pending,
            started_at: None,
            ended_at: None,
            percent: None,
            detail: None,
            tail: Vec::new(),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ScriptInfo {
    pub(super) name: String,
    pub(super) command: String,
    pub(super) terminal_id: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    thread_id: String,
    phase: Phase,
    started_at: String,
    ended_at: Option<String>,
    branch: Option<String>,
    base_ref: String,
    worktree_path: Option<PathBuf>,
    setup_script: Option<ScriptInfo>,
    stages: [Stage; 5],
    error: Option<String>,
    sequence: u64,
}

#[derive(Default)]
struct State {
    generation: u64,
    snapshot: Option<Arc<Snapshot>>,
}

fn channel(database: &Path, thread_id: &str) -> Arc<Channel> {
    let key = (database.to_owned(), thread_id.to_owned());
    let mut setups = SETUPS.lock().unwrap();
    setups.retain(|_, existing| existing.strong_count() > 0);
    if let Some(current) = setups.get(&key).and_then(Weak::upgrade) {
        return current;
    }
    let channel = Arc::new(watch::channel(State::default()).0);
    setups.insert(key, Arc::downgrade(&channel));
    channel
}

pub(in crate::server) struct WorktreeSetupSubscription {
    _channel: Arc<Channel>,
    receiver: watch::Receiver<State>,
    initial: bool,
}

impl WorktreeSetupSubscription {
    pub(in crate::server) async fn recv(&mut self) -> Option<Value> {
        if self.initial {
            self.initial = false;
        } else {
            self.receiver.changed().await.ok()?;
        }
        let state = self.receiver.borrow_and_update();
        Some(serde_json::to_value(state.snapshot.as_deref()).unwrap_or(Value::Null))
    }
}

pub(in crate::server) fn subscribe_worktree_setup(
    database: &Path,
    thread_id: &str,
) -> WorktreeSetupSubscription {
    let channel = channel(database, thread_id);
    let receiver = channel.subscribe();
    WorktreeSetupSubscription {
        _channel: channel,
        receiver,
        initial: true,
    }
}

#[derive(Clone)]
pub(super) struct Progress {
    channel: Arc<Channel>,
    generation: u64,
}

impl Progress {
    pub(super) fn begin(
        database: &Path,
        thread_id: &str,
        branch: Option<&str>,
        base_ref: &str,
    ) -> Self {
        let channel = channel(database, thread_id);
        let mut generation = 0;
        channel.send_modify(|state| {
            state.generation += 1;
            generation = state.generation;
            state.snapshot = Some(Arc::new(Snapshot {
                thread_id: thread_id.to_owned(),
                phase: Phase::Running,
                started_at: now(),
                ended_at: None,
                branch: branch.map(str::to_owned),
                base_ref: base_ref.to_owned(),
                worktree_path: None,
                setup_script: None,
                stages: [
                    StageId::Fetch,
                    StageId::Checkout,
                    StageId::Submodules,
                    StageId::SetupScript,
                    StageId::Agent,
                ]
                .map(Stage::empty),
                error: None,
                sequence: state
                    .snapshot
                    .as_ref()
                    .map_or(0, |prior| prior.sequence + 1),
            }));
        });
        Self {
            channel,
            generation,
        }
    }

    fn update(&self, update: impl FnOnce(&mut Snapshot)) {
        self.channel.send_if_modified(|state| {
            if state.generation != self.generation {
                return false;
            }
            let Some(snapshot) = state.snapshot.as_mut() else {
                return false;
            };
            let snapshot = Arc::make_mut(snapshot);
            update(snapshot);
            snapshot.sequence += 1;
            true
        });
    }

    pub(super) fn stage(&self, id: StageId, status: Status, detail: Option<&str>) {
        self.update(|snapshot| {
            let stage = snapshot.stage(id);
            let at = now();
            if status != Status::Pending {
                stage.started_at.get_or_insert_with(|| at.clone());
            }
            if matches!(status, Status::Pending | Status::Running) {
                stage.ended_at = None;
            } else {
                stage.ended_at.get_or_insert(at);
            }
            stage.status = status;
            if let Some(detail) = detail {
                stage.detail = Some(clamp(detail, 200));
            }
        });
    }

    pub(super) fn fallback(&self, reference: &str) {
        self.update(|snapshot| snapshot.base_ref = reference.to_owned());
    }

    pub(super) fn checkout(&self, path: &Path, branch: &str) {
        self.update(|snapshot| {
            snapshot.branch = Some(branch.to_owned());
            snapshot.worktree_path = Some(path.to_owned());
            let stage = snapshot.stage(StageId::Submodules);
            if stage.status == Status::Pending {
                stage.status = Status::Skipped;
                stage.detail = Some("none".to_owned());
            }
        });
        self.stage(StageId::Checkout, Status::Done, None);
    }

    pub(super) fn script(&self, script: ScriptInfo) {
        self.update(move |snapshot| snapshot.setup_script = Some(script));
    }

    pub(super) fn append_tail(&self, line: String) {
        self.update(move |snapshot| {
            let tail = &mut snapshot.stage(StageId::SetupScript).tail;
            if tail.len() == TAIL_LIMIT {
                tail.remove(0);
            }
            tail.push(clamp(&line, 400));
        });
    }

    pub(super) fn finish(&self, phase: Phase, error: Option<&str>) {
        self.update(|snapshot| {
            let ended = now();
            snapshot.phase = phase;
            snapshot.ended_at = Some(ended.clone());
            snapshot.error = error.map(|error| clamp(error, 1000));
            for stage in &mut snapshot.stages {
                if stage.status == Status::Running {
                    stage.status = match phase {
                        Phase::Cancelled => Status::Skipped,
                        Phase::Failed => Status::Failed,
                        _ => Status::Done,
                    };
                    stage.ended_at = Some(ended.clone());
                }
            }
        });
        let progress = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FINISHED_RETENTION).await;
            progress.channel.send_if_modified(|state| {
                if state.generation != progress.generation {
                    return false;
                }
                state.snapshot.take().is_some()
            });
        });
    }
}

impl Snapshot {
    fn stage(&mut self, id: StageId) -> &mut Stage {
        self.stages.iter_mut().find(|stage| stage.id == id).unwrap()
    }
}

fn now() -> String {
    super::super::turn_commands::timestamp(OffsetDateTime::now_utc())
}

fn clamp(text: &str, limit: usize) -> String {
    if text.encode_utf16().count() <= limit {
        return text.to_owned();
    }
    let mut remaining = limit - 1;
    let mut result = String::new();
    for character in text.chars() {
        let Some(next) = remaining.checked_sub(character.len_utf16()) else {
            break;
        };
        remaining = next;
        result.push(character);
    }
    result.push('…');
    result
}
