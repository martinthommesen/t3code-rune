use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::timeout;

use super::*;

const FIRST: &str = "first fixture turn complete";
const SECOND: &str = "second fixture turn complete";
const APPEND: &str = "Local workspace runtime instructions.";
const TITLE: &str = "Conversation";
const MODEL: &str = "claude-sonnet-4-6";
const MULTI_TURN: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/multi_turn/claude_transcript.ndjson"
);
const MULTI_TURN_RESTART: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/multi_turn_restart/claude_transcript.ndjson"
);
const RESULT_ERROR: &str = include_str!(
    "../../../../server/src/orchestration-v2/testkit/fixtures/claude_result_is_error/claude_transcript.ndjson"
);

struct Replay {
    profile: TempDir,
    id: Uuid,
}

impl Replay {
    fn new(recording: &str) -> Self {
        let profile = tempfile::tempdir().unwrap();
        let executable = profile.path().join("claude");
        fs::write(&executable, include_str!("replay.py")).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(profile.path().join("recording.ndjson"), recording).unwrap();
        let id = recording
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find_map(|record| {
                record["frame"]["options"]["sessionId"]
                    .as_str()
                    .map(Uuid::parse_str)
            })
            .unwrap()
            .unwrap();
        Self { profile, id }
    }

    fn launch(&self, target: SessionTarget) -> Launch {
        let dir = self.profile.path();
        Launch {
            executable: dir.join("claude"),
            working_directory: dir.to_path_buf(),
            environment: [(
                "T3_CLAUDE_REPLAY_RECORDING".into(),
                dir.join("recording.ndjson").into_os_string(),
            )]
            .into(),
            target,
            model: MODEL.into(),
            title: Some(TITLE.into()),
            append_system_prompt: APPEND.into(),
            permission: PermissionMode::FullAccess,
        }
    }

    fn observed(&self) -> Vec<Value> {
        fs::read_to_string(self.profile.path().join("frames.ndjson"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

async fn read_turn(
    session: &mut Session,
) -> (Option<Initialization>, AssistantMessage, Box<TurnResult>) {
    let mut initialization = None;
    let mut final_snapshot = None;
    loop {
        let incoming = timeout(Duration::from_secs(5), session.next_message())
            .await
            .unwrap()
            .unwrap();
        match incoming {
            Message::Initialized(init) => initialization = Some(init),
            Message::Assistant(message) => final_snapshot = Some(message),
            Message::Result(result) => return (initialization, final_snapshot.unwrap(), result),
            Message::Other(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
}

async fn assert_turn(session: &mut Session, expected: &str) -> (AssistantMessage, Box<TurnResult>) {
    let prompt = format!("Respond with exactly: {expected}");
    session.offer_text(&prompt, None).await.unwrap();
    assert!(matches!(
        session.offer_text(&prompt, None).await,
        Err(Error::ActiveTurn)
    ));
    let (initialization, snapshot, result) = read_turn(session).await;
    let initialization = initialization.unwrap();
    assert_eq!(initialization.model, MODEL);
    assert_eq!(initialization.session_id, session.session_id());
    assert!(
        matches!(snapshot.message.content.as_slice(), [AssistantBlock::Text { text }] if text == expected)
    );
    assert_eq!(snapshot.session_id, result.session_id);
    assert_ne!(snapshot.uuid, result.uuid);
    assert!(result.succeeded());
    assert_eq!(result.result.as_deref(), Some(expected));
    assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(result.usage.unwrap().output_tokens, Some(7));
    (snapshot, result)
}

#[tokio::test]
async fn streams_two_recorded_prompts_through_one_cli() {
    let fixture = Replay::new(MULTI_TURN);
    let mut session = Session::open(fixture.launch(SessionTarget::New(fixture.id)))
        .await
        .unwrap();
    let (first, _) = assert_turn(&mut session, FIRST).await;
    let (second, result) = assert_turn(&mut session, SECOND).await;
    assert_ne!(first.message.id, second.message.id);
    assert_eq!(
        result.model_usage.as_ref().unwrap()[MODEL].output_tokens,
        Some(14)
    );
    session.shutdown().await.unwrap();
    let observed = fixture.observed();
    assert_eq!(observed.len(), 5);
    assert_eq!(
        observed[1]["request"],
        json!({
            "subtype": "initialize", "appendSystemPrompt": APPEND, "title": TITLE,
        })
    );
    assert_eq!(observed[4], json!({"eof":"--session-id"}));
}

#[tokio::test]
async fn restarts_the_recorded_native_session_with_fresh_process_usage() {
    let fixture = Replay::new(MULTI_TURN_RESTART);
    let mut session = Session::open(fixture.launch(SessionTarget::New(fixture.id)))
        .await
        .unwrap();
    let (first, result) = assert_turn(&mut session, FIRST).await;
    assert_eq!(result.usage.unwrap().cache_read_input_tokens, Some(11792));
    session.shutdown().await.unwrap();
    let mut resumed = Session::open(fixture.launch(SessionTarget::Resume {
        session_id: result.session_id,
        resume_at: None,
    }))
    .await
    .unwrap();
    let (second, result) = assert_turn(&mut resumed, SECOND).await;
    assert_ne!(first.uuid, second.uuid);
    assert_eq!(result.usage.unwrap().cache_read_input_tokens, Some(11811));
    assert_eq!(
        result.model_usage.as_ref().unwrap()[MODEL].output_tokens,
        Some(7)
    );
    resumed.shutdown().await.unwrap();
    let observed = fixture.observed();
    assert_eq!(observed.len(), 8);
    let flags = |index: usize| observed[index]["argv"].as_array().unwrap();
    assert!(flags(0).contains(&json!(format!("--session-id={}", fixture.id))));
    assert!(flags(4).contains(&json!(format!("--resume={}", fixture.id))));
    assert!(
        !flags(4)
            .iter()
            .any(|flag| flag.as_str().unwrap().starts_with("--session-id"))
    );
    assert_eq!(observed[7], json!({"eof":"--resume"}));
}

#[tokio::test]
async fn does_not_accept_a_success_subtype_with_a_recorded_authentication_failure() {
    let fixture = Replay::new(RESULT_ERROR);
    let mut session = Session::open(fixture.launch(SessionTarget::New(fixture.id)))
        .await
        .unwrap();
    session
        .offer_text("Say hello before the credentials expire.", None)
        .await
        .unwrap();
    let (_, _, result) = read_turn(&mut session).await;
    assert_eq!(result.subtype, "success");
    assert_eq!(result.api_error_status, Some(401));
    assert!(result.is_error && !result.succeeded());
    session
        .offer_text("Try again now that auth is back.", None)
        .await
        .unwrap();
    let (_, _, result) = read_turn(&mut session).await;
    assert!(result.succeeded());
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_a_tool_without_an_approval_handler_using_the_cli_control_contract() {
    let fixture = Replay::new(MULTI_TURN);
    let mut launch = fixture.launch(SessionTarget::New(fixture.id));
    launch.permission = PermissionMode::Default;
    launch
        .environment
        .insert("T3_CLAUDE_REPLAY_APPROVAL".into(), "control-1".into());
    let mut session = Session::open(launch).await.unwrap();
    session
        .offer_text(&format!("Respond with exactly: {FIRST}"), None)
        .await
        .unwrap();
    let control = timeout(Duration::from_secs(5), session.next_message())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(control, Message::RejectedControl { subtype } if subtype == "can_use_tool"));
    let (_, _, result) = read_turn(&mut session).await;
    assert!(result.succeeded());
    assert_turn(&mut session, SECOND).await;
    session.shutdown().await.unwrap();
    assert_eq!(
        fixture.observed()[3],
        json!({"type":"control_response", "response":{
            "subtype":"success", "request_id":"control-1", "response":{
                "behavior":"deny", "toolUseID":"toolu_fixture", "message":"Tool approval is unavailable.",
            },
        }})
    );
}
