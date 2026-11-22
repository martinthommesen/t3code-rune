use std::collections::HashMap;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedMessage {
    pub(crate) run_id: String,
    pub(crate) text: String,
    pub(crate) held: bool,
    pub(crate) worktree_queue: bool,
    pub(crate) text_editable: bool,
    pub(crate) can_send_now: bool,
}

pub(super) fn snapshot(projection: &Value) -> Vec<QueuedMessage> {
    let runs = projection["runs"].as_array();
    let first_worktree_run = runs.and_then(|runs| {
        runs.iter()
            .find(|run| run["ordinal"] == 1 && run["workspacePreparation"]["type"] == "worktree")
    });
    let can_send_now =
        first_worktree_run.is_some_and(|run| attached_stopped_worktree(projection, run));
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
            let held = run["queueHeld"] == true;
            let worktree_queue =
                first_worktree_run.is_some() && ordinal > 1 && run["providerInstanceId"] == "codex";
            Some((
                position,
                ordinal,
                QueuedMessage {
                    run_id: run["id"].as_str()?.to_owned(),
                    text: message["text"].as_str().unwrap_or_default().to_owned(),
                    held,
                    worktree_queue,
                    text_editable: editable,
                    can_send_now: can_send_now && worktree_queue && held,
                },
            ))
        })
        .collect();
    queued.sort_by_key(|(position, ordinal, _)| (*position, *ordinal));
    queued.into_iter().map(|(_, _, message)| message).collect()
}

fn attached_stopped_worktree(projection: &Value, run: &Value) -> bool {
    let thread = &projection["thread"];
    let (Some(run_id), Some(provider_id)) = (run["id"].as_str(), run["providerThreadId"].as_str())
    else {
        return false;
    };
    run["status"] == "interrupted"
        && thread["archivedAt"].is_null()
        && thread["deletedAt"].is_null()
        && thread["worktreePath"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
        && thread["activeProviderThreadId"] == provider_id
        && projection["providerThreads"]
            .as_array()
            .is_some_and(|providers| {
                providers.iter().any(|provider| {
                    provider["id"] == provider_id
                        && provider["driver"] == "codex"
                        && provider["providerInstanceId"] == "codex"
                        && provider["status"] == "not_loaded"
                        && provider["nativeThreadRef"].is_null()
                })
            })
        && projection["turnItems"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["runId"] == run_id
                    && item["type"] == "run_interrupt_result"
                    && item["status"] == "interrupted"
                    && item["providerTurnId"].is_null()
                    && item["message"] == "Run interrupted before provider start"
            })
        })
        && projection["runs"].as_array().is_some_and(|runs| {
            !runs.iter().any(|run| {
                matches!(
                    run["status"].as_str(),
                    Some("preparing" | "starting" | "running" | "waiting")
                )
            })
        })
}

pub(super) fn resumable_run(projection: &Value) -> Option<String> {
    let runs = projection["runs"].as_array()?;
    let first = runs.iter().find(|run| run["ordinal"] == 1)?;
    let thread = &projection["thread"];
    let run_id = first["id"].as_str()?;
    let ordinary_source = projection["messages"].as_array()?.iter().any(|message| {
        message["id"] == first["userMessageId"]
            && message["role"] == "user"
            && message["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
            && message["attachments"].as_array().is_none_or(Vec::is_empty)
            && message["notification"].is_null()
    });
    (first["workspacePreparation"]["type"] == "worktree"
        && attached_stopped_worktree(projection, first)
        && ordinary_source
        && thread["forkedFrom"].is_null()
        && thread["lineage"]["parentThreadId"].is_null()
        && thread["lineage"]["rootThreadId"] == thread["id"]
        && thread["modelSelection"]["instanceId"] == "codex"
        && thread["modelSelection"].as_object()?.len() == 2
        && thread["interactionMode"] == "default"
        && runs.iter().all(|run| {
            run["id"] == run_id || matches!(run["status"].as_str(), Some("queued" | "cancelled"))
        })
        && !projection["runtimeRequests"]
            .as_array()?
            .iter()
            .any(|request| request["status"] == "pending"))
    .then(|| run_id.to_owned())
}
