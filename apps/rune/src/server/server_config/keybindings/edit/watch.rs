use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;

use super::Edits;

const DEBOUNCE: Duration = Duration::from_millis(100);

pub(super) fn start(edits: Arc<Edits>, path: PathBuf) -> JoinHandle<()> {
    if let Some((observer, changes)) = native_observer(&path) {
        return tokio::spawn(handle_changes(edits, path, observer, changes));
    }
    tokio::spawn(poll_file(edits, path))
}

fn native_observer(path: &Path) -> Option<(RecommendedWatcher, UnboundedReceiver<()>)> {
    let parent = path.parent()?;
    let directory = parent.canonicalize().unwrap_or_else(|_| parent.to_owned());
    let filename = path.file_name()?.to_owned();
    let (signal, changes) = unbounded_channel();
    let mut observer = recommended_watcher(move |event: notify::Result<Event>| {
        let changed = match event {
            Ok(event) => {
                !event.kind.is_access()
                    && (event.need_rescan()
                        || event
                            .paths
                            .iter()
                            .any(|entry| entry.file_name() == Some(filename.as_os_str())))
            }
            Err(_) => true,
        };
        if changed {
            let _ = signal.send(());
        }
    })
    .ok()?;
    observer
        .watch(&directory, RecursiveMode::NonRecursive)
        .ok()?;
    Some((observer, changes))
}

async fn handle_changes(
    edits: Arc<Edits>,
    path: PathBuf,
    _observer: RecommendedWatcher,
    mut changes: UnboundedReceiver<()>,
) {
    while changes.recv().await.is_some() {
        loop {
            match tokio::time::timeout(DEBOUNCE, changes.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return,
                Err(_) => break,
            }
        }
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
