use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, Semaphore, broadcast};
use tokio::time::Instant;

use super::{Checkout, RepositoryIdentity, git, ssh};

const CACHE_CAPACITY: usize = 512;
const POSITIVE_TTL: Duration = Duration::from_secs(15 * 60);
const NEGATIVE_TTL: Duration = Duration::from_secs(60);
const AVAILABLE_TTL: Duration = Duration::from_secs(60);
const WORKERS: usize = 4;
const MAX_PENDING: usize = 512;
const CHANGE_BUFFER: usize = 256;

#[derive(Default)]
pub(in crate::server) struct Resolver {
    roots: Memo<PathBuf, Checkout>,
    identities: Memo<Checkout, RepositoryIdentity>,
    ssh: ssh::Config,
}

impl Resolver {
    pub(in crate::server) async fn resolve(
        &self,
        workspace_root: &Path,
        refresh: bool,
    ) -> Option<RepositoryIdentity> {
        let requested = workspace_root.to_owned();
        let root = self
            .roots
            .resolve(&requested, refresh, || git::checkout(&requested))
            .await?;
        self.identities
            .resolve(&root, refresh, || git::lookup(&root, &self.ssh))
            .await
    }

    #[cfg(test)]
    pub(in crate::server) fn isolated_ssh(config: PathBuf) -> Self {
        Self {
            ssh: ssh::Config::isolated(config),
            ..Self::default()
        }
    }
}

type Slot<T> = Arc<AsyncMutex<Option<Cached<T>>>>;

struct Memo<K, T>(Mutex<VecDeque<(K, Slot<T>)>>);

impl<K, T> Default for Memo<K, T> {
    fn default() -> Self {
        Self(Mutex::new(VecDeque::new()))
    }
}

struct Cached<T> {
    value: Option<T>,
    deadline: Instant,
}

impl<K: Clone + Eq, T: Clone> Memo<K, T> {
    async fn resolve<F, Fut>(&self, key: &K, refresh: bool, load: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<T>>,
    {
        let slot = {
            let mut entries = self.0.lock().expect("repository identity cache");
            let index = entries.iter().position(|(existing, _)| existing == key);
            let entry = index
                .and_then(|index| entries.remove(index))
                .unwrap_or_else(|| (key.clone(), Arc::new(AsyncMutex::new(None))));
            let slot = Arc::clone(&entry.1);
            entries.push_back(entry);
            while entries.len() > CACHE_CAPACITY {
                let Some(index) = entries
                    .iter()
                    .position(|(_, slot)| Arc::strong_count(slot) == 1)
                else {
                    break;
                };
                entries.remove(index);
            }
            slot
        };
        let mut cached = slot.lock().await;
        if !refresh
            && let Some(entry) = cached
                .as_ref()
                .filter(|entry| entry.deadline > Instant::now())
        {
            return entry.value.clone();
        }
        let value = load().await;
        let ttl = if value.is_some() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        *cached = Some(Cached {
            value: value.clone(),
            deadline: Instant::now() + ttl,
        });
        value
    }
}

#[derive(Clone, Default)]
pub(in crate::server) struct Available {
    pub(in crate::server) identity: Option<RepositoryIdentity>,
    pub(in crate::server) resolved: bool,
}

#[derive(Clone)]
pub(in crate::server) struct Change {
    pub(in crate::server) workspace_root: PathBuf,
    pub(in crate::server) identity: Option<RepositoryIdentity>,
}

#[derive(Clone)]
pub(in crate::server) struct Identities {
    inner: Arc<Inner>,
}

struct Inner {
    resolver: Resolver,
    state: Mutex<WarmState>,
    workers: Semaphore,
    changes: broadcast::Sender<Change>,
}

#[derive(Default)]
struct WarmState {
    entries: HashMap<PathBuf, WarmEntry>,
    pending: usize,
    next: u64,
}

#[derive(Default)]
struct WarmEntry {
    identity: Option<RepositoryIdentity>,
    deadline: Option<Instant>,
    task: Option<u64>,
    updated: u64,
}

impl Default for Identities {
    fn default() -> Self {
        Self::new(Resolver::default())
    }
}

impl Identities {
    fn new(resolver: Resolver) -> Self {
        Self {
            inner: Arc::new(Inner {
                resolver,
                state: Mutex::new(WarmState::default()),
                workers: Semaphore::new(WORKERS),
                changes: broadcast::channel(CHANGE_BUFFER).0,
            }),
        }
    }

    pub(in crate::server) fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.inner.changes.subscribe()
    }

    /// Read a warm value immediately and queue a cold or expired workspace.
    pub(in crate::server) fn get_available(&self, workspace_root: &Path) -> Available {
        let available = {
            let state = self
                .inner
                .state
                .lock()
                .expect("repository identity enrichment");
            state
                .entries
                .get(workspace_root)
                .map_or_else(Available::default, |entry| Available {
                    identity: entry.identity.clone(),
                    resolved: entry
                        .deadline
                        .is_some_and(|deadline| deadline > Instant::now()),
                })
        };
        if !available.resolved {
            self.schedule(workspace_root, false);
        }
        available
    }

    pub(in crate::server) fn request_refresh(&self, workspace_root: &Path) {
        self.schedule(workspace_root, true);
    }

    fn schedule(&self, workspace_root: &Path, refresh: bool) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let task = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("repository identity enrichment");
            if state.pending >= MAX_PENDING {
                return;
            }
            if !refresh
                && state.entries.get(workspace_root).is_some_and(|entry| {
                    entry.task.is_some()
                        || entry
                            .deadline
                            .is_some_and(|deadline| deadline > Instant::now())
                })
            {
                return;
            }
            state.next += 1;
            let task = state.next;
            state.pending += 1;
            let entry = state.entries.entry(workspace_root.to_owned()).or_default();
            if refresh {
                entry.deadline = None;
            }
            entry.task = Some(task);
            entry.updated = task;
            task
        };
        let inner = Arc::clone(&self.inner);
        let workspace_root = workspace_root.to_owned();
        runtime.spawn(async move {
            let Ok(_permit) = inner.workers.acquire().await else {
                return;
            };
            let identity = inner.resolver.resolve(&workspace_root, refresh).await;
            let mut state = inner.state.lock().expect("repository identity enrichment");
            state.pending -= 1;
            let Some(entry) = state
                .entries
                .get_mut(&workspace_root)
                .filter(|entry| entry.task == Some(task))
            else {
                return;
            };
            entry.identity.clone_from(&identity);
            entry.deadline = Some(Instant::now() + AVAILABLE_TTL);
            entry.task = None;
            while state.entries.len() > CACHE_CAPACITY {
                let Some(oldest) = state
                    .entries
                    .iter()
                    .filter(|(root, entry)| {
                        root.as_path() != workspace_root && entry.task.is_none()
                    })
                    .min_by_key(|(_, entry)| entry.updated)
                    .map(|(root, _)| root.clone())
                else {
                    break;
                };
                state.entries.remove(&oldest);
            }
            let _ = inner.changes.send(Change {
                workspace_root,
                identity,
            });
        });
    }

    #[cfg(test)]
    pub(in crate::server) fn isolated_ssh(config: PathBuf) -> Self {
        Self::new(Resolver::isolated_ssh(config))
    }
}
