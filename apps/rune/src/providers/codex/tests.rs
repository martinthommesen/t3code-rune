use super::*;
use crate::providers::CodexEvent;

const APPROVAL_RECORDING: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_read_only_on_request/codex_transcript.ndjson"
);
const INPUT_RECORDING: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/plan_questions/codex_transcript.ndjson"
);

fn recorded(recording: &str, label: &str) -> Value {
    recorded_matching(recording, |row| row["label"] == label)
}

fn recorded_matching(recording: &str, matches: impl Fn(&Value) -> bool) -> Value {
    recording
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["type"] == "emit_inbound" && matches(row))
        .expect("recorded frame")["frame"]
        .clone()
}

fn decode_request(frame: Value) -> CodexSessionEvent {
    events::decode(
        CodexEvent::Request {
            id: serde_json::from_value(frame["id"].clone()).unwrap(),
            method: frame["method"].as_str().unwrap().into(),
            params: frame.get("params").cloned(),
        },
        Uuid::nil(),
    )
    .unwrap()
    .unwrap()
}

#[test]
fn translates_recorded_user_input_and_keeps_question_identity() {
    let frame = recorded(INPUT_RECORDING, "item/tool/requestUserInput");
    let CodexSessionEvent::Turn {
        key,
        event: TurnEvent::UserInputRequested { request },
    } = decode_request(frame.clone())
    else {
        panic!("expected a user input event");
    };
    assert_eq!(
        key.thread_id.as_str(),
        frame["params"]["threadId"].as_str().unwrap()
    );
    assert_eq!(
        key.turn_id.as_str(),
        frame["params"]["turnId"].as_str().unwrap()
    );
    assert_eq!(request.item_id, frame["params"]["itemId"].as_str().unwrap());
    assert_eq!(request.questions[0].id, "schema_preference");
    assert!(request.questions[0].is_other);
    assert_eq!(
        request.questions[0].options.as_ref().unwrap()[0].label,
        "Strict schemas (Recommended)"
    );
}

#[test]
fn normalizes_web_search_actions_in_provider_order() {
    for (action, query, expected) in [
        (Value::Null, " \u{feff} ", vec![]),
        (
            json!({"type":"search", "queries":[" \u{feff}alpha ", "beta", "alpha"], "query":"beta"}),
            "\u{85}gamma\u{85}",
            vec!["alpha", "beta", "\u{85}gamma\u{85}"],
        ),
        (
            json!({"type":"openPage", "url":" example.test "}),
            " fallback ",
            vec!["example.test", "fallback"],
        ),
        (
            json!({"type":"findInPage", "pattern":" topic ", "url":"example.test"}),
            "topic",
            vec!["topic", "example.test"],
        ),
        (json!({"type":"other"}), "fallback", vec!["fallback"]),
    ] {
        let event = events::decode(
            CodexEvent::Notification {
                method: "item/completed".into(),
                params: Some(
                    json!({"threadId":"native-thread", "turnId":"native-turn", "item":{
                        "type":"webSearch", "id":"native-search", "action":action, "query":query,
                        "results":[{"title":"The provider result body"}],
                    }}),
                ),
            },
            Uuid::nil(),
        )
        .unwrap()
        .unwrap();
        let CodexSessionEvent::Turn {
            event: TurnEvent::WebSearch(search),
            ..
        } = event
        else {
            panic!("expected a web search item");
        };
        assert_eq!(search.lifecycle, ToolLifecycle::Completed);
        assert_eq!(search.item_id, "native-search");
        assert_eq!(search.patterns, expected);
    }
}

#[cfg(unix)]
mod subprocess {
    use std::{collections::HashMap, fs};

    use super::*;

    const INTERRUPT_RECORDING: &str = include_str!(
        "../../../../server/src/orchestration-v2/testkit/fixtures/turn_interrupt/codex_transcript.ndjson"
    );
    const TOOL_RECORDING: &str = include_str!(
        "../../../../server/src/orchestration-v2/testkit/fixtures/command_output_streaming/codex_transcript.ndjson"
    );

    struct Replay {
        directory: PathBuf,
        script: String,
    }

    impl Replay {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("t3-codex-run-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            Self {
                directory,
                script: "set -eu\n".into(),
            }
        }

        fn step(&mut self, frames: &[Value]) {
            self.script
                .push_str("IFS= read -r line\nprintf '%s\\n' \"$line\" >> trace.ndjson\n");
            for value in frames {
                let escaped = value.to_string().replace('\'', "'\"'\"'");
                self.script
                    .push_str(&format!("printf '%s\\n' '{escaped}'\n"));
            }
        }

        async fn launch(&mut self) -> CodexSession {
            self.script
                .push_str("while IFS= read -r line; do :; done\n");
            fs::write(self.directory.join("app-server"), &self.script).unwrap();
            CodexSession::launch(CodexLaunch {
                executable: PathBuf::from("/bin/sh"),
                working_directory: self.directory.clone(),
                arguments: vec![],
                environment: HashMap::new(),
                client_version: "test".into(),
            })
            .await
            .unwrap()
        }

        fn trace(&self) -> Vec<Value> {
            fs::read_to_string(self.directory.join("trace.ndjson"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl Drop for Replay {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn reply(label: &str, id: u64) -> Value {
        let mut frame = recorded(APPROVAL_RECORDING, label);
        frame["id"] = json!(id);
        frame
    }

    fn with_turn(mut frame: Value, key: &TurnKey) -> Value {
        frame["params"]["threadId"] = json!(key.thread_id);
        if let Some(turn) = frame["params"].get_mut("turn") {
            turn["id"] = json!(key.turn_id);
        } else {
            frame["params"]["turnId"] = json!(key.turn_id);
        }
        frame
    }

    async fn next_turn_event(session: &mut CodexSession, expected_key: &TurnKey) -> TurnEvent {
        let CodexSessionEvent::Turn { key, event } = session.next_event().await.unwrap() else {
            panic!("expected turn event");
        };
        assert_eq!(&key, expected_key);
        event
    }

    #[tokio::test]
    async fn retries_a_native_thread_name_after_the_app_server_rejects_it() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/simple/codex_transcript.ndjson"
        );
        let mut replay = Replay::new();
        replay.step(&[reply("initialize", 1)]);
        replay.step(&[]);
        let mut start = recorded(RECORDING, "thread/start");
        start["id"] = json!(2);
        replay.step(&[start]);
        replay.step(&[json!({"id": 3, "error": {"code": -32601, "message": "Method not found"}})]);
        replay.step(&[recorded(RECORDING, "thread/name/set")]);

        let session = replay.launch().await;
        let client = session.client();
        let thread_id = client
            .start_thread(&ThreadOptions {
                cwd: replay.directory.clone(),
                model: "gpt-6-luna".into(),
                app_thread_id: None,
                mcp: None,
            })
            .await
            .unwrap();
        for _ in 0..3 {
            client
                .apply_thread_name(&thread_id, "Replay fixture: simple")
                .await;
        }
        session.shutdown().await.unwrap();

        let requests: Vec<_> = replay
            .trace()
            .into_iter()
            .filter(|frame| frame["method"] == "thread/name/set")
            .collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["id"], 3);
        assert_eq!(requests[1]["id"], 4);
        assert_eq!(
            requests[1]["params"],
            json!({"threadId": thread_id, "name": "Replay fixture: simple"})
        );
    }

    #[tokio::test]
    async fn verifies_terminal_absence_when_codex_does_not_confirm_termination() {
        let mut replay = Replay::new();
        replay.step(&[reply("initialize", 1)]);
        replay.step(&[]);
        replay.step(&[json!({"id": 2, "result": {"terminated": false}})]);
        replay.step(&[json!({"id": 3, "result": {"data": [], "nextCursor": "second"}})]);
        replay.step(&[
            json!({"id": 4, "result": {"data": [{"processId": "another"}], "nextCursor": null}}),
        ]);
        replay.step(&[json!({"id": 5, "result": {"terminated": false}})]);
        replay.step(&[
            json!({"id": 6, "result": {"data": [{"processId": "owned"}], "nextCursor": null}}),
        ]);
        let session = replay.launch().await;
        let client = session.client();
        let thread_id = ThreadId::new("native-thread");
        client
            .terminate_background_terminal(&thread_id, "owned")
            .await
            .unwrap();
        let error = client
            .terminate_background_terminal(&thread_id, "owned")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CodexAdapterError::BackgroundTerminalStillActive
        ));
        session.shutdown().await.unwrap();
        let trace = replay.trace();
        assert_eq!(trace[2]["method"], "thread/backgroundTerminals/terminate");
        assert_eq!(
            trace[2]["params"],
            json!({"threadId":thread_id, "processId":"owned"})
        );
        assert_eq!(trace[3]["method"], "thread/backgroundTerminals/list");
        assert_eq!(
            trace[4]["params"],
            json!({"threadId":thread_id, "cursor":"second"})
        );
    }

    #[tokio::test]
    async fn preserves_captured_message_phases_timing_and_usage() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/simple/codex_transcript.ndjson"
        );
        let settings = recorded(RECORDING, "thread/settings/updated");
        let started = recorded(RECORDING, "turn/started");
        let key = TurnKey {
            thread_id: ThreadId::new(started["params"]["threadId"].as_str().unwrap()),
            turn_id: TurnId::new(started["params"]["turn"]["id"].as_str().unwrap()),
        };
        let started_text = recorded_matching(RECORDING, |row| {
            row["label"] == "item/started"
                && row["frame"]["params"]["item"]["type"] == "agentMessage"
        });
        let text_completed = recorded_matching(RECORDING, |row| {
            row["label"] == "item/completed"
                && row["frame"]["params"]["item"]["type"] == "agentMessage"
        });
        let mut replay = Replay::new();
        replay.step(&[reply("initialize", 1)]);
        replay.step(&[
            settings,
            started,
            started_text,
            recorded(RECORDING, "item/agentMessage/delta"),
            text_completed,
            recorded(RECORDING, "thread/tokenUsage/updated"),
            recorded(RECORDING, "turn/completed"),
        ]);
        let mut session = replay.launch().await;
        assert!(matches!(
            session.next_event().await.unwrap(),
            CodexSessionEvent::ThreadModelChanged { thread_id, model }
                if thread_id == key.thread_id && model == "gpt-6-luna",
        ));
        assert!(matches!(
            next_turn_event(&mut session, &key).await,
            TurnEvent::Started {
                started_at: Some(1_790_295_404)
            },
        ));
        assert!(matches!(
            next_turn_event(&mut session, &key).await,
            TurnEvent::TextStarted {
                phase: Some(AssistantPhase::FinalAnswer),
                started_at_ms: Some(1_790_295_409_066),
                ..
            },
        ));
        assert!(matches!(
            next_turn_event(&mut session, &key).await,
            TurnEvent::TextDelta { delta, .. } if delta == "fixture",
        ));
        assert!(matches!(
            next_turn_event(&mut session, &key).await,
            TurnEvent::TextCompleted {
                text, phase: Some(AssistantPhase::FinalAnswer),
                completed_at_ms: Some(1_790_295_409_161),
                ..
            } if text == "fixture simple ok",
        ));
        let TurnEvent::TokenUsageUpdated { usage } = next_turn_event(&mut session, &key).await
        else {
            panic!("expected the captured usage update");
        };
        let observed_at = "2026-09-25T00:16:49.209Z";
        assert_eq!(
            serde_json::to_value(usage.context_usage(observed_at.into())).unwrap(),
            json!({
                "usedTokens": 22737,
                "maxTokens": 258400,
                "inputTokens": 22730,
                "cachedInputTokens": 6912,
                "outputTokens": 7,
                "reasoningOutputTokens": 0,
                "updatedAt": observed_at,
            })
        );
        let TurnEvent::Settled {
            status: SettlementStatus::Completed,
            completed_at: Some(1_790_295_409),
            usage,
            ..
        } = next_turn_event(&mut session, &key).await
        else {
            panic!("expected the completed turn and its usage");
        };
        assert_eq!(
            serde_json::to_value(usage).unwrap(),
            json!({
                "usageStatus": "complete",
                "usageScope": "main_agent",
                "inputTokens": 22730,
                "cachedInputTokens": 6912,
                "cacheCreationTokens": 0,
                "outputTokens": 7,
                "reasoningTokens": 0,
                "hasSubagents": false,
                "models": ["gpt-6-luna"],
            })
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn starts_resumes_and_interrupts_a_turn_with_recorded_tool_and_approval_frames() {
        let thread = reply("thread/start", 2);
        let started = reply("turn/start", 7);
        let key = TurnKey {
            thread_id: ThreadId::new(thread["result"]["thread"]["id"].as_str().unwrap()),
            turn_id: TurnId::new(started["result"]["turn"]["id"].as_str().unwrap()),
        };
        let command_start = recorded_matching(APPROVAL_RECORDING, |row| {
            row["label"] == "item/started"
                && row["frame"]["params"]["item"]["type"] == "commandExecution"
        });
        let command_delta = with_turn(
            recorded(TOOL_RECORDING, "item/commandExecution/outputDelta"),
            &key,
        );
        let response = recorded_matching(APPROVAL_RECORDING, |row| {
            row["label"] == "item/completed"
                && row["frame"]["params"]["item"]["type"] == "agentMessage"
        });
        let mut completion = with_turn(recorded(INTERRUPT_RECORDING, "turn/completed"), &key);
        completion["params"]["turn"]["status"] = json!("completed");
        let approval = recorded(APPROVAL_RECORDING, "item/commandExecution/requestApproval");
        let mut second_approval = approval.clone();
        second_approval["id"] = json!(1);
        second_approval["params"]["itemId"] = json!("different-command");
        second_approval["params"]["command"] = json!("printf hello");
        let later_key = TurnKey {
            thread_id: key.thread_id.clone(),
            turn_id: TurnId::new("later-turn"),
        };
        let mut later_approval = with_turn(second_approval.clone(), &later_key);
        later_approval["id"] = json!(2);
        let mut later_turn = reply("turn/start", 10);
        later_turn["result"]["turn"]["id"] = json!(later_key.turn_id);
        let mut replay = Replay::new();
        replay.step(&[reply("initialize", 1)]);
        replay.step(&[]);
        replay.step(&[thread]);
        replay.step(&[json!({"id": 3, "error": {"code": -32600, "message": format!("session {} is archived", key.thread_id.as_str())}})]);
        replay.step(&[json!({"id": 4, "result": {}})]);
        replay.step(&[reply("thread/start", 5)]);
        replay.step(&[
            json!({"id": 6, "result": {"config": {"model_reasoning_summary": "detailed"}}}),
        ]);
        replay.step(&[
            recorded(APPROVAL_RECORDING, "turn/started"),
            started,
            command_start,
            command_delta.clone(),
            json!({"id": 99, "method": "item/commandExecution/requestApproval", "params": {"itemId": "missing-turn"}}),
        ]);
        replay.step(std::slice::from_ref(&approval));
        replay.step(&[second_approval]);
        replay.step(&[
            recorded(APPROVAL_RECORDING, "serverRequest/resolved"),
            recorded(APPROVAL_RECORDING, "item/agentMessage/delta"),
            response.clone(),
        ]);
        replay.step(&[completion.clone(), json!({"id": 8, "result": {}})]);
        replay.step(&[json!({"id": 9, "result": {"config": {}}})]);
        replay.step(&[later_turn, later_approval]);
        replay.step(&[json!({"method": "serverRequest/resolved", "params": {"threadId": key.thread_id, "requestId": 2}})]);

        let mut session = replay.launch().await;
        let client = session.client();
        let thread = ThreadOptions {
            cwd: replay.directory.clone(),
            model: "gpt-6-sol".into(),
            app_thread_id: Some("local-thread".into()),
            mcp: None,
        };
        assert_eq!(client.start_thread(&thread).await.unwrap(), key.thread_id);
        assert_eq!(
            client.resume_thread(&key.thread_id, &thread).await.unwrap(),
            key.thread_id
        );
        let options = TurnOptions {
            cwd: thread.cwd.clone(),
            model: thread.model.clone(),
            text: "  Send this exact text  ".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            interaction_mode: InteractionMode::Plan,
            reasoning_effort: Some("high".into()),
            service_tier: None,
        };
        let turn = client.start_turn(&key.thread_id, &options).await.unwrap();
        assert_eq!(turn.key, key);
        assert_eq!(turn.status, TurnStatus::Running);
        assert!(matches!(
            next_turn_event(&mut session, &key).await,
            TurnEvent::Started {
                started_at: Some(_)
            }
        ));
        assert!(matches!(next_turn_event(&mut session, &key).await,
            TurnEvent::ToolChanged {item, lifecycle: ToolLifecycle::Started} if item.kind == "commandExecution"));
        assert!(matches!(next_turn_event(&mut session, &key).await,
            TurnEvent::ToolOutputDelta {delta, ..} if delta == command_delta["params"]["delta"].as_str().unwrap()));
        let TurnEvent::ApprovalRequested { request } = next_turn_event(&mut session, &key).await
        else {
            panic!("expected approval request");
        };
        assert_eq!(request.request_id, RequestId::Number(0.into()));
        assert_eq!(request.kind, ApprovalKind::Command);

        let mut other_replay = Replay::new();
        other_replay.step(&[reply("initialize", 1)]);
        other_replay.step(&[approval]);
        other_replay.step(&[recorded(APPROVAL_RECORDING, "serverRequest/resolved")]);
        let mut other_session = other_replay.launch().await;
        let other_client = other_session.client();
        let TurnEvent::ApprovalRequested {
            request: other_request,
        } = next_turn_event(&mut other_session, &key).await
        else {
            panic!("expected an approval from the second session");
        };
        assert_eq!(other_request.request_id, request.request_id);
        assert!(matches!(
            other_client
                .answer_approval(&request, ApprovalDecision::Accept)
                .await,
            Err(CodexAdapterError::RequestSessionMismatch)
        ));
        other_client
            .answer_approval(&other_request, ApprovalDecision::Decline)
            .await
            .unwrap();
        assert!(
            matches!(other_session.next_event().await.unwrap(), CodexSessionEvent::RequestResolved { request_id, .. } if request_id == other_request.request_id)
        );
        other_session.shutdown().await.unwrap();
        assert_eq!(
            other_replay.trace()[2],
            json!({"id": 0, "result": {"decision": "decline"}})
        );

        client
            .answer_approval(&request, ApprovalDecision::AcceptForSession)
            .await
            .unwrap();
        assert!(
            matches!(session.next_event().await.unwrap(), CodexSessionEvent::RequestResolved {thread_id, request_id} if thread_id == key.thread_id && request_id == request.request_id)
        );
        assert!(matches!(
            next_turn_event(&mut session, &key).await,
            TurnEvent::TextDelta { .. }
        ));
        assert!(matches!(next_turn_event(&mut session, &key).await,
            TurnEvent::TextCompleted {text, ..} if text == response["params"]["item"]["text"].as_str().unwrap()));
        let (interrupted, settled) = tokio::join!(
            client.interrupt_turn(&key),
            next_turn_event(&mut session, &key)
        );
        interrupted.unwrap();
        assert!(matches!(settled,
            TurnEvent::Settled {status: SettlementStatus::Interrupted, completed_at, ..}
            if completed_at == completion["params"]["turn"]["completedAt"].as_i64()));

        let changed_options = TurnOptions {
            model: "openai.gpt-6-astra".into(),
            runtime_mode: RuntimeMode::AutoAcceptEdits,
            reasoning_effort: None,
            service_tier: None,
            ..options.clone()
        };
        assert_eq!(
            client
                .start_turn(&key.thread_id, &changed_options)
                .await
                .unwrap()
                .key,
            later_key
        );
        let TurnEvent::ApprovalRequested { request } =
            next_turn_event(&mut session, &later_key).await
        else {
            panic!("runtime mode change should invalidate the earlier approval allowance");
        };
        client
            .answer_approval(&request, ApprovalDecision::Decline)
            .await
            .unwrap();
        assert!(
            matches!(session.next_event().await.unwrap(), CodexSessionEvent::RequestResolved {request_id, ..} if request_id == request.request_id)
        );
        session.shutdown().await.unwrap();

        let trace = replay.trace();
        assert_eq!(trace[2]["method"], "thread/start");
        assert_eq!(
            trace[2]["params"]["config"]["shell_environment_policy.set.T3CODE_THREAD_ID"],
            "local-thread"
        );
        assert_eq!(trace[3]["method"], "thread/resume");
        assert_eq!(trace[3]["params"]["excludeTurns"], true);
        assert_eq!(trace[4]["method"], "thread/unarchive");
        assert_eq!(trace[4]["params"]["threadId"], key.thread_id.as_str());
        assert_eq!(trace[5]["params"], trace[3]["params"]);
        assert_eq!(trace[6]["method"], "config/read");
        let sent = &trace[7]["params"];
        assert_eq!(trace[7]["method"], "turn/start");
        assert_eq!(sent["input"][0]["text"], options.text);
        assert_eq!(sent["threadId"], key.thread_id.as_str());
        assert_eq!(sent["summary"], "detailed");
        assert_eq!(sent["approvalPolicy"], "untrusted");
        assert_eq!(sent["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(sent["collaborationMode"]["mode"], "plan");
        assert_eq!(
            sent["collaborationMode"]["settings"]["reasoning_effort"],
            "high"
        );
        assert_eq!(trace[8]["id"], 99);
        assert_eq!(trace[8]["error"]["code"], -32602);
        assert_eq!(
            trace[9],
            json!({"id": 0, "result": {"decision": "acceptForSession"}})
        );
        assert_eq!(
            trace[10],
            json!({"id": 1, "result": {"decision": "acceptForSession"}})
        );
        assert_eq!(trace[11]["method"], "turn/interrupt");
        assert_eq!(trace[11]["params"], json!(key));
        assert_eq!(trace[13]["params"]["effort"], "medium");
        assert_eq!(
            trace[14],
            json!({"id": 2, "result": {"decision": "decline"}})
        );
    }
}
