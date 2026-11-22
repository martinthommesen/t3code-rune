use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::timeout;

use super::super::{Connection, Event, REQUEST_TIMEOUT};
use super::{Intent, Selection, TerminalAttach, TerminalStatus, TerminalTarget, TerminalUpdate};

fn target(thread: &str) -> TerminalAttach {
    TerminalAttach {
        target: TerminalTarget {
            thread_id: thread.to_owned(),
            terminal_id: "term-1".to_owned(),
        },
        cwd: Some("/fixture".to_owned()),
        worktree_path: None,
        cols: 80,
        rows: 24,
        resume: false,
    }
}

#[test]
fn detached_and_replaced_terminals_are_not_restored_on_reconnect() {
    let selection = Selection::default();
    let left = target("same-thread");
    let mut right = left.clone();
    right.target.terminal_id = "term-2".to_owned();
    selection.attach(&Intent::new(1, left));
    selection.attach(&Intent::new(2, right.clone()));
    selection.keep_thread(Some("same-thread"));
    assert!(selection.includes(1));
    assert!(selection.includes(2));
    selection.detach(1);
    assert!(!selection.includes(1));
    assert!(selection.includes(2));
    selection.attach(&Intent::new(3, right));
    assert!(!selection.includes(2));
    assert!(selection.includes(3));
    selection.keep_thread(Some("another-thread"));
    assert!(!selection.includes(3));
}

async fn next(connection: &Connection, wake: &mut UnboundedReceiver<()>) -> Event {
    timeout(REQUEST_TIMEOUT, wake.recv())
        .await
        .unwrap()
        .unwrap();
    connection.try_recv().unwrap()
}

async fn expect_archive_disabled(connection: &Connection, wake: &mut UnboundedReceiver<()>) {
    assert!(matches!(
        next(connection, wake).await,
        Event::ArchiveAvailable {
            readable: false,
            archivable: false,
            writable: false,
            deletable: false,
        }
    ));
}

async fn read(socket: &mut WebSocket) -> Value {
    loop {
        let message = socket.recv().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        match frame["_tag"].as_str() {
            Some("Ping") => send(socket, json!({"_tag": "Pong"})).await,
            Some("Ack") => {}
            _ => return frame,
        }
    }
}

async fn send(socket: &mut WebSocket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

async fn request(socket: &mut WebSocket, tag: &str) -> Value {
    let frame = read(socket).await;
    assert_eq!(frame["_tag"], "Request");
    assert_eq!(frame["tag"], tag);
    frame
}

fn output(thread: &str, sequence: u64, data: &str) -> Value {
    json!({"type":"output", "threadId": thread, "terminalId":"term-1", "sequence": sequence, "data": data})
}

async fn chunk(socket: &mut WebSocket, id: &Value, values: Vec<Value>) {
    send(
        socket,
        json!({"_tag":"Chunk", "requestId":id, "values": values}),
    )
    .await;
}

async fn snapshot(socket: &mut WebSocket, id: &Value, thread: &str, history: &str) {
    chunk(socket, id, vec![json!({
        "type":"snapshot", "snapshot": {
            "threadId":thread, "terminalId":"term-1", "history":history, "status":"running", "sequence":10, "label":"Terminal 1",
        }
    })]).await;
}

async fn first_socket(mut socket: WebSocket) {
    request(&mut socket, "orchestration.subscribeShell").await;
    request(&mut socket, "orchestration.subscribeThread").await;
    let attach = request(&mut socket, "terminal.attach").await;
    assert!(attach["payload"].get("resume").is_none());
    assert_eq!(attach["payload"]["restartIfNotRunning"], true);
    snapshot(&mut socket, &attach["id"], "thread", "original").await;
    chunk(
        &mut socket,
        &attach["id"],
        vec![
            output("thread", 11, "first"),
            output("thread", 11, "duplicate"),
            output("thread", 12, "second"),
        ],
    )
    .await;

    let write = request(&mut socket, "terminal.write").await;
    assert_eq!(write["payload"]["data"], "printf 'hello'\r");
    let first_paste_chunk = request(&mut socket, "terminal.write").await;
    assert_eq!(first_paste_chunk["payload"]["data"], "x".repeat(65_535));
    let last_paste_chunk = request(&mut socket, "terminal.write").await;
    assert_eq!(last_paste_chunk["payload"]["data"], "🙂\r");
    let resize = request(&mut socket, "terminal.resize").await;
    assert_eq!(resize["payload"]["cols"], 99);
    assert_eq!(resize["payload"]["rows"], 36);
}

async fn second_socket(mut socket: WebSocket) {
    request(&mut socket, "orchestration.subscribeShell").await;
    request(&mut socket, "orchestration.subscribeThread").await;
    let attach = request(&mut socket, "terminal.attach").await;
    assert_eq!(attach["payload"]["resume"], true);
    assert_eq!(attach["payload"]["restartIfNotRunning"], false);
    assert_eq!(attach["payload"]["cols"], 99);
    assert_eq!(attach["payload"]["rows"], 36);
    snapshot(&mut socket, &attach["id"], "thread", "restored").await;

    let detached = read(&mut socket).await;
    assert_eq!(detached["_tag"], "Interrupt");
    assert_eq!(detached["requestId"], attach["id"]);
    assert_eq!(read(&mut socket).await["_tag"], "Interrupt");
    request(&mut socket, "orchestration.subscribeThread").await;
    let new_attach = request(&mut socket, "terminal.attach").await;
    assert_eq!(new_attach["payload"]["threadId"], "new-thread");
    chunk(
        &mut socket,
        &attach["id"],
        vec![output("thread", 20, "stale")],
    )
    .await;
    snapshot(&mut socket, &new_attach["id"], "new-thread", "current").await;

    let inactive_close = request(&mut socket, "terminal.close").await;
    assert_eq!(inactive_close["payload"]["threadId"], "new-thread");
    assert_eq!(inactive_close["payload"]["terminalId"], "term-2-test");
    assert_eq!(inactive_close["payload"]["deleteHistory"], true);
    send(
        &mut socket,
        json!({"_tag":"Exit", "requestId":inactive_close["id"], "exit":{"_tag":"Success", "value":null}}),
    )
    .await;
    chunk(
        &mut socket,
        &new_attach["id"],
        vec![output("new-thread", 11, "still attached")],
    )
    .await;

    let close = request(&mut socket, "terminal.close").await;
    assert_eq!(close["payload"]["threadId"], "new-thread");
    assert_eq!(close["payload"]["deleteHistory"], true);
    send(
        &mut socket,
        json!({"_tag":"Exit", "requestId":close["id"], "exit":{"_tag":"Success", "value":null}}),
    )
    .await;
    assert_eq!(read(&mut socket).await["requestId"], new_attach["id"]);

    let resumed_later = request(&mut socket, "terminal.attach").await;
    snapshot(
        &mut socket,
        &resumed_later["id"],
        "new-thread",
        "before network loss",
    )
    .await;
}

async fn third_socket(mut socket: WebSocket) {
    request(&mut socket, "orchestration.subscribeShell").await;
    request(&mut socket, "orchestration.subscribeThread").await;
    let attach = request(&mut socket, "terminal.attach").await;
    assert!(attach["payload"].get("resume").is_none());
    snapshot(&mut socket, &attach["id"], "new-thread", "fresh attachment").await;
}

#[tokio::test]
async fn terminal_stream_resumes_the_latest_grid_without_replaying_input_or_stale_output() {
    let connections = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/.well-known/t3/environment", get(|| async {
            Json(json!({"environmentId":"fixture", "label":"Fixture", "orchestrationProtocolVersion":2, "capabilities":{"t3ReadOnly":true,"t3Terminal":true}}))
        }))
        .route("/oauth/token", post(|| async { Json(json!({"access_token":"fixture-access", "scope":"orchestration:read terminal:operate"})) }))
        .route("/api/auth/websocket-ticket/rpc", post(|| async { Json(json!({"ticket":"fixture-ticket"})) }))
        .route("/ws", get(move |ws: WebSocketUpgrade| {
            let socket_number = connections.fetch_add(1, Ordering::Relaxed);
            async move {
                match socket_number {
                    0 => ws.on_upgrade(first_socket),
                    1 => ws.on_upgrade(second_socket),
                    _ => ws.on_upgrade(third_socket),
                }
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (send_wake, mut wake) = unbounded_channel();
    let connection = Connection::start(
        format!("http://{address}"),
        "fixture".to_owned(),
        move || {
            let _ = send_wake.send(());
        },
    );
    assert!(matches!(
        next(&connection, &mut wake).await,
        Event::Connected {
            terminal: true,
            read_only: true,
            ..
        }
    ));
    expect_archive_disabled(&connection, &mut wake).await;
    connection.open_thread("thread".to_owned());
    let old = connection.attach_terminal(target("thread"));
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Snapshot { snapshot } } if attachment == old && snapshot.status == TerminalStatus::Running && snapshot.history == "original")
    );
    for text in ["first", "second"] {
        assert!(
            matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Output { data } } if attachment == old && data == text)
        );
    }

    connection.write_terminal(old, "printf 'hello'\r".to_owned(), false, false);
    connection.write_terminal(old, format!("{}🙂\r", "x".repeat(65_535)), false, false);
    connection.resize_terminal(old, 99, 36);
    assert!(matches!(
        next(&connection, &mut wake).await,
        Event::Reconnecting
    ));
    assert!(
        matches!(next(&connection, &mut wake).await, Event::TerminalFailed { attachment, message } if attachment == old && message.contains("delivery"))
    );
    assert!(matches!(
        next(&connection, &mut wake).await,
        Event::Reconnected
    ));
    expect_archive_disabled(&connection, &mut wake).await;
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Snapshot { snapshot } } if attachment == old && snapshot.history == "restored")
    );

    connection.detach_terminal(old);
    connection.open_thread("new-thread".to_owned());
    let current = connection.attach_terminal(target("new-thread"));
    connection.write_terminal(old, "old input".to_owned(), false, false);
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Snapshot { snapshot } } if attachment == current && snapshot.history == "current")
    );
    let wrong_thread = target("thread").target;
    connection.close_terminal(wrong_thread.clone(), true);
    assert!(
        matches!(next(&connection, &mut wake).await, Event::TerminalCloseSettled { target, result: Err(_) } if target == wrong_thread)
    );

    let mut inactive = target("new-thread").target;
    inactive.terminal_id = "term-2-test".to_owned();
    connection.close_terminal(inactive.clone(), true);
    assert!(
        matches!(next(&connection, &mut wake).await, Event::TerminalCloseSettled { target, result: Ok(()) } if target == inactive)
    );
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Output { data } } if attachment == current && data == "still attached")
    );

    let selected = target("new-thread").target;
    connection.close_terminal(selected.clone(), true);
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Closed } if attachment == current)
    );
    assert!(
        matches!(next(&connection, &mut wake).await, Event::TerminalCloseSettled { target, result: Ok(()) } if target == selected)
    );

    let detached_while_offline = connection.attach_terminal(target("new-thread"));
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Snapshot { .. } } if attachment == detached_while_offline)
    );
    assert!(matches!(
        next(&connection, &mut wake).await,
        Event::Reconnecting
    ));
    connection.detach_terminal(detached_while_offline);
    assert!(matches!(
        next(&connection, &mut wake).await,
        Event::Reconnected
    ));
    expect_archive_disabled(&connection, &mut wake).await;
    let fresh = connection.attach_terminal(target("new-thread"));
    assert!(
        matches!(next(&connection, &mut wake).await, Event::Terminal { attachment, update: TerminalUpdate::Snapshot { snapshot } } if attachment == fresh && snapshot.history == "fresh attachment")
    );
    server.abort();
}
