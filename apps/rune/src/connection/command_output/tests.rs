use super::*;

#[test]
fn applies_recorded_ansi_appends_and_final_replacement() {
    const TRANSCRIPT: &str = include_str!(
        "../../../../server/src/orchestration-v2/testkit/fixtures/command_output_streaming/codex_transcript.ndjson"
    );
    let mut output = CommandOutput::Pending;
    let mut terminal = TerminalOutput::default();
    output
        .apply(
            &mut terminal,
            Frame::Replace {
                text: String::new(),
                truncated: false,
                running: true,
            },
        )
        .unwrap();
    for row in TRANSCRIPT
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
    {
        if row["frame"]["method"] == "item/commandExecution/outputDelta" {
            output
                .apply(
                    &mut terminal,
                    Frame::Append {
                        text: row["frame"]["params"]["delta"].as_str().unwrap().to_owned(),
                        truncated: false,
                        running: true,
                    },
                )
                .unwrap();
        }
    }
    let normalized = terminal.text().to_owned();
    assert!(normalized.contains("\x1b[0;32mstream line 2"));
    assert!(normalized.contains("progress 100%"));
    assert!(!normalized.contains("50%"));
    output
        .apply(
            &mut terminal,
            Frame::Replace {
                text: normalized.clone(),
                truncated: false,
                running: false,
            },
        )
        .unwrap();
    assert_eq!(
        output,
        CommandOutput::Output {
            text: normalized,
            truncated: false,
            running: false
        }
    );
}

#[test]
fn closing_a_finished_stream_retains_output_without_caching_live_text() {
    let target = CommandOutputTarget {
        thread_id: "thread".into(),
        item_id: "command".into(),
    };
    let attachment = Uuid::nil();
    let mut streams = Subscriptions::default();
    for running in [true, false] {
        streams.active.insert(
            attachment,
            Subscription {
                intent: CommandInspection {
                    owner_thread_id: "thread".into(),
                    target: target.clone(),
                    attachment,
                    fallback_revision: None,
                },
                method: Method::Stream,
                request_id: Some("request".into()),
                output: CommandOutput::Output {
                    text: "command output".into(),
                    truncated: false,
                    running,
                },
                terminal: TerminalOutput::default(),
                opened_at: Instant::now(),
            },
        );
        assert_eq!(streams.remove(attachment).as_deref(), Some("request"));
        assert_eq!(
            streams.cached.contains_key(&(target.clone(), None)),
            !running
        );
    }
}
