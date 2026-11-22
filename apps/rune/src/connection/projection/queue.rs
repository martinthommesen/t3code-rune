use std::collections::HashMap;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedMessage {
    pub(crate) run_id: String,
    pub(crate) text: String,
    pub(crate) held: bool,
    pub(crate) worktree_queue: bool,
    pub(crate) text_editable: bool,
}

pub(super) fn snapshot(projection: &Value) -> Vec<QueuedMessage> {
    let runs = projection["runs"].as_array();
    let worktree = runs.is_some_and(|runs| {
        runs.iter()
            .any(|run| run["ordinal"] == 1 && run["workspacePreparation"]["type"] == "worktree")
    });
    let messages: HashMap<_, _> = projection["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| Some((message["id"].as_str()?, message)))
        .collect();
    let mut queued: Vec<_> = runs
        .into_iter()
        .flatten()
        .filter(|run| run["status"] == "queued")
        .filter_map(|run| {
            let message = messages.get(run["userMessageId"].as_str()?)?;
            if !message["delegatedCompletion"].is_null() || !message["notification"].is_null() {
                return None;
            }
            let ordinal = run["ordinal"].as_u64()?;
            let position = run["queuePosition"].as_u64().unwrap_or(ordinal);
            let editable = message["context"]["guest"].is_null()
                && message["attachments"].as_array().is_none_or(Vec::is_empty);
            Some((
                position,
                ordinal,
                QueuedMessage {
                    run_id: run["id"].as_str()?.to_owned(),
                    text: message["text"].as_str().unwrap_or_default().to_owned(),
                    held: run["queueHeld"] == true,
                    worktree_queue: worktree && ordinal > 1 && run["providerInstanceId"] == "codex",
                    text_editable: editable,
                },
            ))
        })
        .collect();
    queued.sort_by_key(|(position, ordinal, _)| (*position, *ordinal));
    queued.into_iter().map(|(_, _, message)| message).collect()
}
