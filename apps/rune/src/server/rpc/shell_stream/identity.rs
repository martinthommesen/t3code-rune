use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio::time::{Instant, timeout_at};

use crate::server::repository_identity::{Change, RepositoryIdentity, digest_projects};

pub(super) fn resume_roots(projects: &[Value], roots: &mut Vec<String>, known: Option<&str>) {
    if known.is_some_and(|known| digest_projects(projects) == known) {
        let missing: HashSet<_> = projects
            .iter()
            .filter(|project| project["repositoryIdentity"].is_null())
            .filter_map(|project| project["workspaceRoot"].as_str())
            .collect();
        roots.retain(|root| missing.contains(root.as_str()));
    }
}

pub(super) fn marked(sequence: u64, projects: &[Value], roots: Vec<String>) -> Value {
    let resolved: HashSet<_> = roots.iter().map(String::as_str).collect();
    let projects: Vec<_> = projects
        .iter()
        .filter(|project| {
            project["workspaceRoot"]
                .as_str()
                .is_some_and(|root| resolved.contains(root))
        })
        .collect();
    json!({
        "kind": "snapshot",
        "snapshot": {
            "schemaVersion": 2,
            "snapshotSequence": sequence,
            "projects": projects,
            "threads": [],
            "archivedThreads": [],
        },
        "resolvedRepositoryIdentityRoots": roots,
    })
}

pub(in crate::server::rpc) async fn batch_changes(
    receiver: &mut Receiver<Change>,
    first: Result<Change, RecvError>,
) -> Option<Vec<Change>> {
    let deadline = Instant::now() + Duration::from_millis(25);
    let mut next = first;
    let mut batch = Vec::with_capacity(64);
    loop {
        match next {
            Ok(change) => batch.push(change),
            Err(RecvError::Lagged(_)) | Err(RecvError::Closed) => return None,
        }
        if batch.len() == 64 {
            break;
        }
        match timeout_at(deadline, receiver.recv()).await {
            Ok(result) => next = result,
            Err(_) => break,
        }
    }
    Some(batch)
}

pub(super) fn changed_projects(projects: &mut [Value], changes: Vec<Change>) -> Vec<String> {
    let mut identities = HashMap::<PathBuf, Option<RepositoryIdentity>>::new();
    let mut roots = Vec::new();
    for change in changes {
        if !identities.contains_key(&change.workspace_root) {
            roots.push(change.workspace_root.to_string_lossy().into_owned());
        }
        identities.insert(change.workspace_root, change.identity);
    }
    for project in projects {
        if let Some(identity) = project["workspaceRoot"]
            .as_str()
            .and_then(|root| identities.get(Path::new(root)))
        {
            project["repositoryIdentity"] = json!(identity);
        }
    }
    roots
}
