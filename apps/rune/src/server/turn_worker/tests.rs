#[cfg(unix)]
mod subprocess {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use serde_json::{Value, json};

    use super::super::*;
    use crate::server::turn_commands::tests::{COMMAND, Fixture, THREAD, command, command_payload};

    const CODEX: &str = include_str!(
        "../../../../server/src/orchestration-v2/testkit/fixtures/simple/codex_transcript.ndjson"
    );

    #[tokio::test]
    async fn runs_a_committed_turn_through_captured_codex_frames() {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let (updates, mut receiver) = broadcast::channel(32);
        let settings = settings_with_recording(&fixture, "");
        let worker = TurnWorker::spawn(commands, settings, updates);
        let sender = worker.sender();
        sender.wake();
        sender.clone().wake();
        let final_sequence = wait_for_run(&fixture, "completed").await;
        assert_eq!(final_sequence, receipt.sequence + 21);
        worker.shutdown().await.unwrap();

        let mut advertised = 0;
        while let Ok(ProjectionChange::Thread(thread_id)) = receiver.try_recv() {
            assert_eq!(thread_id, THREAD);
            advertised += 1;
        }
        assert_eq!(advertised, 9);
        let connection = fixture.connection();
        let saved: String = connection
            .query_row(
                "SELECT json_extract(payload_json, '$.text')
                 FROM orchestration_v2_projection_messages WHERE thread_id = ?1 AND role = 'assistant'",
                [THREAD],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, "fixture simple ok");
    }

    #[tokio::test]
    async fn mirrors_titles_to_the_live_codex_thread_and_deduplicates_names() {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let mut script = "#!/bin/sh\nset -eu\n".to_owned();
        append_request(&mut script, &[reply("initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[reply("thread/start", 2)]);
        append_request(&mut script, &[reply("thread/name/set", 3)]);
        append_request(&mut script, &[reply("config/read", 4)]);
        append_request(
            &mut script,
            &[
                reply("turn/start", 5),
                notification("turn/started", |_| true),
            ],
        );
        append_request(&mut script, &[reply("thread/name/set", 6)]);
        script.push_str("while [ ! -f finish-captured-turn ]; do sleep 0.02; done\n");
        emit_frame(
            &mut script,
            &notification("thread/tokenUsage/updated", |_| true),
        );
        emit_frame(&mut script, &notification("turn/completed", |_| true));
        script.push_str("while IFS= read -r request; do :; done\n");

        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes,
        );
        let queue = worker.sender();
        let offline = enqueue_name_effect(&fixture, "offline", "No attached session");
        queue.wake();
        wait_for_title_effect(&fixture, &offline).await;
        assert!(!fixture.directory.join("requests.ndjson").exists());

        commands.dispatch(command()).unwrap();
        queue.wake();
        wait_for_run(&fixture, "running").await;
        let live = enqueue_name_effect(&fixture, "live", "Unicode title é");
        queue.wake();
        wait_for_title_effect(&fixture, &live).await;
        let duplicate = enqueue_name_effect(&fixture, "duplicate", "Unicode title é");
        queue.wake();
        wait_for_title_effect(&fixture, &duplicate).await;

        fs::write(fixture.directory.join("finish-captured-turn"), "").unwrap();
        wait_for_run(&fixture, "completed").await;
        worker.shutdown().await.unwrap();
        let requests: Vec<Value> = fs::read_to_string(fixture.directory.join("requests.ndjson"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let names: Vec<_> = requests
            .iter()
            .filter(|frame| frame["method"] == "thread/name/set")
            .map(|frame| &frame["params"])
            .collect();
        let native = &reply("thread/start", 2)["result"]["thread"]["id"];
        assert_eq!(
            names,
            [
                &json!({ "threadId": native, "name": "New thread" }),
                &json!({ "threadId": native, "name": "Unicode title é" }),
            ]
        );
    }

    fn enqueue_name_effect(fixture: &Fixture, command_id: &str, title: &str) -> String {
        let effect_id = format!("effect:{command_id}:provider-session.apply-thread-title");
        fixture.connection().execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, 'provider-session.apply-thread-title', ?4, 'pending', 0, ?5, ?5, ?5)",
            rusqlite::params![effect_id, command_id, THREAD,
                json!({ "type": "provider-session.apply-thread-title", "title": title }).to_string(),
                "2026-01-01T00:00:00.000Z",
            ],
        ).unwrap();
        effect_id
    }

    async fn wait_for_title_effect(fixture: &Fixture, effect_id: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let done: bool = fixture
                    .connection()
                    .query_row(
                        "SELECT status = 'succeeded' AND attempt_count = 1 AND lease_owner IS NULL
                            AND completed_at IS NOT NULL AND last_error IS NULL
                     FROM orchestration_v2_effect_outbox WHERE effect_id = ?1",
                        [effect_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_launch_failure_does_not_leave_a_start_owned() {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let effect_id = format!("effect:{COMMAND}:provider-turn.start:{}", receipt.run_id);
        let settings = fixture.directory.join("failure-settings.json");
        fs::write(
            &settings,
            json!({"providerInstances":{"codex":{"driver":"codex","config":{
                "binaryPath": fixture.directory.join("missing-codex"),
            }}}})
            .to_string(),
        )
        .unwrap();
        let (updates, mut receiver) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands, SettingsFile::new(settings), updates);
        worker.sender().wake();
        wait_for_run(&fixture, "failed").await;
        worker.shutdown().await.unwrap();
        assert!(
            matches!(receiver.try_recv().unwrap(), ProjectionChange::Thread(thread) if thread == THREAD)
        );

        let connection = fixture.connection();
        let (run, effect, title, owner) = connection
            .query_row(
                "SELECT run.status, effect.status, json_extract(item.payload_json, '$.title'),
                    effect.lease_owner
             FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = run.thread_id
             JOIN orchestration_v2_projection_turn_items AS item ON item.run_id = run.run_id
             WHERE run.run_id = ?1 AND effect.effect_id = ?2 AND item.type = 'error'",
                rusqlite::params![receipt.run_id, effect_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(run, "failed");
        assert_eq!(effect, "succeeded");
        assert_eq!(title, "Provider session failed to open");
        assert!(owner.is_none());
    }

    #[test]
    fn process_loss_holds_the_unaccepted_prompt_without_resending() {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let effect_id = format!("effect:{COMMAND}:provider-turn.start:{}", receipt.run_id);
        assert!(
            commands
                .claim_start(&effect_id, "lost-worker")
                .unwrap()
                .is_some()
        );
        let (updates, mut receiver) = broadcast::channel(32);
        TurnWorker::recover_before_spawn(&commands, &updates).unwrap();
        assert!(
            matches!(receiver.try_recv().unwrap(), ProjectionChange::Thread(thread) if thread == THREAD)
        );
        assert!(
            commands
                .claim_start(&effect_id, "next-worker")
                .unwrap()
                .is_none()
        );
        let connection = fixture.connection();
        let (run, held, item, effect, recovery_receipts) = connection.query_row(
            "SELECT run.status, json_extract(run.payload_json, '$.queueHeld'),
                     json_extract(item.payload_json, '$.inputIntent'), effect.status,
                     (SELECT count(*) FROM orchestration_command_receipts
                      WHERE command_type = 'provider-runtime.reconcile' AND aggregate_id = run.thread_id)
             FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = run.thread_id
             JOIN orchestration_v2_projection_turn_items AS item ON item.run_id = run.run_id
             WHERE run.run_id = ?1 AND effect.effect_id = ?2 AND item.type = 'user_message'",
            rusqlite::params![receipt.run_id, effect_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?)),
        ).unwrap();
        assert_eq!(run, "queued");
        assert!(held);
        assert_eq!(item, "queued_turn");
        assert_eq!(effect, "cancelled");
        assert_eq!(recovery_receipts, 1);
    }

    #[tokio::test]
    async fn captures_the_baseline_before_the_provider_edits_git() {
        let fixture = Fixture::new();
        let workspace = fixture.directory.join("repository");
        fs::create_dir(&workspace).unwrap();
        let git = |arguments: &[&str]| {
            let result = std::process::Command::new("git")
                .current_dir(&workspace)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            result.stdout
        };
        git(&["init", "--quiet"]);
        fs::write(
            workspace.join("main.txt"),
            "alpha
",
        )
        .unwrap();
        git(&["add", "main.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Baseline",
        ]);

        let connection = fixture.connection();
        connection
            .execute(
                "UPDATE projection_projects SET workspace_root = ?1",
                [workspace.to_str().unwrap()],
            )
            .unwrap();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let (updates, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands,
            settings_with_recording(
                &fixture,
                "printf 'alpha\nbeta\n' > main.txt
",
            ),
            updates,
        );
        worker.sender().wake();
        assert_eq!(
            wait_for_run(&fixture, "completed").await,
            receipt.sequence + 21
        );
        worker.shutdown().await.unwrap();

        let captured: Vec<(String, String)> = {
            let mut query = connection
                .prepare(
                    "SELECT status, json_extract(payload_json, '$.files')
                 FROM orchestration_v2_projection_checkpoints WHERE thread_id = ?1
                 ORDER BY ordinal_within_scope",
                )
                .unwrap();
            query
                .query_map([THREAD], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(captured[0], ("ready".to_owned(), "[]".to_owned()));
        assert_eq!(captured[1].0, "ready");
        assert_eq!(
            serde_json::from_str::<Value>(&captured[1].1).unwrap(),
            json!([{ "path": "main.txt", "kind": "modified", "additions": 1, "deletions": 0}]),
        );
        assert_eq!(
            git(&["status", "--short"]),
            b" M main.txt
"
        );
    }

    #[tokio::test]
    async fn shutdown_closes_a_blocked_provider_start_and_holds_the_prompt() {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let effect_id = format!("effect:{COMMAND}:provider-turn.start:{}", receipt.run_id);
        let initialize = reply("initialize", 1).to_string().replace('\'', "'\"'\"'");
        let script = format!(
            "#!/bin/sh
IFS= read -r request
printf '%s\n' '{initialize}'
while IFS= read -r request; do :; done
"
        );
        let (updates, mut receiver) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands, settings_for_script(&fixture, &script), updates);
        worker.sender().wake();
        let attached = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(attached, ProjectionChange::Thread(thread) if thread == THREAD));
        tokio::time::timeout(Duration::from_secs(5), worker.shutdown())
            .await
            .unwrap()
            .unwrap();
        wait_for_run(&fixture, "queued").await;
        let (session, effect): (String, String) = fixture
            .connection()
            .query_row(
                "SELECT session.status, effect.status
             FROM orchestration_v2_projection_provider_sessions AS session
             JOIN orchestration_v2_effect_outbox AS effect ON session.thread_id = effect.thread_id
             WHERE effect.effect_id = ?1",
                [effect_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(session, "stopped");
        assert_eq!(effect, "cancelled");
    }

    #[tokio::test]
    async fn accepts_a_captured_command_approval_only_after_the_durable_decision() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_read_only_on_request/codex_transcript.ndjson"
        );
        runtime_request_roundtrip(
            RECORDING,
            "item/commandExecution/requestApproval",
            "command",
            json!({ "decision": "accept" }),
        )
        .await;
    }

    #[tokio::test]
    async fn persists_the_recorded_file_change_and_accepts_its_durable_approval() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_restricted_granular/codex_transcript.ndjson"
        );
        runtime_request_roundtrip(
            RECORDING,
            "item/fileChange/requestApproval",
            "file-change",
            json!({ "decision": "accept" }),
        )
        .await;
    }

    #[tokio::test]
    async fn sends_the_answer_to_the_captured_live_question() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/plan_questions/codex_transcript.ndjson"
        );
        runtime_request_roundtrip(
            RECORDING,
            "item/tool/requestUserInput",
            "user_input",
            json!({ "answers": {"schema_preference":["Strict schemas (Recommended)"]} }),
        )
        .await;
    }

    #[tokio::test]
    async fn shutdown_cancels_a_live_approval_and_the_bound_native_turn() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_read_only_on_request/codex_transcript.ndjson"
        );
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let (settings, _) =
            runtime_recording(&fixture, RECORDING, "item/commandExecution/requestApproval");
        let (updates, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands, settings, updates);
        worker.sender().wake();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let requests: i64 = fixture
                    .connection()
                    .query_row(
                        "SELECT count(*) FROM orchestration_v2_projection_runtime_requests
                     WHERE thread_id = ?1 AND status = 'pending'",
                        [THREAD],
                        |row| row.get(0),
                    )
                    .unwrap();
                if requests == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), worker.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(!fixture.directory.join("response.json").exists());
        let connection = fixture.connection();
        let cancelled: (String, String, String, String, String) = connection.query_row(
            "SELECT run.status, attempt.status, provider_turn.status,
                    request.status, json_extract(request.payload_json, '$.responseCapability.type')
             FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.run_id = run.run_id
             JOIN orchestration_v2_projection_provider_turns AS provider_turn ON provider_turn.run_attempt_id = attempt.attempt_id
             JOIN orchestration_v2_projection_runtime_requests AS request ON request.provider_turn_id = provider_turn.provider_turn_id
             WHERE run.run_id = ?1",
            [&receipt.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).unwrap();
        assert_eq!(
            cancelled,
            (
                "cancelled".into(),
                "cancelled".into(),
                "cancelled".into(),
                "cancelled".into(),
                "not_resumable".into(),
            )
        );
    }

    #[tokio::test]
    async fn restart_recovers_an_accepted_turn_and_expires_its_live_requests() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/plan_questions/codex_transcript.ndjson"
        );
        let fixture = Fixture::new();
        let commands = fixture.commands();
        commands.dispatch(command()).unwrap();
        let (settings, _) = runtime_recording(&fixture, RECORDING, "item/tool/requestUserInput");
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands.clone(), settings, changes.clone());
        worker.sender().wake();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pending: i64 = fixture.connection().query_row(
                    "SELECT count(*) FROM orchestration_v2_projection_runtime_requests WHERE status = 'pending'",
                    [],
                    |row| row.get(0),
                ).unwrap();
                if pending == 1 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        worker.task.abort();
        assert!(worker.task.await.unwrap_err().is_cancelled());

        TurnWorker::recover_before_spawn(&commands, &changes).unwrap();
        wait_for_run(&fixture, "cancelled").await;
        let connection = fixture.connection();
        let answer: (String, String, String) = connection.query_row(
            "SELECT request.status, turn.status, session.status
             FROM orchestration_v2_projection_runtime_requests AS request
             JOIN orchestration_v2_projection_provider_turns AS turn ON turn.provider_turn_id = request.provider_turn_id
             JOIN orchestration_v2_projection_provider_sessions AS session ON session.thread_id = turn.thread_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(
            answer,
            ("expired".into(), "cancelled".into(), "stopped".into())
        );
        assert!(!fixture.directory.join("response.json").exists());
    }

    async fn runtime_request_roundtrip(recording: &str, method: &str, kind: &str, response: Value) {
        use crate::server::turn_commands::runtime_requests::RuntimeResponse;

        let fixture = Fixture::new();
        let workspace = fixture.directory.join("work");
        fs::create_dir(&workspace).unwrap();
        fixture
            .connection()
            .execute(
                "UPDATE projection_projects SET workspace_root = ?1",
                [workspace.to_str().unwrap()],
            )
            .unwrap();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let (settings, expected_response) = runtime_recording(&fixture, recording, method);
        let (updates, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands.clone(), settings, updates);
        let sender = worker.sender();
        sender.wake();

        let request_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let request = fixture.connection().query_row(
                    "SELECT runtime_request_id FROM orchestration_v2_projection_runtime_requests
                     WHERE thread_id = ?1 AND status = 'pending' AND kind = ?2",
                    rusqlite::params![THREAD, kind],
                    |row| row.get::<_, String>(0),
                );
                if let Ok(request) = request {
                    break request;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("provider request was not durably published");
        assert!(
            !workspace.join("response.json").exists(),
            "the provider must wait for a committed response"
        );

        let mut payload = json!({
            "type":"runtime-request.respond",
            "commandId":"native-response",
            "threadId":"another-thread",
            "requestId":request_id,
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(response.as_object().unwrap().clone());
        let decode =
            |value: &Value| serde_json::from_value::<RuntimeResponse>(value.clone()).unwrap();
        assert!(commands.respond_runtime_request(decode(&payload)).is_err());
        assert!(!workspace.join("response.json").exists());

        payload["threadId"] = THREAD.into();
        let accepted = commands.respond_runtime_request(decode(&payload)).unwrap();
        assert!(accepted.committed);
        assert_eq!(
            commands
                .respond_runtime_request(decode(&payload))
                .unwrap()
                .sequence,
            accepted.sequence
        );
        sender.wake();
        assert!(wait_for_run(&fixture, "completed").await > accepted.sequence);
        worker.shutdown().await.unwrap();

        let emitted: Value =
            serde_json::from_str(&fs::read_to_string(workspace.join("response.json")).unwrap())
                .unwrap();
        assert_eq!(emitted, expected_response);
        let connection = fixture.connection();
        let (request_status, effect_status): (String, String) = connection.query_row(
            "SELECT request.status, effect.status
             FROM orchestration_v2_projection_runtime_requests AS request
             JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = request.thread_id
             WHERE request.runtime_request_id = ?1 AND effect.effect_type = 'runtime-request.respond'
               AND json_extract(effect.payload_json, '$.requestId') = request.runtime_request_id",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(request_status, "resolved");
        assert_eq!(effect_status, "succeeded");
        let messages: i64 = connection.query_row(
            "SELECT count(*) FROM orchestration_v2_projection_messages WHERE run_id = ?1 AND role = 'assistant'",
            [&receipt.run_id],
            |row| row.get(0),
        ).unwrap();
        if kind == "file-change" {
            let (prompt, changes): (String, String) = connection.query_row(
                "SELECT (SELECT json_extract(payload_json, '$.prompt')
                         FROM orchestration_v2_projection_turn_items WHERE type = 'approval_request'),
                        (SELECT json_extract(payload_json, '$.changes')
                         FROM orchestration_v2_projection_turn_items WHERE type = 'file_change')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            let changes: Value = serde_json::from_str(&changes).unwrap();
            assert_eq!(changes[0]["operation"], "add");
            assert_eq!(
                prompt,
                format!("add {}", changes[0]["path"].as_str().unwrap())
            );
        }
        assert!(messages > 0);
    }

    fn runtime_recording(
        fixture: &Fixture,
        recording: &str,
        request_method: &str,
    ) -> (SettingsFile, Value) {
        let frames: Vec<Value> = recording
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let request_index = frames
            .iter()
            .position(|record| {
                record["type"] == "emit_inbound" && record["label"] == request_method
            })
            .unwrap();
        let expected = frames
            .iter()
            .skip(request_index)
            .find(|record| record["type"] == "expect_outbound" && record["label"] == request_method)
            .unwrap()["frame"]
            .clone();
        let admitted = |record: &Value| {
            let frame = &record["frame"];
            record["type"] == "emit_inbound"
                && match frame["method"].as_str().unwrap_or_default() {
                    "thread/settings/updated"
                    | "turn/started"
                    | "item/agentMessage/delta"
                    | "item/commandExecution/outputDelta"
                    | "thread/tokenUsage/updated"
                    | "serverRequest/resolved"
                    | "turn/completed" => true,
                    "item/started" | "item/completed" => matches!(
                        frame["params"]["item"]["type"].as_str(),
                        Some("agentMessage" | "commandExecution" | "fileChange"),
                    ),
                    method => method == request_method,
                }
        };
        let with_id = |label: &str, id: u64| {
            let mut frame = frames
                .iter()
                .find(|record| {
                    record["type"] == "emit_inbound"
                        && record["label"] == label
                        && record["frame"].get("result").is_some()
                })
                .unwrap()["frame"]
                .clone();
            frame["id"] = id.into();
            frame
        };
        let mut script = "#!/bin/sh
set -eu
"
        .to_owned();
        for stage in [
            vec![with_id("initialize", 1)],
            vec![],
            vec![with_id("thread/start", 2)],
            vec![reply("thread/name/set", 3)],
            vec![with_id("config/read", 4)],
        ] {
            script.push_str(
                "IFS= read -r received
",
            );
            for frame in stage {
                emit_frame(&mut script, &frame);
            }
        }
        script.push_str(
            "IFS= read -r received
",
        );
        emit_frame(&mut script, &with_id("turn/start", 5));
        for record in frames[..=request_index]
            .iter()
            .filter(|record| admitted(record))
        {
            emit_frame(&mut script, &record["frame"]);
        }
        script.push_str("IFS= read -r received\nprintf '%s\\n' \"$received\" > response.json\n");
        for record in frames[request_index + 1..]
            .iter()
            .filter(|record| admitted(record))
        {
            emit_frame(&mut script, &record["frame"]);
        }
        script.push_str(
            "while IFS= read -r received; do :; done
",
        );
        (settings_for_script(fixture, &script), expected)
    }

    fn emit_frame(script: &mut String, frame: &Value) {
        let argument = frame.to_string().replace('\'', "'\"'\"'");
        script.push_str(&format!("printf '%s\\n' '{argument}'\n"));
    }

    #[tokio::test]
    async fn interrupted_native_turn_preserves_status_after_checkpoint_capture() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/turn_interrupt/codex_transcript.ndjson"
        );
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let receipt = commands.dispatch(command()).unwrap();
        let baseline = notification("turn/completed", |_| true);
        let mut terminal = RECORDING
            .lines()
            .find_map(|line| {
                let record: Value = serde_json::from_str(line).unwrap();
                (record["type"] == "emit_inbound" && record["label"] == "turn/completed")
                    .then(|| record["frame"].clone())
            })
            .unwrap();
        assert_eq!(terminal["params"]["turn"]["status"], "interrupted");
        terminal["params"]["threadId"] = baseline["params"]["threadId"].clone();
        terminal["params"]["turn"]["id"] = baseline["params"]["turn"]["id"].clone();
        let settings = settings_with_terminal(&fixture, "", terminal);
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands, settings, changes);
        worker.sender().wake();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let exists: bool = fixture
                    .connection()
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_runs
                     WHERE run_id = ?1 AND status = 'interrupted'
                     AND json_extract(payload_json, '$.checkpointId') IS NOT NULL)",
                        [&receipt.run_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                if exists {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker.shutdown().await.unwrap();
        let outcomes: (String, String, String) = fixture.connection().query_row(
            "SELECT run.status, turn.status, json_extract(turn.payload_json, '$.turnTokenUsage.usageStatus')
             FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_projection_provider_turns AS turn ON turn.thread_id = run.thread_id
             WHERE run.run_id = ?1",
            [&receipt.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(
            outcomes,
            ("interrupted".into(), "interrupted".into(), "partial".into())
        );
    }

    #[tokio::test]
    async fn sends_a_second_prompt_to_the_same_loaded_codex_thread() {
        verify_two_turn_conversation(false).await;
    }

    #[tokio::test]
    async fn continues_the_native_thread_after_a_captured_interruption() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/thread_rollback_to_stopped_turn/codex_transcript.ndjson"
        );
        let frames: Vec<Value> = RECORDING
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut start = 0;
        let phases: Vec<_> = frames
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record["label"] == "turn/completed" && record["type"] == "emit_inbound"
            })
            .take(3)
            .map(|(end, _)| {
                let phase = &frames[start..=end];
                start = end + 1;
                phase
            })
            .collect();
        let mut script = "#!/bin/sh\n".to_owned();
        append_request(&mut script, &[replay_reply(phases[0], "initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[replay_reply(phases[0], "thread/start", 2)]);
        append_request(&mut script, &[reply("thread/name/set", 3)]);
        for (phase, config_id) in phases.iter().zip([4, 6, 8]) {
            append_replayed_turn(&mut script, phase, config_id);
        }
        script.push_str("while IFS= read -r request; do :; done\n");

        let fixture = Fixture::new();
        let commands = fixture.commands();
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes,
        );
        let connection = fixture.connection();
        for (index, (phase, expected)) in phases
            .iter()
            .zip(["completed", "interrupted", "completed"])
            .enumerate()
        {
            let mut payload = command_payload();
            payload["commandId"] = format!("turn-command:{index}").into();
            payload["messageId"] = format!("message:{index}").into();
            payload["text"] = replay_prompt(phase).into();
            let receipt = commands
                .dispatch(serde_json::from_value(payload).unwrap())
                .unwrap();
            worker.sender().wake();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let ready: bool = connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_runs
                         WHERE run_id = ?1 AND status = ?2 AND json_extract(payload_json, '$.checkpointId') IS NOT NULL)",
                        rusqlite::params![receipt.run_id, expected], |row| row.get(0),
                    ).unwrap();
                    if ready { break; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
        }
        worker.shutdown().await.unwrap();

        let requests: Vec<Value> = fs::read_to_string(fixture.directory.join("requests.ndjson"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let turns: Vec<&Value> = requests
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .collect();
        assert_eq!(turns.len(), 3);
        let mut query = connection.prepare(
            "SELECT run.ordinal, turn.ordinal, turn.status, json_extract(provider.payload_json, '$.nativeThreadRef.nativeId')
             FROM orchestration_v2_projection_runs AS run
             JOIN orchestration_v2_projection_provider_turns AS turn ON turn.run_attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
             JOIN orchestration_v2_projection_provider_threads AS provider ON provider.provider_thread_id = turn.provider_thread_id
             WHERE run.thread_id = ?1 ORDER BY run.ordinal",
        ).unwrap();
        let saved: Vec<(i64, i64, String, String)> = query
            .query_map([THREAD], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(saved.len(), 3);
        let command_statuses: Vec<String> = connection
            .prepare(
                "SELECT item.status FROM orchestration_v2_projection_turn_items AS item
             JOIN orchestration_v2_projection_runs AS run ON run.run_id = item.run_id
             WHERE item.thread_id = ?1 AND item.type = 'command_execution' AND run.ordinal = 2",
            )
            .unwrap()
            .query_map([THREAD], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(command_statuses, ["interrupted"]);
        for (index, ((run, turn, status, native), expected)) in saved
            .iter()
            .zip(["completed", "interrupted", "completed"])
            .enumerate()
        {
            assert_eq!(*run, i64::try_from(index + 1).unwrap());
            assert_eq!(run, turn);
            assert_eq!(status, expected);
            assert_eq!(turns[index]["params"]["threadId"], *native);
            assert_eq!(
                turns[index]["params"]["input"][0]["text"],
                replay_prompt(phases[index])
            );
        }
    }

    #[tokio::test]
    async fn stops_an_unbound_start_and_only_sends_the_followup_prompt() {
        verify_followup_after_unbound_stop(true).await;
    }

    #[tokio::test]
    async fn uses_inline_history_when_codex_cannot_inject_items() {
        verify_followup_after_unbound_stop(false).await;
    }

    async fn verify_followup_after_unbound_stop(supports_injection: bool) {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let first = commands.dispatch(command()).unwrap();
        let first_start = format!("effect:{COMMAND}:provider-turn.start:{}", first.run_id);
        let mut script = "#!/bin/sh\nset -eu\n".to_owned();
        append_request(&mut script, &[reply("initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[]);
        script.push_str("while [ ! -f release-unbound-thread ]; do sleep 0.02; done\n");
        let mut discarded = reply("thread/start", 2);
        discarded["result"]["thread"]["id"] = "unbound-native-thread".into();
        emit_frame(&mut script, &discarded);
        append_request(&mut script, &[reply("thread/start", 3)]);
        let injection_reply = if supports_injection {
            json!({"id": 4, "result": {}})
        } else {
            json!({"id": 4, "error": { "code": -32601, "message": "Method not found" }})
        };
        append_request(&mut script, &[injection_reply]);
        append_request(&mut script, &[reply("thread/name/set", 5)]);
        let frames: Vec<Value> = CODEX
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        append_replayed_turn(&mut script, &frames, 6);
        script.push_str("while IFS= read -r request; do :; done\n");
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes,
        );
        let requests_path = fixture.directory.join("requests.ndjson");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if fs::read_to_string(&requests_path)
                    .is_ok_and(|requests| requests.contains("\"method\":\"thread/start\""))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(commands.can_stop_thread(THREAD).unwrap());
        let stop = || {
            serde_json::from_value(json!({
                "type": "thread.stop", "commandId": "stop-unbound", "threadId": THREAD,
            }))
            .unwrap()
        };
        let stopped = commands.stop_thread(stop()).unwrap();
        assert_eq!(stopped.sequence, first.sequence + 6);
        assert!(!commands.stop_thread(stop()).unwrap().committed);
        assert!(!commands.can_stop_thread(THREAD).unwrap());

        let connection = fixture.connection();
        let effect: (String, i64, Option<String>, String) = connection
            .query_row(
                "SELECT status, attempt_count, lease_owner, last_error
             FROM orchestration_v2_effect_outbox WHERE effect_id = ?1",
                [&first_start],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            effect,
            (
                "cancelled".into(),
                1,
                None,
                format!(
                    "Run {} was interrupted before its provider turn started.",
                    first.run_id
                )
            )
        );
        let mut next = command_payload();
        next["commandId"] = "after-early-stop".into();
        next["messageId"] = "message:after-early-stop".into();
        next["text"] = "The first submitted prompt".into();
        commands
            .dispatch(serde_json::from_value(next).unwrap())
            .unwrap();
        worker.sender().wake();
        fs::write(fixture.directory.join("release-unbound-thread"), "").unwrap();
        wait_for_run(&fixture, "completed").await;
        worker.shutdown().await.unwrap();
        let requests: Vec<Value> = fs::read_to_string(requests_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let sent: Vec<_> = requests
            .iter()
            .filter(|frame| frame["method"] == "turn/start")
            .collect();
        assert_eq!(sent.len(), 1);
        let sent_text = sent[0]["params"]["input"][0]["text"].as_str().unwrap();
        if supports_injection {
            assert_eq!(sent_text, "The first submitted prompt");
        } else {
            assert!(sent_text.starts_with("Context handoff (delta_since_target_last_seen):\n"));
            assert!(sent_text.contains("\nRun interrupted before provider start\n"));
            assert!(sent_text.ends_with("\n\nUser message:\nThe first submitted prompt"));
        }
        assert_eq!(
            sent[0]["params"]["threadId"],
            reply("thread/start", 3)["result"]["thread"]["id"]
        );
        let counts: (i64, i64, i64) = connection.query_row(
            "SELECT (SELECT count(*) FROM orchestration_v2_projection_provider_threads),
                    (SELECT count(*) FROM orchestration_v2_projection_provider_turns),
                    (SELECT count(*) FROM orchestration_v2_effect_outbox WHERE effect_type = 'provider-turn.interrupt')",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(counts, (1, 1, 0));
        let injected = requests
            .iter()
            .find(|frame| frame["method"] == "thread/inject_items")
            .unwrap();
        let items = injected["params"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["role"], "user");
        assert_eq!(items[2]["role"], "assistant");
        assert!(
            items[2]["content"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with("\nRun interrupted before provider start")
        );
        let history_delivery: Vec<String> = connection
            .prepare(
                "SELECT json_extract(payload_json, '$.delivery.status') FROM orchestration_events
             WHERE event_type = 'context-handoff.updated' ORDER BY sequence",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            history_delivery,
            [
                "pending",
                if supports_injection {
                    "injected"
                } else {
                    "inline"
                }
            ]
        );
    }

    #[tokio::test]
    async fn sends_a_committed_stop_and_persists_the_captured_codex_terminal() {
        replay_stop(false).await;
    }

    #[tokio::test]
    async fn updates_the_captured_todo_list_and_keeps_its_step_timing() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/todo_list/codex_transcript.ndjson"
        );
        let frames = RECORDING
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let mut script = "#!/bin/sh\nset -eu\n".to_owned();
        for stage in [
            vec![replay_reply(&frames, "initialize", 1)],
            vec![],
            vec![replay_reply(&frames, "thread/start", 2)],
            vec![replay_reply(&frames, "thread/name/set", 3)],
            vec![replay_reply(&frames, "config/read", 4)],
            vec![replay_reply(&frames, "turn/start", 5)],
        ] {
            append_request(&mut script, &stage);
        }
        let mut updates = 0;
        for row in frames.iter().skip_while(|row| row["label"] != "turn/start") {
            if row["type"] != "emit_inbound" || row["frame"].get("id").is_some() {
                continue;
            }
            if row["label"] == "turn/plan/updated" {
                updates += 1;
                if updates == 2 {
                    script.push_str("while [ ! -f release-plan-update ]; do sleep 0.02; done\n");
                }
            }
            emit_frame(&mut script, &row["frame"]);
        }
        script.push_str("while IFS= read -r request; do :; done\n");

        let fixture = Fixture::new();
        let commands = fixture.commands();
        let mut dispatch = command_payload();
        dispatch["text"] = replay_prompt(&frames).into();
        commands
            .dispatch(serde_json::from_value(dispatch).unwrap())
            .unwrap();
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands, settings_for_script(&fixture, &script), changes);
        let connection = fixture.connection();
        wait_for_item(&connection, "todo_list", "completed").await;
        let plan = || -> Value {
            let payload: String = connection.query_row(
                "SELECT payload_json FROM orchestration_v2_projection_plans WHERE thread_id = ?1 AND kind = 'todo_list'",
                [THREAD], |row| row.get(0),
            ).unwrap();
            serde_json::from_str(&payload).unwrap()
        };
        let initial = plan();
        assert_eq!(initial["steps"][0]["status"], "running");
        assert_eq!(initial["steps"][1]["status"], "pending");
        assert!(initial["steps"][0]["durationAnchorAt"].is_string());
        fs::write(fixture.directory.join("release-plan-update"), "").unwrap();
        wait_for_run(&fixture, "completed").await;
        worker.shutdown().await.unwrap();
        let final_plan = plan();
        assert_eq!(final_plan["id"], initial["id"]);
        assert_eq!(final_plan["status"], "active");
        assert!(final_plan["steps"][0]["durationMs"].as_u64().unwrap() > 0);
        for step in final_plan["steps"].as_array().unwrap() {
            assert_eq!(step["status"], "completed");
            assert!(step.get("durationAnchorAt").is_none());
        }
        let rows = {
            let mut select = connection.prepare(
                "SELECT event_type, payload_json FROM orchestration_events WHERE stream_id = ?1
                 AND (json_extract(payload_json, '$.type') = 'todo_list' OR json_extract(payload_json, '$.kind') = 'todo_list')
                 ORDER BY sequence",
            ).unwrap();
            select
                .query_map([THREAD], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .map(|row| {
                    let (kind, payload) = row.unwrap();
                    (kind, serde_json::from_str::<Value>(&payload).unwrap())
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            [
                "node.updated",
                "plan.updated",
                "turn-item.updated",
                "node.updated",
                "plan.updated",
                "turn-item.updated",
            ]
        );
        assert_eq!(rows[2].1["id"], rows[5].1["id"]);
        assert_eq!(rows[2].1["ordinal"], rows[5].1["ordinal"]);
        assert_eq!(rows[5].1["planId"], final_plan["id"]);
        assert_eq!(rows[5].1["status"], "completed");
        assert!(rows[5].1["steps"][0].get("durationMs").is_none());
    }

    #[tokio::test]
    async fn records_completed_web_search_patterns_without_raw_results() {
        replay_web_search(false).await;
    }

    #[tokio::test]
    async fn stops_a_web_search_and_ignores_its_late_result_during_the_next_turn() {
        replay_web_search(true).await;
    }

    async fn replay_web_search(stop_search: bool) {
        const SEARCH: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/web_search/codex_transcript.ndjson"
        );
        const STOP: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/turn_interrupt/codex_transcript.ndjson"
        );
        let frames = |source: &str| {
            source
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        };
        let search = frames(SEARCH);
        let stop = frames(STOP);
        let (search_start, started) = search
            .iter()
            .enumerate()
            .find(|(_, row)| {
                row["type"] == "emit_inbound"
                    && row["label"] == "item/started"
                    && row["frame"]["params"]["item"]["type"] == "webSearch"
            })
            .unwrap();
        let completed = search
            .iter()
            .find(|row| {
                row["type"] == "emit_inbound"
                    && row["label"] == "item/completed"
                    && row["frame"]["params"]["item"]["type"] == "webSearch"
            })
            .unwrap();
        let search_params = &started["frame"]["params"];
        let mut script = "#!/bin/sh\nset -eu\n".to_owned();
        for stage in [
            vec![replay_reply(&search, "initialize", 1)],
            vec![],
            vec![replay_reply(&search, "thread/start", 2)],
            vec![replay_reply(&search, "thread/name/set", 3)],
            vec![replay_reply(&search, "config/read", 4)],
        ] {
            append_request(&mut script, &stage);
        }
        let end = if stop_search {
            search_start + 1
        } else {
            search.len()
        };
        let notifications = search
            .iter()
            .take(end)
            .skip_while(|row| row["label"] != "turn/start")
            .filter(|row| row["type"] == "emit_inbound" && row["frame"].get("id").is_none())
            .map(|row| row["frame"].clone());
        let turn = std::iter::once(replay_reply(&search, "turn/start", 5))
            .chain(notifications)
            .collect::<Vec<_>>();
        append_request(&mut script, &turn);
        if stop_search {
            let mut terminal = stop
                .iter()
                .find(|row| row["label"] == "turn/completed")
                .unwrap()["frame"]
                .clone();
            terminal["params"]["threadId"] = search_params["threadId"].clone();
            terminal["params"]["turn"]["id"] = search_params["turnId"].clone();
            append_request(
                &mut script,
                &[replay_reply(&stop, "turn/interrupt", 6), terminal],
            );
            append_request(
                &mut script,
                &[completed["frame"].clone(), reply("config/read", 7)],
            );
            let after = frames(CODEX);
            let final_turn = std::iter::once(reply("turn/start", 8))
                .chain(
                    after
                        .iter()
                        .filter(|row| {
                            row["type"] == "emit_inbound"
                                && row["frame"]["params"].get("threadId").is_some()
                        })
                        .map(|row| {
                            let mut frame = row["frame"].clone();
                            frame["params"]["threadId"] = search_params["threadId"].clone();
                            frame
                        }),
                )
                .collect::<Vec<_>>();
            append_request(&mut script, &final_turn);
        }
        script.push_str("while IFS= read -r request; do :; done\n");

        let fixture = Fixture::new();
        let commands = fixture.commands();
        let mut dispatch = command_payload();
        dispatch["text"] = replay_prompt(&search).into();
        commands
            .dispatch(serde_json::from_value(dispatch).unwrap())
            .unwrap();
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes,
        );
        let connection = fixture.connection();
        if stop_search {
            wait_for_item(&connection, "web_search", "running").await;
            assert!(commands.can_stop_thread(THREAD).unwrap());
            commands
                .stop_thread(
                    serde_json::from_value(json!({
                        "type": "thread.stop", "commandId": "stop-web-search", "threadId": THREAD,
                    }))
                    .unwrap(),
                )
                .unwrap();
            worker.sender().wake();
            wait_for_item(&connection, "checkpoint", "completed").await;
            let mut next = command_payload();
            next["commandId"] = "after-web-search".into();
            next["messageId"] = "message:after-web-search".into();
            commands
                .dispatch(serde_json::from_value(next).unwrap())
                .unwrap();
            worker.sender().wake();
        }
        wait_for_run(&fixture, "completed").await;
        worker.shutdown().await.unwrap();
        let items: Vec<Value> = connection.prepare(
            "SELECT payload_json FROM orchestration_events WHERE stream_id = ?1 AND event_type = 'turn-item.updated'
               AND json_extract(payload_json, '$.type') = 'web_search' ORDER BY sequence",
        ).unwrap().query_map([THREAD], |row| row.get::<_, String>(0)).unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap()).collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["status"], "running");
        assert_eq!(
            items[1]["status"],
            if stop_search {
                "interrupted"
            } else {
                "completed"
            }
        );
        for item in &items {
            assert_eq!(
                item["nativeItemRef"]["nativeId"],
                search_params["item"]["id"]
            );
            assert_eq!(item["startedAt"], items[0]["startedAt"]);
            assert!(item.get("results").is_none());
        }
        assert!(items[0].get("patterns").is_none());
        if stop_search {
            assert!(items[1].get("patterns").is_none());
        } else {
            assert_eq!(
                items[1]["patterns"],
                json!([completed["frame"]["params"]["item"]["query"]])
            );
        }
        let wire = fs::read_to_string(fixture.directory.join("requests.ndjson")).unwrap();
        assert_eq!(
            wire.matches("turn/interrupt").count(),
            usize::from(stop_search)
        );
        assert!(!wire.contains("backgroundTerminals/terminate"));
    }

    #[tokio::test]
    async fn interrupts_a_running_command_and_terminates_its_captured_background_terminal() {
        replay_running_command_stop(false).await;
    }

    #[tokio::test]
    async fn retries_unconfirmed_terminal_containment_after_the_turn_settles() {
        replay_running_command_stop(true).await;
    }

    async fn replay_running_command_stop(retry_containment: bool) {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/turn_interrupt_mid_tool/codex_transcript.ndjson"
        );
        let frames: Vec<Value> = RECORDING
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let recorded = |label: &str| {
            frames
                .iter()
                .find(|row| row["type"] == "emit_inbound" && row["label"] == label)
                .unwrap()["frame"]
                .clone()
        };
        let expected = frames
            .iter()
            .find(|row| {
                row["type"] == "expect_outbound"
                    && row["label"] == "thread/backgroundTerminals/terminate"
            })
            .unwrap();
        let mut script = "#!/bin/sh\nset -eu\n".to_owned();
        append_request(&mut script, &[replay_reply(&frames, "initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[replay_reply(&frames, "thread/start", 2)]);
        append_request(&mut script, &[replay_reply(&frames, "thread/name/set", 3)]);
        append_request(&mut script, &[replay_reply(&frames, "config/read", 4)]);
        append_request(
            &mut script,
            &[
                replay_reply(&frames, "turn/start", 5),
                recorded("turn/started"),
                recorded("item/reasoning/summaryTextDelta"),
            ],
        );
        script.push_str("while [ ! -f allow-command-start ]; do sleep 0.02; done\n");
        for kind in ["reasoning", "commandExecution"] {
            let label = if kind == "reasoning" {
                "item/completed"
            } else {
                "item/started"
            };
            let frame = frames
                .iter()
                .find(|row| {
                    row["type"] == "emit_inbound"
                        && row["label"] == label
                        && row["frame"]["params"]["item"]["type"] == kind
                })
                .unwrap();
            emit_frame(&mut script, &frame["frame"]);
        }
        append_request(
            &mut script,
            &[
                recorded("thread/tokenUsage/updated"),
                replay_reply(&frames, "turn/interrupt", 6),
            ],
        );
        let terminate = "thread/backgroundTerminals/terminate";
        if retry_containment {
            let process_id = &expected["frame"]["params"]["processId"];
            let still_active = |id| json!({"id": id, "result": {"data": [{"processId": process_id}], "nextCursor": null}});
            append_request(
                &mut script,
                &[
                    recorded("turn/completed"),
                    json!({"id": 7, "result": {"terminated": false}}),
                ],
            );
            append_request(&mut script, &[still_active(8)]);
            append_request(
                &mut script,
                &[json!({"id": 9, "result": {"terminated": false}})],
            );
            append_request(&mut script, &[still_active(10)]);
            append_request(&mut script, &[replay_reply(&frames, terminate, 11)]);
        } else {
            append_request(
                &mut script,
                &[
                    recorded("turn/completed"),
                    replay_reply(&frames, terminate, 7),
                ],
            );
        }
        script.push_str("while IFS= read -r request; do :; done\n");

        let fixture = Fixture::new();
        let commands = fixture.commands();
        let connection = fixture.connection();
        let mut dispatch = command_payload();
        dispatch["text"] = replay_prompt(&frames).into();
        commands
            .dispatch(serde_json::from_value(dispatch).unwrap())
            .unwrap();
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes,
        );
        wait_for_item(&connection, "reasoning", "running").await;
        fs::write(fixture.directory.join("allow-command-start"), "").unwrap();
        wait_for_item(&connection, "command_execution", "running").await;
        assert!(commands.can_stop_thread(THREAD).unwrap());
        commands
            .stop_thread(
                serde_json::from_value(json!({
                    "type": "thread.stop", "commandId": "stop-running-terminal", "threadId": THREAD,
                }))
                .unwrap(),
            )
            .unwrap();
        worker.sender().wake();
        wait_for_run(&fixture, "interrupted").await;
        wait_for_item(&connection, "checkpoint", "completed").await;
        assert!(!commands.can_stop_thread(THREAD).unwrap());
        worker.shutdown().await.unwrap();

        let requests: Vec<Value> = fs::read_to_string(fixture.directory.join("requests.ndjson"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let controls: Vec<_> = requests
            .iter()
            .filter(|frame| frame["method"] == "turn/interrupt" || frame["method"] == terminate)
            .map(|frame| frame["method"].as_str().unwrap())
            .collect();
        assert_eq!(controls[0], "turn/interrupt");
        assert_eq!(controls.len(), if retry_containment { 4 } else { 2 });
        assert!(controls[1..].iter().all(|method| *method == terminate));
        assert_eq!(
            requests.last().unwrap()["params"],
            expected["frame"]["params"]
        );
        let timeline: Vec<(String, String)> = connection.prepare(
            "SELECT type, status FROM orchestration_v2_projection_turn_items WHERE thread_id = ?1 ORDER BY ordinal",
        ).unwrap().query_map([THREAD], |row| Ok((row.get(0)?, row.get(1)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(
            timeline
                .iter()
                .map(|(kind, status)| (kind.as_str(), status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("user_message", "completed"),
                ("reasoning", "completed"),
                ("command_execution", "interrupted"),
                ("run_interrupt_request", "completed"),
                ("run_interrupt_result", "interrupted"),
                ("checkpoint", "completed"),
            ]
        );
        let row: (String, String, u32) = connection.query_row(
            "SELECT effect.status, json_extract(reasoning.payload_json, '$.text'), effect.attempt_count
             FROM orchestration_v2_effect_outbox AS effect
             JOIN orchestration_v2_projection_turn_items AS reasoning ON reasoning.thread_id = effect.thread_id
             WHERE effect.command_id = 'stop-running-terminal' AND reasoning.type = 'reasoning'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(
            row,
            (
                "succeeded".into(),
                "**Running exact command**".into(),
                if retry_containment { 2 } else { 1 }
            )
        );
    }

    async fn wait_for_item(connection: &rusqlite::Connection, kind: &str, status: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let found: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_turn_items
                                    WHERE thread_id = ?1 AND type = ?2 AND status = ?3)",
                        rusqlite::params![THREAD, kind, status],
                        |row| row.get(0),
                    )
                    .unwrap();
                if found {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_acknowledged_stop_stays_running_until_late_terminal_confirmation() {
        replay_stop(true).await;
    }

    #[tokio::test]
    async fn stopping_a_pending_native_question_cancels_its_projection_without_an_answer() {
        let recording = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/plan_questions/codex_transcript.ndjson"
        );
        replay_stop_with_request(recording, "item/tool/requestUserInput", "user_input", false)
            .await;
    }

    #[tokio::test]
    async fn stopping_a_pending_native_approval_releases_it_with_the_session_without_a_decision() {
        let recording = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_read_only_on_request/codex_transcript.ndjson"
        );
        replay_stop_with_request(
            recording,
            "item/commandExecution/requestApproval",
            "command",
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn restart_expires_the_pending_approval_from_a_stopped_turn() {
        let recording = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_read_only_on_request/codex_transcript.ndjson"
        );
        replay_stop_with_request(
            recording,
            "item/commandExecution/requestApproval",
            "command",
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn stops_a_pending_native_file_change_without_answering_the_approval() {
        let recording = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/tool_call_restricted_granular/codex_transcript.ndjson"
        );
        replay_stop_with_request(
            recording,
            "item/fileChange/requestApproval",
            "file-change",
            false,
        )
        .await;
    }

    async fn replay_stop_with_request(
        recording: &str,
        method: &str,
        request_kind: &str,
        restart: bool,
    ) {
        use crate::server::turn_commands::interrupt_effect::ThreadStop;

        const STOP: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/turn_interrupt/codex_transcript.ndjson"
        );
        let approval = request_kind != "user_input";
        let file_approval = request_kind == "file-change";
        let recording: Vec<Value> = recording
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let stop: Vec<Value> = STOP
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let (request_index, request) = recording
            .iter()
            .enumerate()
            .find_map(|(index, record)| {
                (record["type"] == "emit_inbound" && record["label"] == method)
                    .then_some((index, &record["frame"]))
            })
            .unwrap();
        let mut terminal = stop
            .iter()
            .find(|record| record["label"] == "turn/completed")
            .unwrap()["frame"]
            .clone();
        terminal["params"]["threadId"] = request["params"]["threadId"].clone();
        terminal["params"]["turn"]["id"] = request["params"]["turnId"].clone();

        let mut script = "#!/bin/sh\n".to_owned();
        append_request(&mut script, &[replay_reply(&recording, "initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[replay_reply(&recording, "thread/start", 2)]);
        append_request(&mut script, &[reply("thread/name/set", 3)]);
        append_request(&mut script, &[replay_reply(&recording, "config/read", 4)]);
        let mut turn = vec![replay_reply(&recording, "turn/start", 5)];
        turn.extend(
            recording[..=request_index]
                .iter()
                .filter(|record| {
                    record["type"] == "emit_inbound"
                        && (record["label"] == method
                            || (file_approval && record["frame"].get("id").is_none())
                            || matches!(
                                record["label"].as_str(),
                                Some(
                                    "turn/started"
                                        | "thread/settings/updated"
                                        | "thread/tokenUsage/updated"
                                )
                            )
                            || (record["label"] == "item/started"
                                && record["frame"]["params"]["item"]["type"] == "commandExecution"))
                })
                .map(|record| record["frame"].clone()),
        );
        append_request(&mut script, &turn);
        let mut reply = vec![replay_reply(&stop, "turn/interrupt", 6)];
        let resolved = recording
            .iter()
            .find(|record| record["label"] == "serverRequest/resolved")
            .map(|resolved| resolved["frame"].clone());
        if file_approval {
            reply.extend(std::iter::once(terminal).chain(resolved));
        } else {
            reply.extend(resolved.into_iter().chain([terminal]));
        }
        append_request(&mut script, &reply);
        script.push_str("while IFS= read -r request; do :; done\n");

        let fixture = Fixture::new();
        let workspace = fixture.directory.join("repository");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join(".gitignore"), "requests.ndjson\n").unwrap();
        for arguments in [
            ["init", "-q"].as_slice(),
            ["add", ".gitignore"].as_slice(),
            [
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "Baseline",
            ]
            .as_slice(),
        ] {
            let result = std::process::Command::new("git")
                .current_dir(&workspace)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let connection = fixture.connection();
        connection
            .execute(
                "UPDATE projection_projects SET workspace_root = ?1",
                [workspace.to_str().unwrap()],
            )
            .unwrap();
        if !approval {
            connection.execute(
                "UPDATE orchestration_v2_projection_threads SET interaction_mode = 'plan',
                 payload_json = json_set(payload_json, '$.interactionMode', 'plan') WHERE thread_id = ?1",
                [THREAD],
            ).unwrap();
        }
        let commands = fixture.commands();
        let mut user = command_payload();
        user["text"] = replay_prompt(&recording).into();
        let run = commands
            .dispatch(serde_json::from_value(user).unwrap())
            .unwrap();
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes.clone(),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pending: bool = connection
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_runtime_requests
                     WHERE thread_id = ?1 AND status = 'pending' AND kind = ?2)",
                        rusqlite::params![THREAD, request_kind],
                        |row| row.get(0),
                    )
                    .unwrap();
                if pending {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(commands.can_stop_thread(THREAD).unwrap());
        let accepted = commands
            .stop_thread(
                serde_json::from_value::<ThreadStop>(json!({
                    "type":"thread.stop", "commandId":"stop-interaction", "threadId":THREAD,
                }))
                .unwrap(),
            )
            .unwrap();
        let status: String = connection.query_row(
            "SELECT status FROM orchestration_v2_projection_runtime_requests WHERE thread_id = ?1",
            [THREAD], |row| row.get(0),
        ).unwrap();
        assert_eq!(status, "pending");
        worker.sender().wake();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let done: bool = connection.query_row(
                    "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_runs
                     WHERE run_id = ?1 AND status = 'interrupted' AND json_extract(payload_json,'$.checkpointId') IS NOT NULL)",
                    [&run.run_id], |row| row.get(0),
                ).unwrap();
                if done { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let before_release: (String, String, String) = connection.query_row(
            "SELECT request.status, json_extract(request.payload_json, '$.responseCapability.type'),
                    card.status
             FROM orchestration_v2_projection_runtime_requests AS request
             JOIN orchestration_v2_projection_turn_items AS card ON card.node_id = request.node_id
             WHERE request.thread_id = ?1",
            [THREAD], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(
            before_release,
            if approval {
                ("pending".into(), "live".into(), "waiting".into())
            } else {
                ("cancelled".into(), "live".into(), "cancelled".into())
            }
        );
        if restart {
            worker.task.abort();
            assert!(worker.task.await.unwrap_err().is_cancelled());
            TurnWorker::recover_before_spawn(&commands, &changes).unwrap();
            TurnWorker::recover_before_spawn(&commands, &changes).unwrap();
        } else {
            worker.shutdown().await.unwrap();
        }

        let wire: Vec<Value> = fs::read_to_string(workspace.join("requests.ndjson"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(wire.iter().all(|frame| frame.get("method").is_some()));
        assert_eq!(wire.last().unwrap()["method"], "turn/interrupt");
        assert_eq!(
            wire.last().unwrap()["params"],
            json!({
                "threadId":request["params"]["threadId"], "turnId":request["params"]["turnId"],
            })
        );
        let status: (String, String, String, String, String) = connection.query_row(
            "SELECT request.status, json_extract(request.payload_json,'$.responseCapability.type'),
                    node.status, card.status, effect.status
             FROM orchestration_v2_projection_runtime_requests AS request
             JOIN orchestration_v2_projection_nodes AS node ON node.node_id = request.node_id
             JOIN orchestration_v2_projection_turn_items AS card ON card.node_id = node.node_id
             JOIN orchestration_v2_effect_outbox AS effect ON effect.thread_id = request.thread_id
             WHERE request.thread_id = ?1 AND card.type = ?2 AND effect.command_id = 'stop-interaction'",
            rusqlite::params![THREAD, if approval {"approval_request"} else {"user_input_request"}],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        ).unwrap();
        assert_eq!(
            status,
            (
                (if restart { "expired" } else { "cancelled" }).into(),
                (if approval { "not_resumable" } else { "live" }).into(),
                (if restart { "waiting" } else { "cancelled" }).into(),
                (if restart { "waiting" } else { "cancelled" }).into(),
                "succeeded".into()
            )
        );
        let expected = if file_approval {
            [
                "provider-turn.updated",
                "run-attempt.updated",
                "turn-item.updated",
                "run.updated",
            ]
        } else if approval {
            [
                "provider-turn.updated",
                "node.updated",
                "turn-item.updated",
                "run-attempt.updated",
            ]
        } else {
            [
                "runtime-request.updated",
                "node.updated",
                "turn-item.updated",
                "provider-turn.updated",
            ]
        };
        let recent: Vec<String> = connection
            .prepare(
                "SELECT event_type FROM orchestration_events WHERE sequence > ?1 AND stream_id = ?2
             ORDER BY sequence LIMIT 4",
            )
            .unwrap()
            .query_map(rusqlite::params![accepted.sequence, THREAD], |row| {
                row.get(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(recent, expected);
        if restart {
            let recovery: Vec<String> = connection.prepare(
                "SELECT event.event_type FROM orchestration_events AS event
                 JOIN orchestration_command_receipts AS receipt ON receipt.command_id = event.command_id
                 WHERE receipt.command_type = 'provider-runtime.reconcile' AND event.stream_id = ?1
                 ORDER BY event.sequence",
            ).unwrap().query_map([THREAD], |row| row.get(0)).unwrap()
                .collect::<rusqlite::Result<_>>().unwrap();
            assert_eq!(
                recovery,
                ["runtime-request.updated", "provider-session.updated"]
            );
        }
    }

    async fn replay_stop(delay_terminal: bool) {
        use crate::server::turn_commands::interrupt_effect::ThreadStop;

        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/turn_interrupt/codex_transcript.ndjson"
        );
        let recording: Vec<Value> = RECORDING
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let mut payload = command_payload();
        payload["text"] = replay_prompt(&recording).into();
        let intent = commands
            .dispatch(serde_json::from_value(payload).unwrap())
            .unwrap();
        let mut script = "#!/bin/sh\n".to_owned();
        append_request(&mut script, &[replay_reply(&recording, "initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[replay_reply(&recording, "thread/start", 2)]);
        append_request(&mut script, &[reply("thread/name/set", 3)]);
        append_request(&mut script, &[replay_reply(&recording, "config/read", 4)]);
        let notification = |name: &str| {
            recording
                .iter()
                .find(|record| {
                    record["type"] == "emit_inbound" && record["frame"]["method"] == name
                })
                .unwrap()["frame"]
                .clone()
        };
        append_request(
            &mut script,
            &[
                replay_reply(&recording, "turn/start", 5),
                notification("turn/started"),
            ],
        );
        append_request(
            &mut script,
            &[replay_reply(&recording, "turn/interrupt", 6)],
        );
        if delay_terminal {
            script.push_str("while [ ! -f allow-stop-terminal ]; do sleep 0.02; done\n");
        }
        emit_frame(&mut script, &notification("turn/completed"));
        script.push_str("while IFS= read -r request; do :; done\n");
        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(
            commands.clone(),
            settings_for_script(&fixture, &script),
            changes,
        );
        let connection = fixture.connection();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let running: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_provider_turns
                        WHERE thread_id = ?1 AND status = 'running')",
                        [THREAD],
                        |row| row.get(0),
                    )
                    .unwrap();
                if running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let wire_log = fixture.directory.join("requests.ndjson");
        assert!(
            !fs::read_to_string(&wire_log)
                .unwrap()
                .contains("turn/interrupt")
        );
        let stop = serde_json::from_value::<ThreadStop>(json!({
            "type":"thread.stop", "commandId":"stop-command", "threadId":THREAD,
        }))
        .unwrap();
        let request = commands.stop_thread(stop).unwrap();
        assert!(request.committed);
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM orchestration_v2_projection_runs WHERE run_id = ?1",
                    [&intent.run_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "running"
        );
        worker.sender().wake();
        let before_terminal = if delay_terminal {
            let unresponsive = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let row = connection.query_row(
                        "SELECT receipt.result_sequence, run.status, effect.status
                         FROM orchestration_command_receipts AS receipt
                         JOIN orchestration_v2_effect_outbox AS effect
                           ON receipt.command_id = effect.effect_id || ':unresponsive'
                         JOIN orchestration_v2_projection_runs AS run ON run.run_id = ?1
                         WHERE effect.command_id = 'stop-command'",
                        [&intent.run_id],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    );
                    if let Ok(row) = row {
                        return row;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                unresponsive,
                (request.sequence + 1, "running".into(), "succeeded".into())
            );
            fs::write(fixture.directory.join("allow-stop-terminal"), "").unwrap();
            unresponsive.0
        } else {
            request.sequence
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_runs
                        WHERE run_id = ?1 AND status = 'interrupted'
                          AND json_extract(payload_json, '$.checkpointId') IS NOT NULL)",
                        [&intent.run_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker.shutdown().await.unwrap();
        let parsed: Vec<Value> = fs::read_to_string(&wire_log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let interrupts: Vec<_> = parsed
            .iter()
            .filter(|frame| frame["method"] == "turn/interrupt")
            .collect();
        assert_eq!(interrupts.len(), 1);
        let original_request = recording
            .iter()
            .find(|record| {
                record["type"] == "expect_outbound" && record["label"] == "turn/interrupt"
            })
            .unwrap();
        assert_eq!(interrupts[0]["params"], original_request["frame"]["params"]);

        let mut recent = connection
            .prepare(
                "SELECT event_type FROM orchestration_events WHERE sequence > ?1 AND stream_id = ?2
             ORDER BY sequence LIMIT 6",
            )
            .unwrap();
        let order: Vec<String> = recent
            .query_map(rusqlite::params![before_terminal, THREAD], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            order,
            [
                "provider-turn.updated",
                "run-attempt.updated",
                "turn-item.updated",
                "run.updated",
                "node.updated",
                "provider-thread.updated"
            ]
        );
        let (ordinal, reason, parent, stop_status, followed_up): (i64, String, String, String, i64) = connection.query_row(
            "SELECT result.ordinal, json_extract(result.payload_json, '$.message'), json_extract(result.payload_json, '$.parentItemId'),
                    effect.status, followup.result_sequence
             FROM orchestration_v2_projection_turn_items AS result
             JOIN orchestration_v2_effect_outbox AS effect ON effect.command_id = 'stop-command'
             JOIN orchestration_command_receipts AS followup ON followup.command_id = effect.effect_id || ?2
             WHERE result.run_id = ?1 AND result.type = 'run_interrupt_result'",
            rusqlite::params![intent.run_id, if delay_terminal { ":unresponsive" } else { ":background-work-settled" }],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        ).unwrap();
        assert_eq!(ordinal, 1_000_003);
        assert_eq!(reason, "Stopped by user");
        assert!(parent.ends_with(":signal:interrupt-request"));
        assert_eq!(stop_status, "succeeded");
        assert_eq!(
            followed_up,
            request.sequence + if delay_terminal { 1 } else { 6 }
        );
    }

    #[tokio::test]
    async fn streams_captured_command_output_then_uses_the_persisted_final_result() {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/command_output_streaming/codex_transcript.ndjson"
        );
        let frames: Vec<Value> = RECORDING
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let mut payload = command_payload();
        payload["text"] = replay_prompt(&frames).into();
        commands
            .dispatch(serde_json::from_value(payload).unwrap())
            .unwrap();

        let mut script = "#!/bin/sh\n".to_owned();
        append_request(&mut script, &[replay_reply(&frames, "initialize", 1)]);
        append_request(&mut script, &[]);
        append_request(&mut script, &[replay_reply(&frames, "thread/start", 2)]);
        append_request(&mut script, &[reply("thread/name/set", 3)]);
        append_request(&mut script, &[replay_reply(&frames, "config/read", 4)]);
        append_request(&mut script, &[replay_reply(&frames, "turn/start", 5)]);
        let mut started_command = None;
        let mut first_delta = None;
        for record in &frames {
            if record["type"] != "emit_inbound" {
                continue;
            }
            let frame = &record["frame"];
            match frame["method"].as_str().unwrap_or_default() {
                "turn/started" | "turn/completed" | "thread/tokenUsage/updated" => {
                    emit_frame(&mut script, frame);
                }
                "item/started" | "item/completed"
                    if frame["params"]["item"]["type"] == "commandExecution" =>
                {
                    emit_frame(&mut script, frame);
                    if frame["method"] == "item/started" {
                        started_command = frame["params"]["item"]["id"].as_str();
                        script.push_str("while [ ! -f allow-first-output ]; do sleep 0.02; done\n");
                    }
                }
                "item/commandExecution/outputDelta" => {
                    emit_frame(&mut script, frame);
                    if first_delta.is_none() {
                        first_delta = frame["params"]["delta"].as_str();
                        script.push_str("while [ ! -f allow-final-output ]; do sleep 0.02; done\n");
                    }
                }
                _ => {}
            }
        }
        script.push_str("while IFS= read -r request; do :; done\n");

        let (changes, _) = broadcast::channel(32);
        let worker = TurnWorker::spawn(commands, settings_for_script(&fixture, &script), changes);
        let queue = worker.sender();
        let command = command_item_id(started_command.unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let running: bool = fixture
                    .connection()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_turn_items
                       WHERE turn_item_id = ?1 AND thread_id = ?2 AND status = 'running')",
                        rusqlite::params![command, THREAD],
                        |row| row.get(0),
                    )
                    .unwrap();
                if running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut output = queue.subscribe_command_output(THREAD, &command).unwrap();
        let initial = serde_json::to_value(output.recv().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            initial,
            json!({"kind":"replace", "text":"", "truncated":false, "running":true})
        );
        fs::write(fixture.directory.join("allow-first-output"), "").unwrap();
        let append = tokio::time::timeout(Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let append = serde_json::to_value(append).unwrap();
        assert_eq!(
            append,
            json!({"kind":"append", "text":first_delta.unwrap(), "truncated":false, "running":true})
        );
        fs::write(fixture.directory.join("allow-final-output"), "").unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(frame) = output.recv().await.unwrap() {
                let frame = serde_json::to_value(frame).unwrap();
                if frame["running"] == false {
                    return frame;
                }
            }
            panic!("settled stdout must be emitted before the stream ends");
        })
        .await
        .unwrap();
        assert_eq!(
            terminal,
            json!({
                "kind":"replace",
                "text":"\u{1b}[0;32mstream line 2\u{1b}[0m\n\u{1b}[0;32mstream line 3\u{1b}[0m\n\u{1b}[0;32mstream line 4\u{1b}[0m\nprogress 100%\n",
                "truncated": false,
                "running": false,
            })
        );
        assert!(output.recv().await.unwrap().is_none());
        wait_for_run(&fixture, "completed").await;
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resumes_the_same_saved_codex_thread_after_releasing_the_process() {
        verify_two_turn_conversation(true).await;
    }

    async fn verify_two_turn_conversation(restart: bool) {
        let fixture = Fixture::new();
        let commands = fixture.commands();
        let (first_phase, second_phase) = resume_replay_frames();
        let first_text = replay_prompt(&first_phase);
        let second_text = replay_prompt(&second_phase);
        let mut first_payload = command_payload();
        first_payload["text"] = json!(first_text);
        let first = commands
            .dispatch(serde_json::from_value(first_payload).unwrap())
            .unwrap();
        let mut first_script = "#!/bin/sh".to_owned();
        first_script.push(char::from(10));
        append_request(
            &mut first_script,
            &[replay_reply(&first_phase, "initialize", 1)],
        );
        append_request(&mut first_script, &[]);
        append_request(
            &mut first_script,
            &[replay_reply(&first_phase, "thread/start", 2)],
        );
        append_request(&mut first_script, &[reply("thread/name/set", 3)]);
        append_replayed_turn(&mut first_script, &first_phase, 4);
        if !restart {
            append_replayed_turn(&mut first_script, &second_phase, 6);
        }
        first_script.push_str("while IFS= read -r request; do :; done");
        first_script.push(char::from(10));
        let (changes, _) = broadcast::channel(32);
        let settings = settings_for_script(&fixture, &first_script);
        let mut worker = TurnWorker::spawn(commands.clone(), settings, changes.clone());
        worker.sender().wake();
        wait_for_run(&fixture, "completed").await;
        let connection = fixture.connection();
        let native_id: String = connection
            .query_row(
                "SELECT json_extract(payload_json, '$.nativeThreadRef.nativeId')
             FROM orchestration_v2_projection_provider_threads WHERE thread_id = ?1",
                [THREAD],
                |row| row.get(0),
            )
            .unwrap();

        if restart {
            worker.shutdown().await.unwrap();
            TurnWorker::recover_before_spawn(&commands, &changes).unwrap();
            let mut restarted = "#!/bin/sh".to_owned();
            restarted.push(char::from(10));
            append_request(
                &mut restarted,
                &[replay_reply(&second_phase, "initialize", 1)],
            );
            append_request(&mut restarted, &[]);
            append_request(
                &mut restarted,
                &[replay_reply(&second_phase, "thread/resume", 2)],
            );
            append_request(&mut restarted, &[reply("thread/name/set", 3)]);
            append_replayed_turn(&mut restarted, &second_phase, 4);
            restarted.push_str("while IFS= read -r request; do :; done");
            restarted.push(char::from(10));
            worker = TurnWorker::spawn(
                commands.clone(),
                settings_for_script(&fixture, &restarted),
                changes,
            );
        }

        let mut next = command_payload();
        next["commandId"] = "follow-up".into();
        next["messageId"] = "message:follow-up".into();
        next["text"] = json!(second_text);
        let next = commands
            .dispatch(serde_json::from_value(next).unwrap())
            .unwrap();
        assert_ne!(first.run_id, next.run_id);
        worker.sender().wake();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let done: bool = connection
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_runs
                     WHERE run_id = ?1 AND status = 'completed'
                     AND json_extract(payload_json,'$.checkpointId') IS NOT NULL)",
                        [&next.run_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker.shutdown().await.unwrap();

        let captures: Vec<(i64, i64)> =
            {
                let mut query = connection.prepare(
                "SELECT run.ordinal, turn.ordinal FROM orchestration_v2_projection_runs AS run
                 JOIN orchestration_v2_projection_provider_turns AS turn
                 ON turn.run_attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
                 WHERE run.thread_id = ?1 ORDER BY run.ordinal",
            ).unwrap();
                query
                    .query_map([THREAD], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
        assert_eq!(captures, [(1, 1), (2, 2)]);
        let (bound_threads, native, last_ordinal, second_item_ordinal, checkpoints): (i64, String, i64, i64, i64) =
            connection.query_row(
                "SELECT COUNT(*), json_extract(provider.payload_json, '$.nativeThreadRef.nativeId'),
                        provider.last_run_ordinal,
                        (SELECT ordinal FROM orchestration_v2_projection_turn_items
                         WHERE run_id = ?2 AND type = 'user_message'),
                        (SELECT COUNT(*) FROM orchestration_v2_projection_checkpoints WHERE thread_id = ?1)
                 FROM orchestration_v2_projection_provider_threads AS provider WHERE provider.thread_id = ?1",
                rusqlite::params![THREAD, next.run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).unwrap();
        assert_eq!(bound_threads, 1);
        assert_eq!(native, native_id);
        assert_eq!(last_ordinal, 2);
        assert_eq!(second_item_ordinal, 2_000_001);
        assert_eq!(checkpoints, 3);

        let requests: Vec<Value> = fs::read_to_string(fixture.directory.join("requests.ndjson"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let wire_turns: Vec<&Value> = requests
            .iter()
            .filter(|frame| frame["method"] == "turn/start")
            .collect();
        assert_eq!(wire_turns.len(), 2);
        assert_eq!(wire_turns[0]["params"]["input"][0]["text"], first_text);
        assert_eq!(wire_turns[1]["params"]["input"][0]["text"], second_text);
        assert!(
            wire_turns
                .iter()
                .all(|frame| frame["params"]["threadId"] == native_id)
        );
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "thread/start")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "thread/resume")
                .count(),
            usize::from(restart)
        );
    }

    fn resume_replay_frames() -> (Vec<Value>, Vec<Value>) {
        const RECORDING: &str = include_str!(
            "../../../../server/src/orchestration-v2/testkit/fixtures/provider_thread_resume/codex_transcript.ndjson"
        );
        let frames: Vec<Value> = RECORDING
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let second_startup = frames
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record["type"] == "expect_outbound" && record["label"] == "initialize"
            })
            .nth(1)
            .unwrap()
            .0;
        (
            frames[..second_startup].to_vec(),
            frames[second_startup..].to_vec(),
        )
    }

    fn replay_prompt(recording: &[Value]) -> &str {
        recording
            .iter()
            .find_map(|record| {
                if record["type"] == "expect_outbound" && record["label"] == "turn/start" {
                    record["frame"]["params"]["input"][0]["text"].as_str()
                } else {
                    None
                }
            })
            .unwrap()
    }

    fn replay_reply(recording: &[Value], label: &str, id: u64) -> Value {
        let mut reply = recording
            .iter()
            .find(|record| record["label"] == label && record["frame"].get("result").is_some())
            .unwrap()["frame"]
            .clone();
        reply["id"] = id.into();
        reply
    }

    fn append_replayed_turn(script: &mut String, recording: &[Value], config_id: u64) {
        append_request(script, &[replay_reply(recording, "config/read", config_id)]);
        let mut replies = vec![replay_reply(recording, "turn/start", config_id + 1)];
        for record in recording
            .iter()
            .filter(|record| record["type"] == "emit_inbound")
        {
            let frame = &record["frame"];
            if match frame["method"].as_str().unwrap_or_default() {
                "thread/settings/updated"
                | "turn/started"
                | "item/agentMessage/delta"
                | "item/commandExecution/outputDelta"
                | "thread/tokenUsage/updated"
                | "turn/completed" => true,
                "item/started" | "item/completed" => {
                    let item_type = frame["params"]["item"]["type"].as_str();
                    matches!(item_type, Some("agentMessage" | "commandExecution"))
                }
                _ => false,
            } {
                replies.push(frame.clone());
            }
        }
        append_request(script, &replies);
    }

    fn append_request(script: &mut String, frames: &[Value]) {
        script.push_str(
            r#"IFS= read -r received
printf '%s
' "$received" >> requests.ndjson
"#,
        );
        for frame in frames {
            emit_frame(script, frame);
        }
    }

    async fn wait_for_run(fixture: &Fixture, status: &str) -> i64 {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let sequence = fixture.connection().query_row(
                    "SELECT metadata.last_sequence
                     FROM orchestration_v2_projection_runs AS run
                     JOIN orchestration_v2_projection_metadata AS metadata ON metadata.projection_name = 'thread-projections'
                     WHERE run.thread_id = ?1 AND run.status = ?2",
                    rusqlite::params![THREAD, status],
                    |row| row.get::<_, i64>(0),
                );
                if let Ok(sequence) = sequence {
                    return sequence;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("worker did not persist the expected run status")
    }

    fn settings_with_recording(fixture: &Fixture, on_turn_start: &str) -> SettingsFile {
        settings_with_terminal(
            fixture,
            on_turn_start,
            notification("turn/completed", |_| true),
        )
    }

    fn settings_with_terminal(
        fixture: &Fixture,
        on_turn_start: &str,
        terminal: Value,
    ) -> SettingsFile {
        let mut script = "#!/bin/sh\nset -eu\n".to_owned();
        let mut step = |frames: &[Value], action: &str| {
            script.push_str("IFS= read -r received\n");
            script.push_str(action);
            for frame in frames {
                let argument = frame.to_string().replace('\'', "'\"'\"'");
                script.push_str(&format!("printf '%s\\n' '{argument}'\n"));
            }
        };
        step(&[reply("initialize", 1)], "");
        step(&[], "");
        step(&[reply("thread/start", 2)], "");
        step(&[reply("thread/name/set", 3)], "");
        step(&[reply("config/read", 4)], "");
        step(
            &[
                notification("thread/settings/updated", |_| true),
                reply("turn/start", 5),
                notification("turn/started", |_| true),
                notification("item/started", |frame| {
                    frame["params"]["item"]["type"] == "agentMessage"
                }),
                notification("item/agentMessage/delta", |_| true),
                notification("item/completed", |frame| {
                    frame["params"]["item"]["type"] == "agentMessage"
                }),
                notification("thread/tokenUsage/updated", |_| true),
                terminal,
            ],
            on_turn_start,
        );
        script.push_str("while IFS= read -r received; do :; done\n");
        settings_for_script(fixture, &script)
    }

    fn settings_for_script(fixture: &Fixture, script: &str) -> SettingsFile {
        let executable = fixture.directory.join("replay-codex");
        let settings_path = fixture.directory.join("settings.json");
        fs::write(
            &settings_path,
            json!({
                "providerInstances": {
                    "codex": { "driver": "codex", "config": { "binaryPath": executable } }
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        SettingsFile::new(settings_path)
    }

    fn reply(label: &str, id: u64) -> Value {
        let mut frame = notification(label, |frame| frame.get("result").is_some());
        frame["id"] = id.into();
        frame
    }

    fn notification(label: &str, accepts: impl Fn(&Value) -> bool) -> Value {
        CODEX
            .lines()
            .find_map(|line| {
                let record: Value = serde_json::from_str(line).unwrap();
                (record["type"] == "emit_inbound"
                    && record["label"] == label
                    && accepts(&record["frame"]))
                .then(|| record["frame"].clone())
            })
            .unwrap()
    }
}
