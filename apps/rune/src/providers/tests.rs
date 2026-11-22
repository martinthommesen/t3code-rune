use super::protocol::{Frame, read_frame};
use super::*;
use tokio::io::BufReader;

#[tokio::test]
async fn reads_split_utf8_crlf_and_unterminated_frames() {
    let wire = "\r\n{\"method\":\"delta\",\"params\":\"é\"}\r\n{\"id\":1,\"result\":null}";
    let mut input = BufReader::with_capacity(1, wire.as_bytes());
    let mut buffer = Vec::new();
    assert!(matches!(
        read_frame(&mut input, &mut buffer, 128).await.unwrap(),
        Some(Frame::Notification { method, params: Some(params) }) if method == "delta" && params == "é"
    ));
    assert!(matches!(
        read_frame(&mut input, &mut buffer, 128).await.unwrap(),
        Some(Frame::Response {
            result: Ok(Value::Null),
            ..
        })
    ));
    assert!(
        read_frame(&mut input, &mut buffer, 128)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn rejects_oversized_input_before_finishing_a_line() {
    let mut input = BufReader::with_capacity(2, b"12345".as_slice());
    let mut buffer = Vec::new();
    assert!(matches!(
        read_frame(&mut input, &mut buffer, 4).await,
        Err(CodexError::FrameTooLarge)
    ));
}

#[cfg(unix)]
mod subprocess {
    use std::fs;

    use super::*;

    const DEADLINE: Duration = Duration::from_secs(5);
    const RECORDING: &str = include_str!(
        "../../../server/src/orchestration-v2/testkit/fixtures/tool_call_read_only_on_request/codex_transcript.ndjson"
    );

    struct Peer {
        directory: PathBuf,
    }

    impl Peer {
        fn new(body: &str) -> Self {
            Self::with_startup_output("", body)
        }

        fn with_startup_output(startup: &str, body: &str) -> Self {
            let directory = std::env::temp_dir().join(format!("t3-codex-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            let script = directory.join("app-server");
            let initialize = record("emit_inbound", "initialize");
            let preamble = format!(
                r#"#!/bin/sh
set -eu
test "$0" = app-server
test "$1" = -c
test "$2" = 'thread_unload_delay_secs=0'
test "$3" = --profile
test "$4" = 'profile with spaces'
test "$CODEX_INTERNAL_ORIGINATOR_OVERRIDE" = 'T3 Code'
test "$CODEX_TEST_MARKER" = 'fixture-only'
IFS= read -r line
case "$line" in *'"id":1'*'"method":"initialize"'*) ;; *) exit 20 ;; esac
case "$line" in *'"experimentalApi":true'*'"optOutNotificationMethods":["turn/diff/updated"]'*) ;; *) exit 21 ;; esac
case "$line" in *'"name":"T3 Code"'*'"version":"test"'*) ;; *) exit 22 ;; esac
case "$line" in *'"io.modelcontextprotocol/ui"'*'"text/html;profile=mcp-app"'*) ;; *) exit 26 ;; esac
{startup}
{}
IFS= read -r line
test "$line" = '{{"method":"initialized"}}'
"#,
                emit(&initialize)
            );
            fs::write(script, format!("{preamble}{body}\n")).unwrap();
            Self { directory }
        }

        async fn launch(&self) -> CodexAppServer {
            timeout(
                DEADLINE,
                CodexAppServer::launch(CodexLaunch {
                    executable: PathBuf::from("/bin/sh"),
                    working_directory: self.directory.clone(),
                    arguments: vec!["--profile".into(), "profile with spaces".into()],
                    environment: HashMap::from([(
                        "CODEX_TEST_MARKER".into(),
                        "fixture-only".into(),
                    )]),
                    client_version: "test".into(),
                }),
            )
            .await
            .unwrap()
            .unwrap()
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn record(kind: &str, label: &str) -> Value {
        RECORDING
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|row| row["type"] == kind && row["label"] == label)
            .expect("recorded frame")["frame"]
            .clone()
    }

    fn emit(frame: &Value) -> String {
        format!(
            "printf '%s\\n' '{}'",
            frame.to_string().replace('\'', "'\"'\"'")
        )
    }

    async fn event(server: &mut CodexAppServer) -> CodexEvent {
        timeout(DEADLINE, server.next_event())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn relays_recorded_turn_and_preserves_approval_response_identity() {
        let approval = record("emit_inbound", "item/commandExecution/requestApproval");
        let mut response = record("emit_inbound", "turn/start");
        response["id"] = json!(2);
        let resolved = record("emit_inbound", "serverRequest/resolved");
        let done = record("emit_inbound", "turn/completed");
        let answer = record("expect_outbound", "item/commandExecution/requestApproval");
        let init = record("emit_inbound", "initialize");
        let peer = Peer::new(&format!(
            r#"
IFS= read -r start
case "$start" in *'"id":2'*'"method":"turn/start"'*) ;; *) exit 23 ;; esac
{}
IFS= read -r approval
test "$approval" = '{}'
{}
{}
{}
while IFS= read -r line; do :; done
"#,
            emit(&approval),
            answer,
            emit(&resolved),
            emit(&response),
            emit(&done)
        ));
        let mut server = peer.launch().await;
        assert_eq!(
            server.initialization.user_agent,
            init["result"]["userAgent"].as_str().unwrap()
        );
        let client = server.client();
        let request = record("expect_outbound", "turn/start");
        let start = client.request("turn/start", Some(request["params"].clone()), DEADLINE);
        let approval = async {
            let CodexEvent::Request {
                id,
                method,
                params: Some(params),
            } = event(&mut server).await
            else {
                panic!("expected approval request");
            };
            assert_eq!(method, "item/commandExecution/requestApproval");
            assert_eq!(params["kind"], "command");
            assert_eq!(id, RequestId::Number(0.into()));
            client
                .respond(id.clone(), Ok(json!({"decision": "accept"})))
                .await
                .unwrap();
            assert!(matches!(
                client.respond(id, Ok(Value::Null)).await,
                Err(CodexError::RequestAlreadyResolved)
            ));
        };
        let (result, ()) = tokio::join!(start, approval);
        assert_eq!(result.unwrap(), response["result"]);
        assert!(
            matches!(event(&mut server).await, CodexEvent::Notification { method, .. } if method == "serverRequest/resolved")
        );
        assert!(
            matches!(event(&mut server).await, CodexEvent::Notification { method, .. } if method == "turn/completed")
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn routes_out_of_order_replies_and_retires_resolved_string_request() {
        let peer = Peer::new(
            r#"
IFS= read -r first
IFS= read -r second
case "$first" in *'"id":2'*'"method":"first"'*) ;; *) exit 24 ;; esac
case "$second" in *'"id":3'*'"method":"second"'*) ;; *) exit 25 ;; esac
printf '%s\n' '{"id":3.0,"error":{"code":-32601,"message":"unknown method"}}'
printf '%s\n' '{"id":"approval-1","method":"item/tool/requestUserInput","params":null}'
printf '%s\n' '{"method":"serverRequest/resolved","params":{"requestId":"approval-1"}}'
printf '%s\n' '{"id":"2","result":"first answer"}'
while IFS= read -r line; do :; done
"#,
        );
        let mut server = peer.launch().await;
        let client = server.client();
        let (first, second) = tokio::join!(biased; client.request("first", None, DEADLINE), client.request("second", None, DEADLINE));
        assert_eq!(first.unwrap(), "first answer");
        assert!(
            matches!(second, Err(CodexError::Remote { method, error }) if method == "second" && error.code == -32601)
        );
        let CodexEvent::Request {
            id,
            params: Some(Value::Null),
            ..
        } = event(&mut server).await
        else {
            panic!("expected user input request");
        };
        assert_eq!(id, RequestId::Text("approval-1".into()));
        assert!(
            matches!(event(&mut server).await, CodexEvent::Notification { method, .. } if method == "serverRequest/resolved")
        );
        assert!(matches!(
            client.respond(id, Ok(Value::Null)).await,
            Err(CodexError::RequestAlreadyResolved)
        ));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delivers_a_notification_burst_and_preserves_the_following_reply() {
        let count = QUEUE_CAPACITY * 3;
        let frames = notification_burst(count);
        let peer = Peer::new(&format!(
            r#"
IFS= read -r fill
case "$fill" in *'"id":2'*'"method":"burst"'*) ;; *) exit 27 ;; esac
{frames}
printf '%s\n' '{{"id":2,"result":"finished"}}'
while IFS= read -r line; do :; done
"#
        ));
        let mut server = peer.launch().await;
        let client = server.client();
        let (reply, ()) = tokio::join!(
            client.request("burst", None, DEADLINE),
            assert_notification_burst(&mut server, count)
        );
        assert_eq!(reply.unwrap(), "finished");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delivers_startup_notifications_after_initialization() {
        let count = QUEUE_CAPACITY * 3;
        let peer = Peer::with_startup_output(
            &notification_burst(count),
            "while IFS= read -r line; do :; done",
        );
        let mut server = peer.launch().await;
        assert_notification_burst(&mut server, count).await;
        server.shutdown().await.unwrap();
    }

    fn notification_burst(count: usize) -> String {
        let mut frames = String::new();
        let mut delta = record("emit_inbound", "item/agentMessage/delta");
        for index in 0..count {
            delta["params"]["delta"] = json!(index.to_string());
            frames.push_str(&emit(&delta));
            frames.push('\n');
        }
        frames
    }

    async fn assert_notification_burst(server: &mut CodexAppServer, count: usize) {
        for expected in 0..count {
            assert!(matches!(event(server).await,
                CodexEvent::Notification { method, params: Some(params) }
                if method == "item/agentMessage/delta"
                    && params["delta"].as_str().and_then(|delta| delta.parse().ok()) == Some(expected)));
        }
    }

    #[tokio::test]
    async fn fails_pending_requests_when_the_process_exits() {
        let peer = Peer::new(
            r#"
IFS= read -r line
printf 'fatal: fixture exit\n' >&2
exit 17
"#,
        );
        let mut server = peer.launch().await;
        let error = server
            .client()
            .request("hang", None, DEADLINE)
            .await
            .unwrap_err();
        assert!(
            matches!(error, CodexError::Exited { code: Some(17), diagnostic: Some(diagnostic) } if diagnostic == "fatal: fixture exit")
        );
        assert!(matches!(
            server.next_event().await,
            Err(CodexError::Exited { code: Some(17), .. })
        ));
    }

    #[tokio::test]
    async fn reaps_a_process_that_ignores_end_of_input() {
        let peer = Peer::new("while IFS= read -r line; do :; done\nwhile :; do :; done");
        let server = peer.launch().await;
        let client = server.client();
        timeout(DEADLINE, server.shutdown()).await.unwrap().unwrap();
        assert!(matches!(
            client.request("after shutdown", None, DEADLINE).await,
            Err(CodexError::Closed)
        ));
    }
}
