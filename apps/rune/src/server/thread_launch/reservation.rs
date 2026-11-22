use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::sync::watch;

use super::PreparedLaunch;

type LaunchKey = (PathBuf, String);

static PREPARATIONS: Mutex<BTreeMap<LaunchKey, Active>> = Mutex::new(BTreeMap::new());

struct Active {
    thread_id: String,
    cancellable: bool,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
}

pub(super) struct PreparationReservation {
    key: LaunchKey,
    pub(super) cancelled: watch::Receiver<bool>,
    finished: watch::Sender<bool>,
}

impl PreparationReservation {
    pub(super) fn new(plan: &PreparedLaunch, thread_id: &str) -> Option<Self> {
        let key = (plan.database.clone(), plan.input.command_id.0.clone());
        let mut preparations = PREPARATIONS.lock().unwrap();
        let entry = match preparations.entry(key.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => entry,
            std::collections::btree_map::Entry::Occupied(_) => return None,
        };
        let (cancel, cancelled) = watch::channel(false);
        let (finished, completion) = watch::channel(false);
        entry.insert(Active {
            thread_id: thread_id.to_owned(),
            cancellable: false,
            cancel,
            finished: completion,
        });
        Some(Self {
            key,
            cancelled,
            finished,
        })
    }

    pub(super) fn begin_checkout(&self) {
        if let Some(active) = PREPARATIONS.lock().unwrap().get_mut(&self.key) {
            active.cancellable = true;
        }
    }

    pub(super) fn begin_release(&self) -> bool {
        let mut preparations = PREPARATIONS.lock().unwrap();
        if let Some(active) = preparations.get_mut(&self.key) {
            active.cancellable = false;
        }
        !*self.cancelled.borrow()
    }
}

impl Drop for PreparationReservation {
    fn drop(&mut self) {
        PREPARATIONS.lock().unwrap().remove(&self.key);
        self.finished.send_replace(true);
    }
}

pub(in super::super) async fn cancel_worktree_setup(database: &Path, thread_id: &str) -> bool {
    let finished = PREPARATIONS
        .lock()
        .unwrap()
        .iter()
        .find(|((path, _), active)| {
            path == database && active.thread_id == thread_id && active.cancellable
        })
        .map(|(_, active)| {
            active.cancel.send_replace(true);
            active.finished.clone()
        });
    let Some(mut finished) = finished else {
        return false;
    };
    let _ = finished.wait_for(|done| *done).await;
    true
}
