use rusqlite::params;
use serde_json::{Value, json};
use tokio::time::timeout;

use super::*;
use crate::server::turn_commands::tests::{Fixture, THREAD};

const ITEM: &str = "turn-item:command";
const AT: &str = "2026-10-03T12:00:00Z";

fn write_item(
    fixture: &Fixture,
    item_type: &str,
    status: &str,
    output: Option<&str>,
    driver: &str,
) {
    let mut item = json!({
        "id": ITEM,
        "threadId": THREAD,
        "type": item_type,
        "status": status,
        "nativeItemRef": { "driver": driver, "nativeId": "command-1", "strength": "strong" },
        "startedAt": AT,
        "updatedAt": AT,
    });
    if let Some(output) = output {
        item["output"] = output.into();
    }
    fixture
        .connection()
        .execute(
            "INSERT INTO orchestration_v2_projection_turn_items
               (turn_item_id, thread_id, ordinal, type, status, updated_at, payload_json)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6)
             ON CONFLICT(turn_item_id) DO UPDATE
               SET status = excluded.status, payload_json = excluded.payload_json",
            params![ITEM, THREAD, item_type, status, AT, item.to_string()],
        )
        .unwrap();
}

fn write_command(fixture: &Fixture, status: &str, output: Option<&str>) {
    write_item(fixture, "command_execution", status, output, "codex");
}

fn setup() -> (
    Fixture,
    CommandOutputHub,
    broadcast::Sender<ProjectionChange>,
) {
    let fixture = Fixture::new();
    let (changes, _) = broadcast::channel(2);
    let hub = CommandOutputHub::new(fixture.commands(), changes.clone());
    (fixture, hub, changes)
}

async fn next(subscription: &mut CommandOutputSubscription) -> Value {
    let frame = timeout(Duration::from_secs(3), subscription.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::to_value(frame).unwrap()
}

fn replace(text: &str, running: bool) -> Value {
    json!({"kind": "replace", "text": text, "truncated": false, "running": running})
}

fn notify(changes: &broadcast::Sender<ProjectionChange>, thread_id: &str) {
    assert!(
        changes
            .send(ProjectionChange::Thread(thread_id.into()))
            .is_ok()
    );
}

#[tokio::test]
async fn scopes_replacements_and_coalesced_appends_to_one_command() {
    let (fixture, hub, changes) = setup();
    write_command(&fixture, "running", None);
    hub.append(THREAD, ITEM, "begin\n");
    let mut subscription = hub.subscribe(THREAD, ITEM).unwrap();
    assert_eq!(next(&mut subscription).await, replace("begin\n", true));
    hub.append("another-thread", ITEM, "unrelated");
    hub.append(THREAD, "another-item", "unrelated");
    hub.append(THREAD, ITEM, "live ");
    hub.append(THREAD, ITEM, "output\n");
    assert_eq!(
        next(&mut subscription).await,
        json!({"kind":"append", "text":"live output\n", "truncated":false, "running":true})
    );
    hub.finish(THREAD, ITEM);
    write_command(&fixture, "completed", Some("persisted\n"));
    notify(&changes, THREAD);
    assert_eq!(next(&mut subscription).await, replace("persisted\n", false));
    assert!(subscription.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn recovers_a_slow_subscriber_from_the_tail_and_a_missed_settlement() {
    let (fixture, hub, changes) = setup();
    write_command(&fixture, "running", None);
    let mut subscription = hub.subscribe(THREAD, ITEM).unwrap();
    assert_eq!(next(&mut subscription).await, replace("", true));

    let flood = "恭".repeat(MAX_APPEND_CHARS + 1);
    hub.append(THREAD, ITEM, &flood);
    assert_eq!(next(&mut subscription).await, replace(&flood, true));
    hub.finish(THREAD, ITEM);
    write_command(&fixture, "interrupted", None);
    notify(&changes, THREAD);
    for index in 0..4 {
        notify(&changes, &format!("unrelated-{index}"));
    }
    assert_eq!(next(&mut subscription).await, replace(&flood, false));
}

#[tokio::test]
async fn replaces_an_evicted_tail_and_treats_persisted_empty_output_as_final() {
    let (fixture, hub, changes) = setup();
    write_command(&fixture, "waiting", Some("snapshot"));
    let mut subscription = hub.subscribe(THREAD, ITEM).unwrap();
    assert_eq!(next(&mut subscription).await, replace("snapshot", true));
    write_command(&fixture, "running", Some("updated snapshot"));
    notify(&changes, THREAD);
    assert_eq!(
        next(&mut subscription).await,
        replace("updated snapshot", true)
    );
    write_command(&fixture, "running", None);
    notify(&changes, THREAD);
    assert!(
        timeout(Duration::from_millis(200), subscription.recv())
            .await
            .is_err()
    );

    hub.append(THREAD, ITEM, "before eviction\n");
    assert_eq!(next(&mut subscription).await["kind"], "append");
    for index in 0..MAX_LIVE_COMMANDS {
        hub.append(THREAD, &format!("command:{index}"), "another tail");
    }
    hub.append(THREAD, ITEM, "after eviction\n");
    assert_eq!(
        next(&mut subscription).await,
        replace("after eviction\n", true)
    );
    hub.finish(THREAD, ITEM);
    write_command(&fixture, "failed", Some(""));
    notify(&changes, THREAD);
    assert_eq!(next(&mut subscription).await, replace("", false));
}

#[tokio::test]
async fn filters_historical_bash_output_and_bounds_large_persisted_text() {
    let (fixture, hub, _) = setup();
    write_item(
        &fixture,
        "command_execution",
        "completed",
        Some(r#"{"stdout":"  built","stderr":"1 warning","interrupted":false}"#),
        "claudeAgent",
    );
    let mut subscription = hub.subscribe(THREAD, ITEM).unwrap();
    assert_eq!(
        next(&mut subscription).await,
        replace("  built\n1 warning", false)
    );

    let line = "🙂".repeat(MAX_FINAL_SOURCE_CHARS);
    write_command(&fixture, "completed", Some(&line));
    let mut subscription = hub.subscribe(THREAD, ITEM).unwrap();
    let frame = next(&mut subscription).await;
    assert_eq!(frame["truncated"], true);
    assert_eq!(frame["running"], false);
    let text = frame["text"].as_str().unwrap();
    assert!(!text.is_empty());
    assert!(text.ends_with('🙂'));
    assert!(utf16_len(text) <= TAIL_CHARS);
}

#[test]
fn returns_contract_error_for_another_thread_or_non_command_row() {
    let (fixture, hub, _) = setup();
    write_item(&fixture, "assistant_message", "completed", None, "codex");
    for thread in [THREAD, "another-thread"] {
        let Err(error) = hub.subscribe(thread, ITEM) else {
            panic!("noncommand must not open a stream");
        };
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({"_tag":"OrchestrationV2CommandOutputError", "threadId":thread, "message":"No such command."})
        );
    }
    assert!(hub.cache.lock().unwrap().subscribers.is_empty());
}
