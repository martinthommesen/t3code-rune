use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use rusqlite::params;
use t3_rune::providers::claude::SessionTarget;
use uuid::Uuid;

use super::*;
use crate::server::turn_commands::tests::{Fixture, THREAD, command_payload};

const REPLAY: &str = include_str!("../../providers/claude/replay.py");
const LIVE: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/multi_turn/claude_transcript.ndjson"
);
const REOPEN: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/multi_turn_restart/claude_transcript.ndjson"
);

#[tokio::test]
async fn persists_both_stock_text_turns_in_one_native_claude_process() {
    assert_two_turns(LIVE, false).await;
}

#[tokio::test]
async fn resumes_the_native_claude_session_with_exact_durable_billed_usage() {
    assert_two_turns(REOPEN, true).await;
}

async fn assert_two_turns(recording: &str, reopen: bool) {
    let fixture = Fixture::new();
    let workspace = fixture.directory.join("workspace");
    fs::create_dir(&workspace).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-qb", "main"])
            .current_dir(&workspace)
            .status()
            .unwrap()
            .success()
    );
    fs::write(workspace.join("readme.txt"), "Fixture\n").unwrap();
    let database = fixture.connection();
    database
        .execute(
            "UPDATE projection_projects SET workspace_root=?1",
            [workspace.to_str().unwrap()],
        )
        .unwrap();
    let title = if reopen {
        "Replay fixture: multi_turn_restart"
    } else {
        "Replay fixture: multi_turn"
    };
    database.execute("UPDATE orchestration_v2_projection_threads
        SET provider_instance_id='claudeAgent', default_provider='claudeAgent', runtime_mode='full-access', title=?1,
            payload_json=json_set(payload_json, '$.providerInstanceId', 'claudeAgent',
              '$.modelSelection.instanceId','claudeAgent','$.modelSelection.model','claude-sonnet-4-6',
              '$.runtimeMode', 'full-access', '$.lastVisitedAt',NULL,'$.title',?1)", [title]).unwrap();
    database.execute("UPDATE orchestration_events SET payload_json=(SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id=?1)
        WHERE stream_id=?1 AND event_type='thread.created'", [THREAD]).unwrap();
    let recording_path = fixture.directory.join("claude.ndjson");
    fs::write(&recording_path, recording).unwrap();
    let executable = fixture.directory.join("claude");
    fs::write(&executable, REPLAY).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let settings = SettingsFile::new(fixture.directory.join("settings.json"));
    let log_path = fixture.directory.join("frames.ndjson");
    fs::write(settings.path(), json!({
        "providerInstances":{"claudeAgent":{"driver":"claudeAgent","config":{"binaryPath":executable},
            "environment":[{"name":"T3_CLAUDE_REPLAY_RECORDING","value":recording_path},
                {"name":"T3_CLAUDE_REPLAY_LOG","value":log_path}]}},
    }).to_string()).unwrap();
    let messages: Vec<Value> = recording
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let native: Uuid = messages
        .iter()
        .filter_map(|line| line["frame"]["options"]["sessionId"].as_str())
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let prompts = messages
        .iter()
        .filter(|line| line["frame"]["type"] == "prompt.offer")
        .map(|line| {
            line["frame"]["message"]["message"]["content"]
                .as_str()
                .unwrap()
        });
    let expected_native_items: Vec<&str> = messages
        .iter()
        .filter(|line| line["frame"]["type"] == "assistant")
        .map(|line| line["frame"]["uuid"].as_str().unwrap())
        .collect();
    let commands = fixture.commands();
    let mut runtimes = Runtimes::default();
    for (index, text) in prompts.enumerate() {
        let command_id = format!("message-{}", index + 1);
        let mut command = command_payload();
        command["commandId"] = json!(command_id);
        command["messageId"] = json!(command_id);
        command["text"] = json!(text);
        let dispatch = commands
            .dispatch_with_settings(
                serde_json::from_value(command).unwrap(),
                Some(&settings.read().unwrap()),
            )
            .unwrap();
        let effect_id = format!(
            "effect:{command_id}:provider-turn.start:{}",
            dispatch.run_id
        );
        let mut claim = commands
            .claim_claude_start(&effect_id, "claude-worker")
            .unwrap()
            .unwrap();
        if index == 0 {
            assert!(matches!(claim.target, SessionTarget::New(_)));
            claim.target = SessionTarget::New(native);
        } else {
            assert_eq!(
                claim.target,
                SessionTarget::Resume {
                    session_id: native,
                    resume_at: None
                }
            );
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            runtimes.execute(
                &commands,
                &settings,
                &claim,
                &TurnQueue::new(commands.clone(), tokio::sync::broadcast::channel(4).0),
                |_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let capture_id = format!("effect:checkpoint.capture:{}", dispatch.run_id);
        assert!(
            commands
                .capture_checkpoint(&capture_id, "claude-worker")
                .await
                .unwrap()
                .is_some()
        );
        if index == 0 && reopen {
            runtimes.idle.get_mut(THREAD).unwrap().idle_since -= KEEP_IDLE_FOR;
            runtimes.release_idle().await;
        }
    }
    let owned = runtimes.idle.remove(THREAD).unwrap().process.unwrap();
    owned.session.shutdown().await.unwrap();
    let log = fs::read_to_string(log_path).unwrap();
    assert_eq!(
        log.lines().filter(|line| line.contains("\"argv\"")).count(),
        if reopen { 2 } else { 1 }
    );
    let mut turns = database
        .prepare(
            "SELECT payload_json FROM orchestration_v2_projection_provider_turns ORDER BY ordinal",
        )
        .unwrap();
    let turns: Vec<Value> = turns
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect();
    assert_eq!(turns.len(), 2);
    for (index, turn) in turns.iter().enumerate() {
        assert_eq!(turn["status"], "completed");
        assert_eq!(
            turn["nativeTurnRef"],
            json!({"driver":"claudeAgent","nativeId":expected_native_items[index],"strength":"weak"})
        );
        let usage = &turn["turnTokenUsage"];
        assert_eq!(usage["inputTokens"], if index == 0 { 11795 } else { 11814 });
        assert_eq!(usage["outputTokens"], 7);
        assert_eq!(usage["usageStatus"], "complete");
        assert_eq!(
            usage["otherUsage"],
            json!({"inputTokens":0,"outputTokens":0})
        );
    }
    let threads: (u64, u64, u64) = database.query_row("SELECT
       (SELECT count(*) FROM orchestration_v2_projection_runs WHERE status='completed'),
       (SELECT count(*) FROM orchestration_v2_projection_provider_sessions WHERE driver='claudeAgent'),
       (SELECT count(*) FROM orchestration_v2_projection_checkpoints WHERE status='ready' AND thread_id=?1)", params![THREAD],
       |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(threads, (2, 1, 3));
    if let Some(out) = std::env::var_os("T3_CLAUDE_DURABLE_PROOF") {
        let mut events = database
            .prepare(
                "SELECT json_set(metadata_json, '$.id', event_id, '$.threadId', stream_id,
                '$.occurredAt', occurred_at, '$.type', event_type, '$.payload', json(payload_json))
                FROM orchestration_events ORDER BY sequence",
            )
            .unwrap();
        let events: Vec<Value> = events
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect();
        let name = if reopen {
            "native-multi-turn-restart.json"
        } else {
            "native-multi-turn.json"
        };
        fs::write(
            PathBuf::from(out).join(name),
            serde_json::to_string_pretty(&events).unwrap(),
        )
        .unwrap();
    }
}
