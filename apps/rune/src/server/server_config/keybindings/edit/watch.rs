use std::path::PathBuf;
use std::sync::Arc;

use notify::RecommendedWatcher;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use super::Edits;

use crate::server::config_watch::{self, DEBOUNCE};

pub(super) fn start(edits: Arc<Edits>, path: PathBuf) -> JoinHandle<()> {
    if let Some((observer, changes)) = config_watch::native(&path) {
        return tokio::spawn(handle_changes(edits, path, observer, changes));
    }
    tokio::spawn(poll_file(edits, path))
}

async fn handle_changes(
    edits: Arc<Edits>,
    path: PathBuf,
    _observer: RecommendedWatcher,
    mut changes: UnboundedReceiver<()>,
) {
    while config_watch::changed(&mut changes).await {
        let edits = Arc::clone(&edits);
        let path = path.clone();
        let _ = tokio::task::spawn_blocking(move || edits.observe(&path)).await;
    }
}

async fn poll_file(edits: Arc<Edits>, path: PathBuf) {
    let mut ticks = tokio::time::interval(DEBOUNCE);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = None;
    let mut pending = false;
    loop {
        ticks.tick().await;
        let source = path.clone();
        let result = tokio::task::spawn_blocking(move || std::fs::read(source).ok()).await;
        let Ok(contents) = result else { return };
        if last.as_ref() != Some(&contents) {
            last = Some(contents);
            pending = true;
        } else if pending {
            let edits = Arc::clone(&edits);
            let path = path.clone();
            let _ = tokio::task::spawn_blocking(move || edits.observe(&path)).await;
            pending = false;
        }
    }
}
