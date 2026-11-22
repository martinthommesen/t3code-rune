use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio::task::{AbortHandle, JoinSet};

use super::credentials::Permissions;
use super::filesystem_browse::{self, BrowseInput};
use super::launch_model;
use super::named_projects::{self, CreateNamedProject};
use super::project_clone::StartInput as CloneStartInput;
use super::project_commands::CommandError as ProjectCommandError;
use super::repository_identity::{Change as IdentityChange, Identities};
use super::store::Store;
use super::terminal_rpc;
use super::thread_commands::ThreadCommandError;
use super::thread_launch::{self, LaunchContext, LaunchInput};
use super::turn_commands::MessageDispatch;
use super::turn_commands::interrupt_effect::ThreadStop;
use super::turn_commands::runtime_requests::RuntimeResponse;
use super::vcs_status;
use super::{LocalOperations, ProjectionChange, SharedEnvironment, server_config};
use super::{vcs_checkout, vcs_refs};

mod editor_links;
mod queued_run;
mod shell_stream;
mod streams;

#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
enum RequestId {
    String(String),
    Number(u64),
}

#[derive(Deserialize)]
#[serde(tag = "_tag")]
enum ClientMessage {
    Request {
        id: RequestId,
        tag: String,
        payload: Value,
    },
    Interrupt {
        #[serde(rename = "requestId")]
        request_id: RequestId,
    },
    Ack {
        #[serde(rename = "requestId")]
        request_id: Option<RequestId>,
    },
    Ping,
    Eof,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRequest {
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRequest {
    project_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandOutputRequest {
    thread_id: String,
    item_id: String,
}

#[derive(Serialize)]
#[serde(tag = "_tag")]
enum RpcError<'a> {
    OrchestrationV2GetShellSnapshotError {
        message: &'a str,
    },
    OrchestrationV2GetThreadProjectionError {
        #[serde(rename = "threadId")]
        thread_id: &'a str,
        message: &'a str,
    },
    EnvironmentAuthorizationError {
        #[serde(rename = "requiredScope")]
        required_scope: &'a str,
        message: &'a str,
    },
    InvalidRequest {
        message: &'a str,
    },
    ProjectMutationError {
        #[serde(rename = "commandId")]
        command_id: &'a str,
        message: &'a str,
    },
    OrchestrationV2DispatchCommandError {
        #[serde(rename = "commandId")]
        command_id: &'a str,
        #[serde(rename = "commandType")]
        command_type: &'a str,
        message: &'a str,
    },
    ReadOnlyEnvironment {
        message: &'a str,
    },
}

pub(super) async fn serve(
    mut socket: WebSocket,
    environment: SharedEnvironment,
    permissions: Permissions,
) {
    let store = environment.store.clone();
    let local = environment.local.clone();
    let mut changes = local
        .as_ref()
        .map(|local| local.projection_changes.subscribe());
    let identities = local
        .as_ref()
        .map(|local| local.repository_identities.clone());
    let mut identity_changes: Option<Receiver<IdentityChange>> = None;
    let mut shell_requests = HashMap::<RequestId, shell_stream::Subscription>::new();
    let mut shell_completions = HashSet::<RequestId>::new();
    let mut thread_requests = HashMap::<RequestId, String>::new();
    let mut streams = streams::Streams::new();
    let mut operations = JoinSet::new();
    let mut operation_requests = HashMap::<RequestId, AbortHandle>::new();
    loop {
        let message = tokio::select! {
            message = socket.recv() => message,
            frame = streams.next() => {
                if send(&mut socket, frame).await { continue; }
                break;
            }
            completed = operations.join_next(), if !operations.is_empty() => {
                match completed {
                    Some(Ok((id, frame))) => {
                        operation_requests.remove(&id);
                        if !send(&mut socket, frame).await { break; }
                    }
                    Some(Err(error)) if !error.is_cancelled() => break,
                    _ => {}
                }
                continue;
            }
            change = async { changes.as_mut().expect("local host has a projection change stream").recv().await },
                if changes.is_some() && (!shell_requests.is_empty() || !thread_requests.is_empty()) => {
                let thread_ids = match change {
                    Ok(ProjectionChange::Project) => Vec::new(),
                    Ok(ProjectionChange::Thread(thread_id)) => vec![thread_id],
                    Err(RecvError::Lagged(_)) => thread_requests.values().cloned().collect(),
                    Err(RecvError::Closed) => break,
                };
                if refresh_projections(
                    &mut socket,
                    &store,
                    identities.as_ref().expect("local projection changes have an identity service"),
                    &mut shell_requests,
                    &mut thread_requests,
                    &thread_ids,
                ).await { continue; }
                break;
            }
            change = async { identity_changes.as_mut().expect("local identity stream").recv().await },
                if identity_changes.is_some() && !shell_requests.is_empty() => {
                let changes = shell_stream::batch_changes(
                    identity_changes.as_mut().expect("local identity stream"),
                    change,
                ).await;
                let store = store.clone();
                let identities = identities.clone().expect("local identity service");
                let Ok(Ok(enrichment)) = tokio::task::spawn_blocking(move || {
                    shell_stream::Enrichment::read(&store, &identities, changes)
                }).await else { break; };
                for shell in shell_requests.values_mut() {
                    if let Some(frame) = shell.enrichment_frame(&enrichment)
                        && !send(&mut socket, frame).await
                    {
                        return;
                    }
                }
                continue;
            }
        };
        let Some(Ok(message)) = message else { break };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) | Message::Binary(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        let Ok(message) = serde_json::from_str::<ClientMessage>(text.as_str()) else {
            break;
        };
        let response = match message {
            ClientMessage::Request { id, tag, payload }
                if tag == "subscribeServerConfig" && local.is_some() =>
            {
                if let Err(message) = server_config::validate_input(&payload) {
                    Ok(defect(id, message))
                } else if operation_requests.contains_key(&id) {
                    Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    ))
                } else {
                    streams.start_server_config(id, Arc::clone(&environment));
                    continue;
                }
            }
            ClientMessage::Request { id, tag, payload }
                if matches!(
                    tag.as_str(),
                    "server.upsertKeybinding" | "server.removeKeybinding"
                ) && local.is_some() =>
            {
                match server_config::KeybindingEdit::decode(
                    &payload,
                    tag == "server.removeKeybinding",
                ) {
                    Err(message) => Ok(defect(id, message)),
                    Ok(_) if !permissions.settings_write => Ok(failure(
                        id,
                        json!({
                            "_tag": "EnvironmentAuthorizationError",
                            "requiredScope": "orchestration:operate",
                            "requiredPermission": "settings:write",
                            "message": "The authenticated token is missing required scope: settings:write."
                        }),
                    )),
                    Ok(_) if operation_requests.contains_key(&id) => Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    )),
                    Ok(edit) => {
                        let local = Arc::clone(local.as_ref().expect("local operations"));
                        let request_id = id.clone();
                        let task = operations.spawn(async move {
                            let path = local.settings.path().with_file_name("keybindings.json");
                            let result = tokio::task::spawn_blocking(move || {
                                local.config.keybindings.update(&path, edit)
                            })
                            .await;
                            let response = match result {
                                Ok(Ok(value)) => success(id.clone(), value),
                                Ok(Err(error)) => failure(id.clone(), error),
                                Err(_) => failure(
                                    id.clone(),
                                    RpcError::InvalidRequest {
                                        message: "Could not change the environment keybindings.",
                                    },
                                ),
                            };
                            (id, response)
                        });
                        operation_requests.insert(request_id, task);
                        continue;
                    }
                }
            }
            ClientMessage::Request { id, tag, payload }
                if tag == "projects.searchEntries" && local.is_some() =>
            {
                use super::workspace_search::Input;
                match Input::decode(&payload) {
                    Err(message) => Ok(defect(id, message)),
                    Ok(_) if !permissions.filesystem_read => Ok(failure(
                        id,
                        json!({
                            "_tag": "EnvironmentAuthorizationError",
                            "requiredScope": "orchestration:read",
                            "requiredPermission": "filesystem:read",
                            "message": "The authenticated token is missing required scope: filesystem:read."
                        }),
                    )),
                    Ok(_) if operation_requests.contains_key(&id) => Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    )),
                    Ok(input) => {
                        let local = Arc::clone(local.as_ref().expect("local operations"));
                        let request_id = id.clone();
                        let task = operations.spawn(async move {
                            let result = match local.workspace_search.search(input).await {
                                Ok(value) => success(id.clone(), value),
                                Err(error) => failure(id.clone(), error),
                            };
                            (id, result)
                        });
                        operation_requests.insert(request_id, task);
                        continue;
                    }
                }
            }
            ClientMessage::Request { id, tag, payload }
                if tag == "assets.createUrl"
                    && local.is_some()
                    && super::workspace_attachment::Input::applies(&payload) =>
            {
                match super::workspace_attachment::Input::decode(&payload) {
                    _ if !permissions.filesystem_read => Ok(failure(
                        id,
                        json!({
                            "_tag": "EnvironmentAuthorizationError",
                            "requiredScope": "orchestration:read",
                            "requiredPermission": "filesystem:read",
                            "message": "The authenticated token is missing required scope: filesystem:read."
                        }),
                    )),
                    None => Ok(defect(
                        id,
                        "Invalid workspace attachment resource.".to_owned(),
                    )),
                    Some(_) if operation_requests.contains_key(&id) => Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    )),
                    Some(input) => {
                        let local = Arc::clone(local.as_ref().expect("local operations"));
                        let store = store.clone();
                        let request_id = id.clone();
                        let task = operations.spawn(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                let state_dir = local
                                    .settings
                                    .path()
                                    .parent()
                                    .expect("local state directory");
                                local.workspace_attachments.issue(&store, state_dir, input)
                            })
                            .await;
                            let frame = match result {
                                Ok(Ok(asset)) => success(id.clone(), asset),
                                Ok(Err(error)) => failure(id.clone(), error),
                                Err(_) => failure(
                                    id.clone(),
                                    RpcError::InvalidRequest {
                                        message: "Could not prepare the workspace attachment.",
                                    },
                                ),
                            };
                            (id, frame)
                        });
                        operation_requests.insert(request_id, task);
                        continue;
                    }
                }
            }
            ClientMessage::Request { id, tag, payload }
                if local.is_some()
                    && matches!(
                        tag.as_str(),
                        "filesystem.getMetadata" | "shell.openInEditor"
                    ) =>
            {
                match editor_links::Call::decode(&tag, payload) {
                    Err(message) => Ok(defect(id, message)),
                    Ok(call) => {
                        if let Some(error) = call.denied(permissions) {
                            Ok(failure(id, error))
                        } else if operation_requests.contains_key(&id) {
                            Ok(failure(
                                id,
                                RpcError::InvalidRequest {
                                    message: "Request ID is already in use.",
                                },
                            ))
                        } else {
                            let settings = &local.as_ref().expect("local operations").settings;
                            let state_dir = settings
                                .path()
                                .parent()
                                .expect("local state directory")
                                .to_owned();
                            let request_id = id.clone();
                            let task = operations.spawn(async move {
                                let frame = match call.run(state_dir).await {
                                    Ok(value) => success(id.clone(), value),
                                    Err(error) => failure(id.clone(), error),
                                };
                                (id, frame)
                            });
                            operation_requests.insert(request_id, task);
                            continue;
                        }
                    }
                }
            }
            ClientMessage::Request { id, tag, payload }
                if tag == "projects.readFile" && local.is_some() =>
            {
                match super::workspace_file::Input::decode(&payload) {
                    Err(message) => Ok(defect(id, message)),
                    Ok(_) if !permissions.filesystem_read => Ok(failure(
                        id,
                        json!({
                            "_tag": "EnvironmentAuthorizationError",
                            "requiredScope": "orchestration:read",
                            "requiredPermission": "filesystem:read",
                            "message": "The authenticated token is missing required scope: filesystem:read."
                        }),
                    )),
                    Ok(_) if operation_requests.contains_key(&id) => Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    )),
                    Ok(input) => {
                        let settings = &local.as_ref().expect("local operations").settings;
                        let state_dir = settings
                            .path()
                            .parent()
                            .expect("local state directory")
                            .to_owned();
                        let request_id = id.clone();
                        let task = operations.spawn(async move {
                            let result = match super::workspace_file::read(input, state_dir).await {
                                Ok(value) => success(id.clone(), value),
                                Err(error) => failure(id.clone(), error),
                            };
                            (id, result)
                        });
                        operation_requests.insert(request_id, task);
                        continue;
                    }
                }
            }
            ClientMessage::Request { id, tag, payload }
                if tag == "orchestration.subscribeShell" =>
            {
                let store = store.clone();
                let identities = identities.clone();
                if identity_changes.is_none()
                    && let Some(service) = &identities
                {
                    identity_changes = Some(service.subscribe());
                }
                let Ok(update) = tokio::task::spawn_blocking(move || {
                    shell_stream::Subscription::start(&store, id, payload, identities.as_ref())
                })
                .await
                else {
                    break;
                };
                if let Some(subscription) = update.subscription {
                    shell_requests.insert(subscription.id.clone(), subscription);
                } else if shell_requests.is_empty() {
                    identity_changes = None;
                }
                if let Some(request_id) = update.complete_on_ack {
                    shell_completions.insert(request_id);
                }
                let Some(frame) = update.frame else { continue };
                Ok(frame)
            }
            ClientMessage::Request { id, tag, payload }
                if tag == "orchestration.subscribeCommandOutput" =>
            {
                match serde_json::from_value::<CommandOutputRequest>(payload) {
                    Ok(input) if local.is_some() => {
                        let queue = local
                            .as_ref()
                            .expect("local operations are present")
                            .turn_queue
                            .clone();
                        streams.start_command_output(id, queue, input.thread_id, input.item_id);
                        continue;
                    }
                    Ok(input) => Ok(failure(
                        id,
                        json!({
                            "_tag":"OrchestrationV2CommandOutputError",
                            "threadId":input.thread_id,
                            "message":"No such command.",
                        }),
                    )),
                    Err(_) => Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "A thread and command item ID are required.",
                        },
                    )),
                }
            }
            ClientMessage::Request { id, tag, payload } if tag == "subscribeVcsStatus" => {
                match vcs_status::Input::parse(payload) {
                    Ok(input) if local.is_some() => {
                        let service = local
                            .as_ref()
                            .expect("local operations are present")
                            .vcs_status
                            .clone();
                        streams.start_vcs_status(id, service, input);
                        continue;
                    }
                    Ok(_) => Ok(vcs_status_unavailable(id)),
                    Err(message) => Ok(defect(id, message)),
                }
            }
            ClientMessage::Request { id, tag, payload } if tag == "vcs.refreshStatus" => {
                if operation_requests.contains_key(&id) {
                    Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    ))
                } else {
                    match vcs_status::RefreshInput::parse(payload) {
                        Ok(input) if local.is_some() => {
                            let service = local
                                .as_ref()
                                .expect("local operations are present")
                                .vcs_status
                                .clone();
                            let request_id = id.clone();
                            let task = operations.spawn(async move {
                                let response = match service.refresh(input).await {
                                    Ok(status) => success(id.clone(), status),
                                    Err(error) => failure(id.clone(), error),
                                };
                                (id, response)
                            });
                            operation_requests.insert(request_id, task);
                            continue;
                        }
                        Ok(_) => Ok(vcs_status_unavailable(id)),
                        Err(message) => Ok(defect(id, message)),
                    }
                }
            }
            ClientMessage::Request { id, tag, payload } if tag == "vcs.listRefs" => {
                if local.is_none() {
                    Ok(failure(
                        id,
                        RpcError::EnvironmentAuthorizationError {
                            required_scope: "orchestration:read",
                            message: "Git references are unavailable in the history server.",
                        },
                    ))
                } else if operation_requests.contains_key(&id) {
                    Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    ))
                } else {
                    match vcs_refs::Input::parse(payload) {
                        Ok(input) => {
                            let request_id = id.clone();
                            let task = operations.spawn(async move {
                                let response = match vcs_refs::list(input).await {
                                    Ok(refs) => success(id.clone(), refs),
                                    Err(error) => failure(id.clone(), error),
                                };
                                (id, response)
                            });
                            operation_requests.insert(request_id, task);
                            continue;
                        }
                        Err(error) => Ok(defect(id, error)),
                    }
                }
            }
            ClientMessage::Request { id, tag, payload }
                if matches!(
                    tag.as_str(),
                    "projects.createNew"
                        | "projects.ensureScratch"
                        | "orchestration.launchThread"
                        | "t3.getDefaultCodexModel"
                        | "worktreeSetup.cancel"
                        | "vcs.switchRef"
                        | "vcs.createRef"
                        | "projectClone.start"
                        | "projectClone.cancel"
                        | "projectClone.retry"
                ) || (tag == "projects.mutate" && payload["type"] == "project.delete")
                    || (tag == "orchestration.dispatchCommand"
                        && payload["type"] == thread_launch::RETRY_COMMAND) =>
            {
                let clone_operation = tag.starts_with("projectClone.");
                let git_write = matches!(tag.as_str(), "vcs.switchRef" | "vcs.createRef");
                let (scope, allowed) = if clone_operation || git_write {
                    ("source-control:write", permissions.source_control_write)
                } else {
                    ("orchestration:operate", permissions.orchestration_operate)
                };
                if operation_requests.contains_key(&id) {
                    Ok(failure(
                        id,
                        RpcError::InvalidRequest {
                            message: "Request ID is already in use.",
                        },
                    ))
                } else if let Some(local) = local.as_ref().filter(|_| allowed) {
                    let local = Arc::clone(local);
                    let request_id = id.clone();
                    let store = store.clone();
                    let task = operations.spawn(async move {
                        let response = match tag.as_str() {
                            "projects.createNew" => {
                                create_named_project(&local, id.clone(), payload).await
                            }
                            "projects.ensureScratch" => {
                                ensure_scratch_project(&local, id.clone(), payload).await
                            }
                            "t3.getDefaultCodexModel" => {
                                default_codex_model(&local, id.clone(), payload).await
                            }
                            "orchestration.launchThread" => {
                                launch_thread(&local, &store, id.clone(), payload).await
                            }
                            "orchestration.dispatchCommand" => {
                                retry_preparation(&local, &store, id.clone(), payload).await
                            }
                            "worktreeSetup.cancel" => {
                                cancel_worktree_setup(&store, id.clone(), payload).await
                            }
                            "vcs.switchRef" => {
                                switch_git_ref(&local, &store, id.clone(), payload).await
                            }
                            "vcs.createRef" => {
                                create_git_ref(&local, &store, id.clone(), payload).await
                            }
                            "projects.mutate" => delete_project(local, id.clone(), payload).await,
                            _ => clone_project(&local, id.clone(), &tag, payload).await,
                        };
                        (id, response)
                    });
                    operation_requests.insert(request_id, task);
                    continue;
                } else if git_write {
                    Ok(failure(
                        id,
                        json!({
                            "_tag":"EnvironmentAuthorizationError",
                            "requiredScope":"orchestration:operate",
                            "requiredPermission":"source-control:write",
                            "message":"The authenticated token is missing required scope: source-control:write."
                        }),
                    ))
                } else if matches!(
                    tag.as_str(),
                    "projects.mutate" | "projects.ensureScratch" | "orchestration.dispatchCommand"
                ) {
                    Ok(failure(
                        id,
                        json!({
                            "_tag":"EnvironmentAuthorizationError",
                            "requiredScope":scope, "requiredPermission":scope,
                            "message":"The authenticated token is missing required scope: orchestration:operate."
                        }),
                    ))
                } else {
                    Ok(failure(
                        id,
                        RpcError::EnvironmentAuthorizationError {
                            required_scope: scope,
                            message: if clone_operation {
                                "This session cannot manage repository clones."
                            } else if tag == "worktreeSetup.cancel" {
                                "This session cannot cancel workspace setup."
                            } else if tag == "projects.createNew" {
                                "This session cannot create a project."
                            } else {
                                "This session cannot launch a thread."
                            },
                        },
                    ))
                }
            }
            ClientMessage::Request { id, tag, payload } if tag == "subscribeProjectClones" => {
                if !payload.is_object() {
                    Ok(defect(id, "A clone subscription expects an object.".into()))
                } else if let Some(local) = local.as_ref() {
                    Ok(streams.start_project_clones(id, local.project_clones.subscribe()))
                } else {
                    Ok(chunk(id, json!([])))
                }
            }
            ClientMessage::Request { id, tag, payload } if tag == "subscribeWorktreeSetup" => {
                match serde_json::from_value::<ThreadRequest>(payload) {
                    Ok(input) if local.is_some() && !input.thread_id.trim().is_empty() => {
                        streams.start_worktree_setup(id, store.path(), input.thread_id.trim());
                        continue;
                    }
                    Ok(input) if !input.thread_id.trim().is_empty() => Ok(chunk(id, Value::Null)),
                    _ => Ok(defect(id, "A thread ID is required.".into())),
                }
            }
            ClientMessage::Request { id, tag, payload }
                if matches!(tag.as_str(), "terminal.observe" | "terminal.attach") =>
            {
                let (scope, allowed) = if tag == "terminal.observe" {
                    ("terminal:read", permissions.terminal_read)
                } else {
                    ("terminal:operate", permissions.terminal_operate)
                };
                if let Some(local) = local.as_ref().filter(|_| allowed) {
                    let local_for_task = Arc::clone(local);
                    let store = store.clone();
                    tokio::task::spawn_blocking(move || {
                        terminal_rpc::subscribe(&local_for_task.terminals, &store, &tag, payload)
                            .expect("known terminal subscription")
                    })
                    .await
                    .map(|result| match result {
                        Ok(subscription) => streams.start(id, subscription, Arc::clone(local)),
                        Err(error) => failure(id, error),
                    })
                } else {
                    Ok(terminal_denied(id, scope))
                }
            }
            ClientMessage::Request { id, tag, payload } => {
                let store = store.clone();
                let local = local.clone();
                let thread_subscription = (tag == "orchestration.subscribeThread")
                    .then(|| {
                        payload
                            .get("threadId")?
                            .as_str()
                            .map(|thread| (id.clone(), thread.to_owned()))
                    })
                    .flatten();
                let response = tokio::task::spawn_blocking(move || {
                    response(&store, local.as_deref(), permissions, id, &tag, payload)
                })
                .await;
                if response
                    .as_ref()
                    .is_ok_and(|frame| frame["_tag"] == "Chunk")
                    && let Some((id, thread_id)) = thread_subscription
                {
                    thread_requests.insert(id, thread_id);
                }
                response
            }
            ClientMessage::Ping => Ok(json!({ "_tag": "Pong" })),
            ClientMessage::Interrupt { request_id } => {
                streams.interrupt(&request_id);
                if let Some(task) = operation_requests.remove(&request_id) {
                    task.abort();
                }
                thread_requests.remove(&request_id);
                shell_requests.remove(&request_id);
                shell_completions.remove(&request_id);
                if shell_requests.is_empty() {
                    identity_changes = None;
                }
                Ok(json!({
                    "_tag": "Exit",
                    "requestId": request_id,
                    "exit": { "_tag": "Failure", "cause": [{"_tag": "Interrupt"}] },
                }))
            }
            ClientMessage::Ack { request_id } => match request_id {
                Some(id) if shell_completions.remove(&id) => Ok(success(id, Value::Null)),
                _ => continue,
            },
            ClientMessage::Eof => break,
        };
        let Ok(response) = response else {
            break;
        };
        if !send(&mut socket, response).await {
            break;
        }
    }
}

fn vcs_status_unavailable(id: RequestId) -> Value {
    failure(
        id,
        RpcError::EnvironmentAuthorizationError {
            required_scope: "orchestration:read",
            message: "Git status is unavailable in the history server.",
        },
    )
}

async fn switch_git_ref(
    local: &LocalOperations,
    store: &Store,
    id: RequestId,
    payload: Value,
) -> Value {
    let input = match vcs_checkout::SwitchRefInput::parse(payload) {
        Ok(input) => input,
        Err(message) => return defect(id, message),
    };
    let cwd = input.cwd.clone();
    match vcs_checkout::switch_ref(store.path(), input).await {
        Ok(result) => {
            refresh_after_checkout(&local.vcs_status, cwd).await;
            success(id, result)
        }
        Err(error) => failure(id, error),
    }
}

async fn create_git_ref(
    local: &LocalOperations,
    store: &Store,
    id: RequestId,
    payload: Value,
) -> Value {
    let input = match vcs_checkout::CreateRefInput::parse(payload) {
        Ok(input) => input,
        Err(message) => return defect(id, message),
    };
    let cwd = input.cwd.clone();
    let switch = input.switch_ref;
    match vcs_checkout::create_ref(store.path(), input).await {
        Ok(result) => {
            if switch {
                refresh_after_checkout(&local.vcs_status, cwd).await;
            } else {
                let _ = local
                    .vcs_status
                    .refresh(vcs_status::RefreshInput {
                        cwd,
                        local_only: false,
                    })
                    .await;
            }
            success(id, result)
        }
        Err(error) => failure(id, error),
    }
}

async fn refresh_after_checkout(status: &vcs_status::Service, cwd: String) {
    let _ = status
        .refresh(vcs_status::RefreshInput {
            cwd: cwd.clone(),
            local_only: true,
        })
        .await;
    let status = status.clone();
    tokio::spawn(async move {
        let _ = status
            .refresh(vcs_status::RefreshInput {
                cwd,
                local_only: false,
            })
            .await;
    });
}

async fn cancel_worktree_setup(store: &Store, id: RequestId, payload: Value) -> Value {
    let request = match serde_json::from_value::<ThreadRequest>(payload) {
        Ok(request) => request,
        Err(error) => return defect(id, error.to_string()),
    };
    let cancelled =
        thread_launch::cancel_worktree_setup(store.path(), request.thread_id.trim()).await;
    success(id, json!({"cancelled":cancelled}))
}

async fn default_codex_model(local: &LocalOperations, id: RequestId, payload: Value) -> Value {
    let request = match serde_json::from_value::<ProjectRequest>(payload) {
        Ok(request) => request,
        Err(error) => return defect(id, error.to_string()),
    };
    match launch_model::discover(
        local.projects.clone(),
        local.settings.clone(),
        request.project_id,
    )
    .await
    {
        Ok(selection) => success(id, selection),
        Err(message) => failure(
            id,
            json!({"_tag":"CodexModelDiscoveryError", "message":message}),
        ),
    }
}

async fn delete_project(local: Arc<LocalOperations>, id: RequestId, payload: Value) -> Value {
    let fallback_id = id.clone();
    tokio::spawn(async move {
        let command_id = payload["commandId"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let project_id = payload["projectId"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let projects = local.projects.clone();
        match tokio::task::spawn_blocking(move || projects.dispatch(payload)).await {
            Ok(Ok(deleted)) => {
                if deleted.committed {
                    let _ = local.projection_changes.send(ProjectionChange::Project);
                }
                local.project_clones.discard(&project_id).await;
                let mut project = json!(deleted.project);
                project["repositoryIdentity"] = Value::Null;
                success(id, project)
            }
            Ok(Err(error)) => {
                let detail = error.to_string();
                let name = match &error {
                    ProjectCommandError::ProjectNotFound { .. } => "ProjectNotFoundError",
                    ProjectCommandError::ProjectNotEmpty { .. } => "ProjectNotEmptyError",
                    _ => "ProjectOperationError",
                };
                let message = if matches!(error, ProjectCommandError::ProjectNotEmpty { .. }) {
                    &detail
                } else {
                    "Failed to mutate project."
                };
                failure(
                    id,
                    json!({
                        "_tag":"ProjectMutationError", "commandId":command_id,
                        "message":message,
                        "cause":{"name":name,"message":detail},
                    }),
                )
            }
            Err(_) => defect(id, "Could not delete the project.".into()),
        }
    })
    .await
    .unwrap_or_else(|_| defect(fallback_id, "Could not delete the project.".into()))
}

async fn clone_project(local: &LocalOperations, id: RequestId, tag: &str, payload: Value) -> Value {
    if tag == "projectClone.start" {
        let input = match serde_json::from_value::<CloneStartInput>(payload) {
            Ok(input) => input,
            Err(error) => return defect(id, error.to_string()),
        };
        return match local.project_clones.start(input).await {
            Ok(result) => success(id, result),
            Err(error) => failure(id, error),
        };
    }
    let Some(project_id) = payload["projectId"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return defect(id, "A project ID is required.".into());
    };
    if tag == "projectClone.cancel" {
        return success(
            id,
            json!({"applied": local.project_clones.cancel(project_id.to_owned()).await}),
        );
    }
    match local.project_clones.retry(project_id.to_owned()).await {
        Ok(applied) => success(id, json!({"applied": applied})),
        Err(error) => failure(id, error),
    }
}

async fn launch_thread(
    local: &LocalOperations,
    store: &Store,
    id: RequestId,
    payload: Value,
) -> Value {
    let input = match serde_json::from_value::<LaunchInput>(payload) {
        Ok(input) => input,
        Err(error) => return defect(id, error.to_string()),
    };
    let (project_id, instance_id) = input.provider_project();
    let (project_id, instance_id) = (project_id.to_owned(), instance_id.to_owned());
    let settings = local.settings.clone();
    let provider_enabled = tokio::task::spawn_blocking(move || {
        settings.read().is_ok_and(|settings| {
            thread_launch::provider_enabled(&settings, &project_id, &instance_id)
        })
    })
    .await
    .unwrap_or(false);
    let changes = local.projection_changes.clone();
    let queue = local.turn_queue.clone();
    let context = LaunchContext {
        database: store.path(),
        scratch_root: &local.named_projects_root.with_file_name("scratch"),
        provider_enabled,
        settings_path: Some(local.settings.path()),
        terminals: Some(Arc::clone(&local.terminals)),
    };
    match thread_launch::launch(input, context, move |thread_id, start_turn| {
        let _ = changes.send(ProjectionChange::Thread(thread_id.to_owned()));
        if start_turn {
            queue.wake();
        }
    })
    .await
    {
        Ok(result) => success(id, result),
        Err(error) => failure(id, error),
    }
}

async fn retry_preparation(
    local: &LocalOperations,
    store: &Store,
    id: RequestId,
    payload: Value,
) -> Value {
    let input = match serde_json::from_value::<thread_launch::RetryInput>(payload) {
        Ok(input) => input,
        Err(error) => return defect(id, error.to_string()),
    };
    let context = LaunchContext {
        database: store.path(),
        scratch_root: &local.named_projects_root.with_file_name("scratch"),
        provider_enabled: true,
        settings_path: Some(local.settings.path()),
        terminals: Some(Arc::clone(&local.terminals)),
    };
    let changes = local.projection_changes.clone();
    let queue = local.turn_queue.clone();
    match thread_launch::retry_preparation(input, context, move |thread_id, start_turn| {
        let _ = changes.send(ProjectionChange::Thread(thread_id.to_owned()));
        if start_turn {
            queue.wake();
        }
    })
    .await
    {
        Ok(sequence) => success(id, json!({"sequence": sequence})),
        Err(error) => failure(id, error),
    }
}

async fn create_named_project(local: &LocalOperations, id: RequestId, payload: Value) -> Value {
    let Ok(input) = serde_json::from_value::<CreateNamedProject>(payload) else {
        return failure(
            id,
            RpcError::InvalidRequest {
                message: "A project name is required.",
            },
        );
    };
    match named_projects::create(
        local.projects.clone(),
        local.named_projects_root.clone(),
        input,
        local.projection_changes.clone(),
    )
    .await
    {
        Ok(project) => success(id, project),
        Err(error) => failure(
            id,
            json!({
                "_tag":"OrchestrationDispatchCommandError",
                "message":error.message(),
            }),
        ),
    }
}

async fn ensure_scratch_project(local: &LocalOperations, id: RequestId, payload: Value) -> Value {
    if !payload.is_object() && !payload.is_array() {
        return defect(id, "Expected object | array".into());
    }
    let base = local
        .named_projects_root
        .parent()
        .expect("managed projects root has a base directory");
    match super::scratch_project::ensure(
        local.projects.clone(),
        base,
        local.projection_changes.clone(),
    )
    .await
    {
        Ok(project) => success(id, json!({"projectId":project})),
        Err(error) => failure(id, error.payload()),
    }
}

async fn send(socket: &mut WebSocket, frame: Value) -> bool {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .is_ok()
}

async fn refresh_projections(
    socket: &mut WebSocket,
    store: &Store,
    identities: &Identities,
    shell_requests: &mut HashMap<RequestId, shell_stream::Subscription>,
    thread_requests: &mut HashMap<RequestId, String>,
    thread_ids: &[String],
) -> bool {
    let requests = thread_requests
        .iter()
        .filter(|(_, thread_id)| thread_ids.contains(thread_id))
        .map(|(request_id, thread_id)| (request_id.clone(), thread_id.clone()))
        .collect::<Vec<_>>();
    let store = store.clone();
    let identities = identities.clone();
    let shells = std::mem::take(shell_requests);
    let Ok((updates, frames)) = tokio::task::spawn_blocking(move || {
        let updates: Vec<_> = shells
            .into_values()
            .map(|shell| shell.refresh(&store, &identities))
            .collect();
        let mut frames = Vec::with_capacity(requests.len());
        for (id, thread_id) in requests {
            frames.push((id.clone(), thread_snapshot(&store, id, &thread_id, true)));
        }
        (updates, frames)
    })
    .await
    else {
        return false;
    };
    for shell in updates {
        if let Some(subscription) = shell.subscription {
            shell_requests.insert(subscription.id.clone(), subscription);
        }
        if let Some(frame) = shell.frame
            && !send(socket, frame).await
        {
            return false;
        }
    }
    for (id, frame) in frames {
        if frame["_tag"] == "Exit" {
            thread_requests.remove(&id);
        }
        if !send(socket, frame).await {
            return false;
        }
    }
    true
}

fn thread_snapshot(store: &Store, id: RequestId, thread_id: &str, live: bool) -> Value {
    let result = if live {
        store.live_thread_snapshot(thread_id)
    } else {
        store.thread(thread_id).map(|detail| {
            detail.map(|detail| {
                json!({
                        "kind": "snapshot",
                        "snapshotSequence": detail.snapshot_sequence,
                        "projection": detail
                })
            })
        })
    };
    match result {
        Ok(Some(snapshot)) => chunk(id, snapshot),
        missing => failure(
            id,
            RpcError::OrchestrationV2GetThreadProjectionError {
                thread_id,
                message: if missing.is_ok() {
                    "Thread not found."
                } else {
                    "Could not read this thread."
                },
            },
        ),
    }
}

fn response(
    store: &Store,
    local: Option<&LocalOperations>,
    permissions: Permissions,
    id: RequestId,
    tag: &str,
    payload: Value,
) -> Value {
    match tag {
        "server.probe" => {
            if payload.is_null() {
                defect(id, "Expected object | array".into())
            } else {
                success(id, json!({}))
            }
        }
        "orchestration.getTurnItem" => {
            let Some(input) = super::store::CommandDetail::parse(payload) else {
                return defect(id, "A thread and turn item ID are required.".into());
            };
            match store.command_detail(&input) {
                Ok(item) => success(id, json!({"item": item})),
                Err(_) => failure(
                    id,
                    RpcError::OrchestrationV2GetThreadProjectionError {
                        thread_id: &input.thread_id,
                        message: "Failed to load turn item",
                    },
                ),
            }
        }
        "orchestration.getThreadFamily" => {
            let subject = match shell_lookup_key(&payload, "threadId") {
                Ok(subject) => subject,
                Err(message) => return defect(id, message),
            };
            match store.thread_family(subject) {
                Ok(family) => success(id, family),
                Err(_) => failure(
                    id,
                    RpcError::OrchestrationV2GetShellSnapshotError {
                        message: "Failed to load the thread family",
                    },
                ),
            }
        }
        "orchestration.getWorktreeThreadIds" => {
            let path = match shell_lookup_key(&payload, "worktreePath") {
                Ok(path) => path,
                Err(message) => return defect(id, message),
            };
            match store.worktree_thread_ids(path) {
                Ok(threads) => success(id, json!({"threadIds": threads})),
                Err(_) => failure(
                    id,
                    RpcError::OrchestrationV2GetShellSnapshotError {
                        message: "Failed to load the worktree's threads",
                    },
                ),
            }
        }
        "orchestration.getArchivedShellSnapshot" if local.is_some() => {
            match store.live_archived_shell_snapshot() {
                Ok(snapshot) => success(id, snapshot),
                Err(_) => failure(
                    id,
                    RpcError::OrchestrationV2GetShellSnapshotError {
                        message: "Could not read archived threads.",
                    },
                ),
            }
        }
        "orchestration.getOlderSettledThreads" if local.is_some() => {
            let input = match super::store::OlderSettledRequest::decode(&payload) {
                Ok(input) => input,
                Err(message) => return defect(id, message),
            };
            match store.older_settled_threads(&input) {
                Ok(page) => success(id, page),
                Err(_) => failure(
                    id,
                    RpcError::OrchestrationV2GetShellSnapshotError {
                        message: "Failed to load older settled threads",
                    },
                ),
            }
        }
        "filesystem.browse" => browse(id, local.is_some() && permissions.filesystem_read, payload),
        "projects.mutate" => mutate_project(local, permissions.orchestration_operate, id, payload),
        "assets.createUrl" if local.is_some() => {
            let Some(input) = super::project_favicon::Input::decode(payload) else {
                return defect(id, "Invalid project favicon resource.".to_owned());
            };
            let local = local.expect("local operations are present");
            match local
                .project_favicons
                .issue(store.path(), &local.project_clones, input)
            {
                Ok(value) => success(id, value),
                Err(error) => failure(id, error),
            }
        }
        "server.updateSettings" => update_settings(id, store.path(), local, permissions, payload),
        "server.getSettings" if local.is_some() => {
            let settings = &local.expect("local operations are present").settings;
            match settings.read() {
                Ok(snapshot) => success(id, snapshot.for_client()),
                Err(error) => failure(
                    id,
                    json!({
                        "_tag": "ServerSettingsError", "operation":"read", "settingsPath": settings.path(),
                        "message": error.to_string(),
                    }),
                ),
            }
        }
        "terminal.open" | "terminal.write" | "terminal.resize" | "terminal.clear"
        | "terminal.close" => {
            let Some(local) = local.filter(|_| permissions.terminal_operate) else {
                return terminal_denied(id, "terminal:operate");
            };
            match terminal_rpc::call(&local.terminals, store, tag, payload)
                .expect("known terminal command")
            {
                Ok(value) => success(id, value),
                Err(error) => failure(id, error),
            }
        }
        "orchestration.subscribeThread" => {
            let Ok(thread) = serde_json::from_value::<ThreadRequest>(payload) else {
                return failure(
                    id,
                    RpcError::InvalidRequest {
                        message: "A thread ID is required.",
                    },
                );
            };
            thread_snapshot(store, id, &thread.thread_id, local.is_some())
        }
        "orchestration.dispatchCommand" => {
            dispatch_thread(local, store, permissions.orchestration_operate, id, payload)
        }
        "t3.canStopThread" if local.is_some() => {
            let thread_id = payload["threadId"].as_str().unwrap_or_default();
            let available = permissions.orchestration_operate
                && local
                    .is_some_and(|local| local.turns.can_stop_thread(thread_id).unwrap_or(false));
            success(id, json!({"available":available}))
        }
        "t3.threadActions" if local.is_some() => {
            let thread_id = payload["threadId"].as_str().unwrap_or_default();
            let permitted = local.filter(|_| permissions.orchestration_operate);
            let stop = permitted
                .is_some_and(|local| local.turns.can_stop_thread(thread_id).unwrap_or(false));
            let queue_worktree = permitted
                .is_some_and(|local| local.turns.can_queue_worktree(thread_id).unwrap_or(false));
            let codex_reasoning = permitted.is_some_and(|local| {
                local.settings.read().is_ok_and(|settings| {
                    local
                        .turns
                        .can_change_codex_reasoning(thread_id, &settings)
                        .unwrap_or(false)
                })
            });
            success(
                id,
                json!({"available":stop,"queueWorktree":queue_worktree,"codexReasoning":codex_reasoning}),
            )
        }
        _ => failure(
            id,
            RpcError::ReadOnlyEnvironment {
                message: if local.is_some() {
                    "This local environment does not support this operation."
                } else {
                    "This environment only supports browsing projects and thread history."
                },
            },
        ),
    }
}

fn browse(id: RequestId, allowed: bool, payload: Value) -> Value {
    if !allowed {
        return failure(
            id,
            RpcError::EnvironmentAuthorizationError {
                required_scope: "filesystem:read",
                message: "This session cannot browse filesystem folders.",
            },
        );
    }
    let input = match serde_json::from_value::<BrowseInput>(payload) {
        Ok(input) => input,
        Err(error) => return defect(id, error.to_string()),
    };
    match filesystem_browse::browse(input) {
        Ok(result) => match serde_json::to_value(result) {
            Ok(result) => success(id, result),
            Err(error) => defect(
                id,
                format!("Failed to encode response for RPC \"filesystem.browse\": {error}"),
            ),
        },
        Err(error) => failure(id, error),
    }
}

fn update_settings(
    id: RequestId,
    database: &std::path::Path,
    local: Option<&LocalOperations>,
    permissions: Permissions,
    payload: Value,
) -> Value {
    let patch = match super::settings::Patch::decode(&payload) {
        Ok(patch) => patch,
        Err(message) => return defect(id, message),
    };
    if let Some(required) = patch.required_permission(permissions.settings_write && local.is_some())
    {
        let scope = if required == "access:write" {
            required
        } else {
            "orchestration:operate"
        };
        return failure(
            id,
            json!({
                "_tag":"EnvironmentAuthorizationError",
                "requiredScope":scope,
                "requiredPermission":required,
                "message":format!("The authenticated token is missing required scope: {required}."),
            }),
        );
    }
    let settings = &local
        .expect("authorized settings write requires a local host")
        .settings;
    if let Some(unsupported) = patch.unsupported() {
        return failure(
            id,
            settings.error(
                "normalize",
                &format!("The setting '{unsupported}' cannot be updated by this host."),
            ),
        );
    }
    match settings.update(patch, database) {
        Ok(settings) => success(id, settings),
        Err(error) => failure(id, error),
    }
}

fn shell_lookup_key<'a>(payload: &'a Value, field: &str) -> Result<&'a str, String> {
    let fields = payload.as_object().ok_or("Expected object")?;
    let value = fields
        .get(field)
        .ok_or_else(|| format!("Missing key\n  at [\"{field}\"]"))?;
    let value = value
        .as_str()
        .ok_or_else(|| format!("Expected string\n  at [\"{field}\"]"))?;
    let value = super::thread_commands::trim_js(value);
    if value.is_empty() {
        return Err(format!("Expected a non-blank string\n  at [\"{field}\"]"));
    }
    Ok(value)
}

fn defect(id: RequestId, message: String) -> Value {
    json!({
        "_tag": "Exit",
        "requestId": id,
        "exit": {"_tag": "Failure", "cause": [{"_tag": "Die", "defect": message}]},
    })
}

fn mutate_project(
    local: Option<&LocalOperations>,
    can_manage: bool,
    id: RequestId,
    payload: Value,
) -> Value {
    let Some(local) = local.filter(|_| can_manage) else {
        return failure(
            id,
            RpcError::EnvironmentAuthorizationError {
                required_scope: "orchestration:operate",
                message: "This session cannot change projects.",
            },
        );
    };
    let command_id = payload["commandId"].as_str().unwrap_or_default().to_owned();
    if command_id.is_empty() {
        return failure(
            id,
            RpcError::InvalidRequest {
                message: "A command ID is required.",
            },
        );
    }
    let refresh_favicon =
        payload["type"] == "project.update" && payload.get("faviconPath").is_some();
    match local.projects.dispatch(payload) {
        Ok(result) => {
            if refresh_favicon {
                local
                    .project_favicons
                    .invalidate(result.project.workspace());
            }
            if result.committed {
                let _ = local.projection_changes.send(ProjectionChange::Project);
            }
            success(id, result.project)
        }
        Err(ProjectCommandError::InvalidProject(detail)) => failure(
            id,
            json!({
                "_tag": "ProjectMutationError",
                "commandId": command_id,
                "message": detail,
                "cause": { "name": "ProjectInvalidError", "message": detail },
            }),
        ),
        Err(error @ ProjectCommandError::WorkspaceDirectoryConflict { .. }) => {
            workspace_directory_conflict(id, &command_id, error)
        }
        Err(error) => failure(
            id,
            RpcError::ProjectMutationError {
                command_id: &command_id,
                message: &error.to_string(),
            },
        ),
    }
}

fn workspace_directory_conflict(
    id: RequestId,
    command_id: &str,
    error: ProjectCommandError,
) -> Value {
    let message = error.to_string();
    let ProjectCommandError::WorkspaceDirectoryConflict {
        project_id,
        path,
        ancestor,
    } = error
    else {
        unreachable!("workspace directory conflict")
    };
    let cause = if ancestor {
        json!({
            "name": "WorkspaceRootStatFailedError",
            "message": message,
            "cause": {
                "name": "PlatformError",
                "message": format!("BadResource: FileSystem.stat ({path})"),
                "cause": {
                    "name": "Error",
                    "message": format!("ENOTDIR: not a directory, stat '{path}'"),
                },
            },
        })
    } else {
        json!({ "name": "WorkspaceRootNotDirectoryError", "message": message })
    };
    failure(
        id,
        json!({
            "_tag":"ProjectMutationError", "commandId":command_id, "message":"Failed to mutate project.",
            "cause": {
                "name": "ProjectOperationError",
                "message": format!("Project operation 'normalize-workspace' failed for {project_id}."),
                "cause": cause,
            },
        }),
    )
}

fn dispatch_thread(
    local: Option<&LocalOperations>,
    store: &Store,
    can_manage: bool,
    id: RequestId,
    payload: Value,
) -> Value {
    let Some(local) = local.filter(|_| can_manage) else {
        return failure(
            id,
            RpcError::EnvironmentAuthorizationError {
                required_scope: "orchestration:operate",
                message: "This session cannot change threads.",
            },
        );
    };
    let Some(command_id) = payload["commandId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
    else {
        return failure(
            id,
            RpcError::InvalidRequest {
                message: "A command ID is required.",
            },
        );
    };
    let command_type = payload["type"].as_str().unwrap_or_default();
    if matches!(command_type, "queue.resume" | "queued-run.start") {
        let thread_id = payload["threadId"].as_str().unwrap_or_default();
        let selected = (command_type == "queued-run.start")
            .then(|| payload["runId"].as_str().unwrap_or_default());
        return resume_worktree_queue(local, id, thread_id, &command_id, selected);
    }
    if matches!(command_type, "queued-run.edit" | "queued-run.cancel") {
        return queued_run::dispatch(local, id, &payload, &command_id, command_type);
    }
    if command_type == super::turn_commands::model_selection::COMMAND {
        let thread_id = payload["threadId"].as_str().unwrap_or_default().to_owned();
        let result = local
            .settings
            .read()
            .map_err(|error| error.to_string())
            .and_then(|settings| {
                local
                    .turns
                    .set_codex_reasoning(payload, &settings)
                    .map_err(|error| error.to_string())
            });
        return match result {
            Ok((sequence, committed)) => {
                if committed {
                    let _ = local
                        .projection_changes
                        .send(ProjectionChange::Thread(thread_id));
                }
                success(id, json!({"sequence":sequence}))
            }
            Err(message) => failure(
                id,
                RpcError::OrchestrationV2DispatchCommandError {
                    command_id: &command_id,
                    command_type: super::turn_commands::model_selection::COMMAND,
                    message: &message,
                },
            ),
        };
    }
    if matches!(
        command_type,
        "message.dispatch" | "runtime-request.respond" | "thread.stop"
    ) {
        let thread_id = payload["threadId"].as_str().unwrap_or_default().to_owned();
        let command_type = command_type.to_owned();
        let result = if command_type == "thread.stop" {
            serde_json::from_value::<ThreadStop>(payload)
                .map_err(|error| error.to_string())
                .and_then(|input| {
                    local
                        .turns
                        .stop_thread(input)
                        .map(|receipt| (receipt.sequence, receipt.committed))
                        .map_err(|error| error.to_string())
                })
        } else if command_type == "message.dispatch" {
            serde_json::from_value::<MessageDispatch>(payload)
                .map_err(|error| error.to_string())
                .and_then(|input| {
                    let result = match local.settings.read() {
                        Ok(settings) => local.turns.dispatch_with_settings(input, Some(&settings)),
                        Err(_) => local.turns.dispatch(input),
                    };
                    result
                        .map(|receipt| (receipt.sequence, receipt.committed))
                        .map_err(|error| error.to_string())
                })
        } else {
            serde_json::from_value::<RuntimeResponse>(payload)
                .map_err(|error| error.to_string())
                .and_then(|input| {
                    local
                        .turns
                        .respond_runtime_request(input)
                        .map(|receipt| (receipt.sequence, receipt.committed))
                        .map_err(|error| error.to_string())
                })
        };
        return match result {
            Ok((sequence, committed)) => {
                if committed {
                    if command_type == "thread.stop" {
                        let _ =
                            thread_launch::request_worktree_cancellation(store.path(), &thread_id);
                    }
                    let _ = local
                        .projection_changes
                        .send(ProjectionChange::Thread(thread_id));
                }
                local.turn_queue.wake();
                success(id, json!({"sequence": sequence}))
            }
            Err(message) => failure(
                id,
                RpcError::OrchestrationV2DispatchCommandError {
                    command_id: &command_id,
                    command_type: &command_type,
                    message: &message,
                },
            ),
        };
    }
    if !matches!(
        command_type,
        "thread.create"
            | "thread.visit"
            | "thread.metadata.update"
            | "thread.archive"
            | "thread.unarchive"
            | "thread.delete"
    ) {
        return failure(
            id,
            RpcError::OrchestrationV2DispatchCommandError {
                command_id: &command_id,
                command_type,
                message: "This local host does not support that thread command.",
            },
        );
    }
    let command_type = command_type.to_owned();
    let thread_id = payload["threadId"].as_str().unwrap_or_default().to_owned();
    let result = if command_type == "thread.delete" {
        local
            .threads
            .delete_archived(payload)
            .map(|result| (result.receipt, result.thread_ids))
    } else if command_type == "thread.archive" {
        local.threads.archive(payload)
    } else if command_type == "thread.unarchive" {
        local.threads.unarchive(payload)
    } else {
        local
            .threads
            .dispatch(payload)
            .map(|receipt| (receipt, vec![thread_id]))
    };
    match result {
        Ok((result, thread_ids)) => {
            if result.committed {
                for thread_id in thread_ids {
                    let _ = local
                        .projection_changes
                        .send(ProjectionChange::Thread(thread_id));
                }
                if command_type == "thread.metadata.update" {
                    local.turn_queue.wake();
                }
            }
            success(id, json!({"sequence": result.sequence}))
        }
        Err(ThreadCommandError::InvalidRequest(message))
            if matches!(
                command_type.as_str(),
                "thread.metadata.update" | "thread.archive" | "thread.unarchive" | "thread.delete"
            ) =>
        {
            defect(id, message)
        }
        Err(error)
            if matches!(error, ThreadCommandError::NotArchived(_))
                || (command_type == "thread.archive"
                    && matches!(error, ThreadCommandError::Rejected(_))) =>
        {
            let message = error.to_string();
            failure(
                id,
                json!({
                    "_tag": "OrchestrationV2DispatchCommandError",
                    "commandId": command_id,
                    "commandType": command_type,
                    "message": message,
                    "detail": message,
                    "cause": {
                        "name": "OrchestratorDispatchError",
                        "message": format!("Failed to dispatch orchestration command {command_type} ({command_id})."),
                        "cause": message
                    }
                }),
            )
        }
        Err(ThreadCommandError::MissingProjection(thread_id)) => {
            let message = format!("No orchestration projection exists for thread {thread_id}.");
            failure(
                id,
                json!({
                    "_tag":"OrchestrationV2DispatchCommandError", "commandId":command_id, "commandType":command_type,
                    "message": message, "detail":message,
                    "cause":{"name":"OrchestratorProjectionError",
                             "message":format!("Failed to load orchestration projection for thread {thread_id}."),
                             "cause":{"name":"ProjectionStoreThreadNotFoundError","message":message}},
                }),
            )
        }
        Err(error) => failure(
            id,
            RpcError::OrchestrationV2DispatchCommandError {
                command_id: &command_id,
                command_type: &command_type,
                message: &error.to_string(),
            },
        ),
    }
}

fn resume_worktree_queue(
    local: &LocalOperations,
    id: RequestId,
    thread_id: &str,
    command_id: &str,
    selected: Option<&str>,
) -> Value {
    use super::turn_commands::queued_worktree::{
        INVALID_SELECTION, ResumeOutcome, UNRESOLVED_WORKTREE,
    };

    let (command_type, result) = match selected {
        Some(run_id) => (
            "queued-run.start",
            local
                .turns
                .start_queued_worktree_run(thread_id, command_id, run_id),
        ),
        None => (
            "queue.resume",
            local.turns.resume_worktree_queue(thread_id, command_id),
        ),
    };
    match result {
        Ok(ResumeOutcome::Accepted {
            sequence,
            committed,
        }) => {
            if committed {
                let _ = local
                    .projection_changes
                    .send(ProjectionChange::Thread(thread_id.to_owned()));
            }
            local.turn_queue.wake();
            success(id, json!({"sequence":sequence}))
        }
        Ok(refusal @ (ResumeOutcome::Unresolved | ResumeOutcome::InvalidSelection)) => {
            let detail = if matches!(refusal, ResumeOutcome::Unresolved) {
                UNRESOLVED_WORKTREE
            } else {
                INVALID_SELECTION
            };
            failure(
                id,
                json!({
                    "_tag":"OrchestrationV2DispatchCommandError", "commandId": command_id,
                    "commandType": command_type, "message": detail, "detail": detail,
                    "cause": {
                        "name": "OrchestratorDispatchError",
                        "message": format!("Failed to dispatch orchestration command {command_type} ({command_id})."),
                        "cause": detail,
                    },
                }),
            )
        }
        Ok(ResumeOutcome::PreviouslyRejected(detail)) => {
            let message = format!("Command {command_id} was previously rejected: {detail}");
            failure(
                id,
                json!({
                    "_tag":"OrchestrationV2DispatchCommandError", "commandId": command_id,
                    "commandType": command_type, "message": message, "detail": message,
                    "cause": {"name": "OrchestratorCommandPreviouslyRejectedError", "message": message},
                }),
            )
        }
        Err(error) => failure(
            id,
            RpcError::OrchestrationV2DispatchCommandError {
                command_id,
                command_type,
                message: &error.to_string(),
            },
        ),
    }
}

fn chunk(id: RequestId, value: Value) -> Value {
    json!({"_tag": "Chunk", "requestId": id, "values": [value]})
}

fn success(id: RequestId, value: impl Serialize) -> Value {
    json!({"_tag":"Exit", "requestId":id, "exit":{"_tag":"Success", "value":value}})
}

fn terminal_denied(id: RequestId, required_scope: &'static str) -> Value {
    failure(
        id,
        RpcError::EnvironmentAuthorizationError {
            required_scope,
            message: "This session cannot access that terminal operation.",
        },
    )
}

fn failure(id: RequestId, error: impl Serialize) -> Value {
    json!({
        "_tag": "Exit",
        "requestId": id,
        "exit": {
            "_tag": "Failure",
            "cause": [{ "_tag": "Fail", "error": error }]
        }
    })
}
