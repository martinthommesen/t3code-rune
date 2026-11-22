use std::path::Path;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

pub(super) const DEBOUNCE: Duration = Duration::from_millis(100);

pub(super) fn native(path: &Path) -> Option<(RecommendedWatcher, UnboundedReceiver<()>)> {
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

pub(super) async fn changed(changes: &mut UnboundedReceiver<()>) -> bool {
    if changes.recv().await.is_none() {
        return false;
    }
    loop {
        match tokio::time::timeout(DEBOUNCE, changes.recv()).await {
            Ok(Some(())) => continue,
            Ok(None) => return false,
            Err(_) => return true,
        }
    }
}
