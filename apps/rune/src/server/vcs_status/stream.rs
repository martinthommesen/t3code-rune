use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio::task::AbortHandle;
use tokio::time::{Instant, MissedTickBehavior};

use super::{
    GitManagerError, Input, LocalStatus, ReadOptions, RefreshInput, RefreshStatus, RemoteStatus,
    git, read, snapshot, totals,
};

const CACHED_CHECKOUTS: usize = 256;
const BUFFERED_EVENTS: usize = 16;
const HEAD_POLL: Duration = Duration::from_millis(200);
const TRACKING_POLL: Duration = Duration::from_secs(30);
const MAX_HEAD_BYTES: u64 = 64 * 1024;

#[derive(Clone, Default)]
pub(in crate::server) struct Service {
    checkouts: Arc<Mutex<VecDeque<Arc<Entry>>>>,
}

impl Service {
    pub(in crate::server) async fn subscribe(
        &self,
        input: Input,
    ) -> Result<Subscription, GitManagerError> {
        let entry = self.entry(&input.cwd).await?;
        let mut state = entry.state.lock().await;
        let observed_head = entry.head_contents().await;
        let details = input.include_remote || entry.wants_remote();
        if state.local.is_none() || (details && !state.detailed) || state.head != observed_head {
            let (parts, _) = read(
                &entry.cwd,
                ReadOptions {
                    tracking: false,
                    branch_changes: details,
                },
            )
            .await?;
            state.update_local(&entry, parts.local, details);
            state.head = observed_head;
        }
        let initial = state.snapshot();
        let updates = entry.updates.subscribe();
        entry.retain(input.include_remote);
        drop(state);
        Ok(Subscription {
            initial,
            updates,
            entry,
            remote: input.include_remote,
        })
    }

    pub(in crate::server) async fn refresh(
        &self,
        input: RefreshInput,
    ) -> Result<RefreshStatus, GitManagerError> {
        let entry = self.entry(&input.cwd).await?;
        let mut state = entry.state.lock().await;
        let observed_head = entry.head_contents().await;
        let (parts, checkout) = read(
            &entry.cwd,
            ReadOptions {
                tracking: !input.local_only,
                branch_changes: true,
            },
        )
        .await?;
        let refs_digest = match checkout {
            Some(checkout) => totals::refs_digest(&entry.cwd, &checkout).await,
            None => None,
        };
        let local = parts.local.clone();
        state.update_local(&entry, parts.local, true);
        state.head = observed_head;
        if !input.local_only {
            state.update_remote(&entry, parts.remote);
        }
        Ok(RefreshStatus {
            local,
            remote: state
                .remote
                .as_ref()
                .and_then(Clone::clone)
                .unwrap_or_default(),
            refs_digest,
        })
    }

    async fn entry(&self, requested: &str) -> Result<Arc<Entry>, GitManagerError> {
        let requested = Path::new(requested);
        let checkout = git::checkout(requested).await?;
        let cwd = match checkout.as_ref() {
            Some(checkout) => checkout.root.clone(),
            None => requested
                .canonicalize()
                .unwrap_or_else(|_| requested.to_owned()),
        };
        let head = checkout.map(|checkout| checkout.index.with_file_name("HEAD"));
        let mut checkouts = self.checkouts.lock().expect("VCS checkout registry");
        let position = checkouts.iter().position(|existing| existing.cwd == cwd);
        let entry = position
            .and_then(|position| checkouts.remove(position))
            .unwrap_or_else(|| {
                Arc::new(Entry {
                    cwd,
                    head: Mutex::new(None),
                    state: AsyncMutex::new(State::default()),
                    updates: broadcast::channel(BUFFERED_EVENTS).0,
                    demand: Mutex::new(Demand::default()),
                })
            });
        if head.is_some() {
            *entry.head.lock().expect("VCS checkout HEAD path") = head;
        }
        checkouts.push_back(Arc::clone(&entry));
        while checkouts.len() > CACHED_CHECKOUTS {
            let Some(index) = checkouts
                .iter()
                .position(|entry| Arc::strong_count(entry) == 1)
            else {
                break;
            };
            checkouts.remove(index);
        }
        Ok(entry)
    }
}

pub(in crate::server) struct Subscription {
    pub(in crate::server) initial: Value,
    updates: broadcast::Receiver<Value>,
    entry: Arc<Entry>,
    remote: bool,
}

impl Subscription {
    pub(in crate::server) async fn next(&mut self) -> Option<Value> {
        match self.updates.recv().await {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Closed) => None,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let state = self.entry.state.lock().await;
                self.updates = self.entry.updates.subscribe();
                Some(state.snapshot())
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut demand = self.entry.demand.lock().expect("VCS watcher demand");
        demand.subscribers -= 1;
        demand.remote -= usize::from(self.remote);
        if demand.subscribers == 0
            && let Some(task) = demand.task.take()
        {
            task.abort();
        }
    }
}

struct Entry {
    cwd: PathBuf,
    head: Mutex<Option<PathBuf>>,
    state: AsyncMutex<State>,
    updates: broadcast::Sender<Value>,
    demand: Mutex<Demand>,
}

#[derive(Default)]
struct Demand {
    subscribers: usize,
    remote: usize,
    task: Option<AbortHandle>,
}

impl Entry {
    fn retain(self: &Arc<Self>, remote: bool) {
        let mut demand = self.demand.lock().expect("VCS watcher demand");
        demand.subscribers += 1;
        demand.remote += usize::from(remote);
        if demand.task.is_none() {
            let worker = tokio::spawn(watch(Arc::downgrade(self)));
            demand.task = Some(worker.abort_handle());
        }
    }

    fn wants_remote(&self) -> bool {
        self.demand.lock().expect("VCS watcher demand").remote > 0
    }

    async fn head_contents(&self) -> HeadContents {
        let path = self.head.lock().expect("VCS checkout HEAD path").clone();
        let Some(path) = path else {
            return HeadContents::Unavailable;
        };
        tokio::task::spawn_blocking(move || read_head(&path))
            .await
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct State {
    local: Option<LocalStatus>,
    detailed: bool,
    // The outer option distinguishes a cold cache from a confirmed absent remote status.
    remote: Option<Option<RemoteStatus>>,
    tracking_at: Option<Instant>,
    head: HeadContents,
}

impl State {
    fn snapshot(&self) -> Value {
        snapshot(
            self.local.clone().unwrap_or_default(),
            self.remote.clone().flatten(),
        )
    }

    fn update_local(&mut self, entry: &Entry, local: LocalStatus, detailed: bool) {
        let branch_changed = self.local.as_ref().is_some_and(|previous| {
            previous.ref_name != local.ref_name || previous.is_repo != local.is_repo
        });
        if branch_changed {
            if self.remote.take().is_some() {
                let _ = entry
                    .updates
                    .send(json!({"_tag":"remoteUpdated","remote":null}));
            }
            self.tracking_at = None;
        }
        if self.local.as_ref() != Some(&local) {
            let _ = entry
                .updates
                .send(json!({"_tag":"localUpdated","local":local}));
        }
        self.local = Some(local);
        self.detailed = detailed;
    }

    fn update_remote(&mut self, entry: &Entry, remote: Option<RemoteStatus>) {
        if self.remote.as_ref() != Some(&remote) {
            let _ = entry
                .updates
                .send(json!({"_tag":"remoteUpdated","remote":remote}));
        }
        self.remote = Some(remote);
        self.tracking_at = Some(Instant::now());
    }
}

#[derive(Default, PartialEq, Eq)]
enum HeadContents {
    #[default]
    Unavailable,
    File(Vec<u8>),
}

fn read_head(path: &Path) -> HeadContents {
    if !path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= MAX_HEAD_BYTES)
    {
        return HeadContents::Unavailable;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return HeadContents::Unavailable;
    };
    let mut contents = Vec::new();
    if file
        .take(MAX_HEAD_BYTES + 1)
        .read_to_end(&mut contents)
        .is_err()
        || contents.len() as u64 > MAX_HEAD_BYTES
    {
        return HeadContents::Unavailable;
    }
    HeadContents::File(contents)
}

async fn watch(entry: Weak<Entry>) {
    let mut interval = tokio::time::interval(HEAD_POLL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(entry) = entry.upgrade() else { return };
        let mut state = entry.state.lock().await;
        let observed_head = entry.head_contents().await;
        let checkout_changed = state.head != observed_head;
        let include_remote = entry.wants_remote();
        let tracking_due = include_remote
            && state.local.as_ref().is_some_and(|local| local.is_repo)
            && state
                .tracking_at
                .is_none_or(|last| last.elapsed() >= TRACKING_POLL);
        if !checkout_changed && !tracking_due {
            continue;
        }
        let track = tracking_due || state.remote.is_some();
        let Ok((parts, _)) = read(
            &entry.cwd,
            ReadOptions {
                tracking: track,
                branch_changes: include_remote,
            },
        )
        .await
        else {
            continue;
        };
        state.update_local(&entry, parts.local, include_remote);
        state.head = observed_head;
        if track {
            state.update_remote(&entry, parts.remote);
        }
    }
}
