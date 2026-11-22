use std::path::Path;

use tokio::task::JoinHandle;

use super::{SettingsFile, disk};
use crate::server::config_watch::{self, DEBOUNCE};

pub(super) fn start(settings: SettingsFile) -> JoinHandle<()> {
    let observer = if is_symlink(settings.path()) {
        None
    } else {
        config_watch::native(settings.path())
    };
    settings.initialize_watch();
    tokio::spawn(async move {
        if let Some((_observer, mut changes)) = observer {
            while config_watch::changed(&mut changes).await {
                let file = settings.clone();
                match tokio::task::spawn_blocking(move || {
                    file.observe();
                    is_symlink(file.path())
                })
                .await
                {
                    Ok(false) => (),
                    Ok(true) => break,
                    Err(_) => return,
                }
            }
        }
        poll(settings).await;
    })
}

fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.is_symlink())
}

async fn poll(settings: SettingsFile) {
    let mut ticks = tokio::time::interval(DEBOUNCE);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = None;
    let mut pending = false;
    loop {
        ticks.tick().await;
        let path = settings.path().to_owned();
        let Ok(revision) = tokio::task::spawn_blocking(move || disk::revision(&path)).await else {
            return;
        };
        if let Ok(revision) = revision {
            if last.as_ref() != Some(&revision) {
                last = Some(revision);
                pending = true;
            } else if pending {
                let file = settings.clone();
                let _ = tokio::task::spawn_blocking(move || file.observe()).await;
                pending = false;
            }
        }
    }
}
