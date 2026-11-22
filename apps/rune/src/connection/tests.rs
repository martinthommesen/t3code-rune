use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

async fn read(socket: &mut WebSocket) -> Value {
    loop {
        let message = socket.recv().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        if frame["_tag"] == "Ping" {
            write(socket, json!({"_tag": "Pong"})).await;
        } else {
            return frame;
        }
    }
}

async fn write(socket: &mut WebSocket, frame: Value) {
    socket
        .send(WsMessage::Text(frame.to_string().into()))
        .await
        .unwrap();
}

async fn next_event(connection: &Connection, wake: &mut UnboundedReceiver<()>) -> Event {
    timeout(REQUEST_TIMEOUT, wake.recv())
        .await
        .unwrap()
        .unwrap();
    connection.try_recv().unwrap()
}

#[tokio::test]
async fn publishes_the_http_snapshot_before_replaying_the_socket() {
    let router = Router::new()
        .route(
            "/.well-known/t3/environment",
            get(|| async {
                Json(json!({
                    "environmentId": "snapshot-host",
                    "label": "Snapshot fixture",
                    "orchestrationProtocolVersion": 2,
                    "capabilities": {"t3ShellHttp": true}
                }))
            }),
        )
        .route(
            "/oauth/token",
            post(|| async { Json(json!({"access_token": "snapshot-token"})) }),
        )
        .route(
            "/api/auth/websocket-ticket/rpc",
            post(|| async { Json(json!({"ticket": "snapshot-ticket"})) }),
        )
        .route(
            "/api/orchestration/shell",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer snapshot-token");
                assert_eq!(headers["x-t3-orchestration-protocol"], "2");
                let mut frame = shell_snapshot(&Value::Null);
                let mut snapshot = frame["values"][0]["snapshot"].take();
                snapshot["projectIdentities"] = json!("snapshot-digest");
                Json(snapshot)
            }),
        )
        .route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    let request = read(&mut socket).await;
                    assert_eq!(request["tag"], "orchestration.subscribeShell");
                    assert_eq!(
                        request["payload"],
                        json!({
                            "afterSequence": 1,
                            "requestCompletionMarker": true,
                            "knownProjectIdentities": "snapshot-digest",
                        })
                    );
                    write(&mut socket, json!({
                    "_tag": "Chunk",
                    "requestId": request["id"],
                    "values": [
                        {"kind": "project.updated", "sequence": 2, "project": {
                            "id": "project", "title": "Updated project", "workspaceRoot": "/project"
                        }},
                        {"kind": "synchronized"}
                    ]
                })).await;
                    expect(&mut socket, "Ack", &request["id"]).await;
                    while socket.recv().await.is_some() {}
                })
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (wake, mut wake_rx) = unbounded_channel();
    let connection = Connection::start(
        format!("http://{address}"),
        "fixture pairing code".into(),
        move || {
            let _ = wake.send(());
        },
    );
    assert!(matches!(
        next_event(&connection, &mut wake_rx).await,
        Event::Connected { .. }
    ));
    for name in ["Project", "Updated project"] {
        assert!(matches!(
            next_event(&connection, &mut wake_rx).await,
            Event::Shell { projects, threads }
                if projects[0].name == name && threads[0].title == "Thread"
        ));
    }
    server.abort();
}

#[tokio::test]
async fn pairs_reads_sends_launches_and_recovers_without_replaying_mutations() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let tickets = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/.well-known/t3/environment", get(|| async {
            Json(json!({"environmentId": "test-host", "label": "Fixture", "orchestrationProtocolVersion": 2}))
        }))
        .route("/oauth/token", post(move |Form(form): Form<HashMap<String, String>>| async move {
            assert_eq!(exchanges.fetch_add(1, Ordering::Relaxed), 0);
            assert_eq!(form["subject_token"], "test pairing code");
            assert_eq!(form["grant_type"], "urn:ietf:params:oauth:grant-type:token-exchange");
            assert_eq!(form["requested_token_type"], ACCESS_TOKEN_TYPE);
            Json(json!({"access_token": "test-bearer"}))
        }))
        .route("/api/auth/websocket-ticket/rpc", post(move |headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer test-bearer");
            let ticket = tickets.fetch_add(1, Ordering::Relaxed);
            Json(json!({"ticket": ticket.to_string()}))
        }))
        .route("/ws", get(|axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>, ws: WebSocketUpgrade| async move {
            if query["orchestrationProtocol"] != "2" {
                return (StatusCode::UNAUTHORIZED, "invalid ticket or protocol").into_response();
            }
            match query["wsTicket"].as_str() {
                "0" => ws.on_upgrade(serve_thread),
                "1" => ws.on_upgrade(serve_restored_thread),
                _ => (StatusCode::UNAUTHORIZED, "invalid ticket").into_response(),
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (wake, mut wake_rx) = unbounded_channel();
    let connection = Connection::start(
        format!("http://{address}"),
        "test pairing code".into(),
        move || {
            let _ = wake.send(());
        },
    );

    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::Connected { environment_id, read_only: false, .. } if environment_id == "test-host")
    );
    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::Shell { projects, threads } if projects[0].name == "Project" && threads[0].title == "Thread")
    );
    connection.open_thread("thread".into());
    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::Thread { thread_id, entries } if thread_id == "thread" && entries[0].message().is_some_and(|message| message.text == "Existing response"))
    );
    assert!(matches!(next_event(&connection, &mut wake_rx).await,
        Event::ThreadQuestions { thread_id, questions, can_respond: false }
        if thread_id == "thread" && questions.is_empty()));
    connection.send_prompt("thread".into(), "  Preserve whitespace ".into());
    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::PromptAccepted { thread_id } if thread_id == "thread")
    );
    connection.launch_thread(
        "project".into(),
        "  Launch test\nThis is a new thread.\n".into(),
        LaunchWorkspace::Root,
    );
    assert!(matches!(
        next_event(&connection, &mut wake_rx).await,
        Event::ThreadLaunched { project_id, thread_id }
            if project_id == "project" && Uuid::parse_str(&thread_id).is_ok()
    ));
    connection.send_prompt("thread".into(), "Wait for confirmation".into());
    connection.launch_thread(
        "project".into(),
        "Unconfirmed thread launch".into(),
        LaunchWorkspace::Root,
    );
    assert!(matches!(
        next_event(&connection, &mut wake_rx).await,
        Event::Reconnecting
    ));
    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::RequestError { thread_id, action: ThreadAction::Send, .. } if thread_id == "thread")
    );
    assert!(matches!(
        next_event(&connection, &mut wake_rx).await,
        Event::LaunchError { project_id, message }
            if project_id == "project" && message.contains("could not be confirmed")
    ));
    assert!(matches!(
        next_event(&connection, &mut wake_rx).await,
        Event::Reconnected
    ));
    assert!(matches!(
        next_event(&connection, &mut wake_rx).await,
        Event::Shell { .. }
    ));
    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::Thread { thread_id, entries } if thread_id == "thread" && entries[0].message().is_some_and(|message| message.text == "Restored response"))
    );
    assert!(matches!(next_event(&connection, &mut wake_rx).await,
        Event::ThreadQuestions { thread_id, questions, can_respond: false }
        if thread_id == "thread" && questions.is_empty()));
    connection.close_thread();
    assert!(
        matches!(next_event(&connection, &mut wake_rx).await, Event::Shell { projects, .. } if projects[0].name == "Renamed")
    );
    server.abort();
}

async fn serve_thread(mut socket: WebSocket) {
    let shell = read(&mut socket).await;
    assert_eq!(shell["tag"], "orchestration.subscribeShell");
    write(&mut socket, shell_snapshot(&shell["id"])).await;
    expect(&mut socket, "Ack", &shell["id"]).await;

    let thread = read(&mut socket).await;
    assert_eq!(thread["tag"], "orchestration.subscribeThread");
    assert_eq!(thread["payload"]["threadId"], "thread");
    write(
        &mut socket,
        thread_snapshot(&thread["id"], "Existing response"),
    )
    .await;
    expect(&mut socket, "Ack", &thread["id"]).await;

    let prompt = read(&mut socket).await;
    assert_eq!(prompt["tag"], "orchestration.dispatchCommand");
    let payload = &prompt["payload"];
    assert_eq!(payload["type"], "message.dispatch");
    assert_eq!(payload["threadId"], "thread");
    assert_eq!(payload["text"], "  Preserve whitespace ");
    write(
        &mut socket,
        json!({"_tag": "Exit", "requestId": prompt["id"], "exit": {
            "_tag": "Success", "value": {"sequence": 3}
        }}),
    )
    .await;
    let launch = read_launch(&mut socket, "  Launch test\nThis is a new thread.\n").await;
    assert_eq!(
        launch["payload"]["title"],
        "Launch test This is a new thread."
    );
    assert_eq!(launch["payload"]["generateTitle"], true);
    write(
        &mut socket,
        json!({"_tag": "Exit", "requestId": launch["id"], "exit": {
            "_tag": "Success", "value": {"threadId": launch["payload"]["threadId"]}
        }}),
    )
    .await;
    let unconfirmed = read(&mut socket).await;
    assert_eq!(unconfirmed["tag"], "orchestration.dispatchCommand");
    assert_eq!(unconfirmed["payload"]["text"], "Wait for confirmation");
    read_launch(&mut socket, "Unconfirmed thread launch").await;
}

async fn read_launch(socket: &mut WebSocket, prompt: &str) -> Value {
    let config = read(socket).await;
    assert_eq!(config["tag"], "server.getSettings");
    write(
        socket,
        json!({"_tag": "Exit", "requestId": config["id"], "exit": {
            "_tag": "Success", "value": {
                "defaultModelSelection": {"instanceId": "global", "model": "other"},
                "defaultRuntimeMode": "full-access",
                "providerInstances": {"project-provider": {"driver": "codex"}},
                "projectSettingsOverrides": {"project": {
                    "defaultModelSelection": {"instanceId": "project-provider", "model": "model", "options": {"effort": "medium"}},
                    "defaultRuntimeMode": "approval-required"
                }}
            }
        }}),
    )
    .await;
    let launch = read(socket).await;
    assert_eq!(launch["tag"], "orchestration.launchThread");
    assert_eq!(launch["payload"]["projectId"], "project");
    assert_eq!(launch["payload"]["runtimeMode"], "approval-required");
    assert_eq!(
        launch["payload"]["workspaceStrategy"],
        json!({"type": "root"})
    );
    assert_eq!(launch["payload"]["initialMessage"]["text"], prompt);
    assert_eq!(
        launch["payload"]["modelSelection"],
        json!({"instanceId": "project-provider", "model": "model", "options": {"effort": "medium"}})
    );
    launch
}

#[test]
fn launch_uses_legacy_project_model_only_until_settings_are_folded() {
    let project = serde_json::from_value(json!({
        "id": "project",
        "title": "Project",
        "workspaceRoot": "/project",
        "defaultModelSelection": {"provider": "codex", "model": "project-model"}
    }))
    .unwrap();
    let draft = launch::Draft {
        project_id: "project".into(),
        thread_id: "thread".into(),
        prompt: "Start a thread".into(),
        workspace: LaunchWorkspace::Root,
    };
    let mut settings = json!({
        "defaultModelSelection": {"instanceId": "environment", "model": "environment-model"},
        "defaultRuntimeMode": "auto"
    });
    let payload = launch::payload(settings.clone(), &project, &draft).unwrap();
    assert_eq!(payload["modelSelection"]["instanceId"], "codex");

    settings["projectSettingsFolded"] = json!(true);
    let payload = launch::payload(settings.clone(), &project, &draft).unwrap();
    assert_eq!(payload["modelSelection"]["instanceId"], "environment");
    assert_eq!(payload["runtimeMode"], "auto");

    settings["projectSettingsOverrides"] = json!({"project": {"defaultModelSelection": null}});
    let payload = launch::payload(settings, &project, &draft).unwrap();
    assert!(payload["modelSelection"].is_null());
}

#[test]
fn disabled_and_hidden_project_models_inherit_the_environment_default() {
    let project = serde_json::from_value(json!({
        "id": "project", "title": "Project", "workspaceRoot": "/project"
    }))
    .unwrap();
    let draft = launch::Draft {
        project_id: "project".into(),
        thread_id: "thread".into(),
        prompt: "Start a thread".into(),
        workspace: LaunchWorkspace::Root,
    };
    let mut settings = json!({
        "defaultModelSelection": {"instanceId": "codex", "model": "fallback"},
        "defaultRuntimeMode": "approval-required",
        "projectSettingsOverrides": {"project": {
            "defaultModelSelection": {"instanceId": "other", "model": "custom"}
        }},
        "providerInstances": {"other": {"driver": "cursor", "config": {"enabled": false}}},
        "providerModelPreferences": {"other": {"hiddenModels": ["custom"]}}
    });
    let model = |settings: &Value| {
        launch::payload(settings.clone(), &project, &draft).unwrap()["modelSelection"]["model"]
            .clone()
    };
    assert_eq!(model(&settings), "fallback");
    settings["projectSettingsOverrides"]["project"]["providerInstances"] =
        json!({"other": {"enabled": true}});
    assert_eq!(model(&settings), "fallback");
    settings["providerInstances"]["other"]["config"]["customModels"] =
        json!([{"slug": " custom "}]);
    assert_eq!(model(&settings), "custom");
}

async fn serve_restored_thread(mut socket: WebSocket) {
    let shell = read(&mut socket).await;
    assert_eq!(shell["tag"], "orchestration.subscribeShell");
    let thread = read(&mut socket).await;
    assert_eq!(thread["tag"], "orchestration.subscribeThread");
    assert_eq!(thread["payload"]["threadId"], "thread");
    write(&mut socket, shell_snapshot(&shell["id"])).await;
    write(
        &mut socket,
        thread_snapshot(&thread["id"], "Restored response"),
    )
    .await;
    expect(&mut socket, "Ack", &shell["id"]).await;
    expect(&mut socket, "Ack", &thread["id"]).await;
    expect(&mut socket, "Interrupt", &thread["id"]).await;
    write(
        &mut socket,
        json!({"_tag": "Chunk", "requestId": shell["id"], "values": [{
            "kind": "project.updated", "sequence": 4,
            "project": {"id": "project", "title": "Renamed", "workspaceRoot": "/project"}
        }]}),
    )
    .await;
}

async fn expect(socket: &mut WebSocket, tag: &str, request_id: &Value) {
    assert_eq!(
        read(socket).await,
        json!({"_tag": tag, "requestId": request_id})
    );
}

fn shell_snapshot(request_id: &Value) -> Value {
    json!({"_tag": "Chunk", "requestId": request_id, "values": [{
        "kind": "snapshot", "snapshot": {
            "snapshotSequence": 1,
            "projects": [{"id": "project", "title": "Project", "workspaceRoot": "/project"}],
            "threads": [{"id": "thread", "projectId": "project", "title": "Thread", "status": "idle"}]
        }
    }]})
}

fn thread_snapshot(request_id: &Value, response: &str) -> Value {
    json!({"_tag": "Chunk", "requestId": request_id, "values": [{
        "kind": "snapshot", "snapshotSequence": 2, "projection": {
            "messages": [{"id": "message", "role": "assistant", "text": response}]
        }
    }]})
}
