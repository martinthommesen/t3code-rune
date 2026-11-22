use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::super::tests::{Fixture, THREAD};
use super::super::{PreparingDispatch, append_preparing_dispatch};
use super::{MutationOutcome, QueuedRunMutation};

#[test]
fn edits_and_cancels_held_worktree_prompts_with_canonical_receipts() {
    let fixture = Fixture::new();
    let commands = fixture.commands();
    let mut database = fixture.connection();
    let transaction = database.transaction().unwrap();
    append_preparing_dispatch(
        &transaction,
        PreparingDispatch {
            provider: crate::server::turn_commands::NativeProvider::Codex,
            command_id: "launch",
            thread_id: THREAD,
            message_id: "message:first",
            text: "First prompt",
            model_selection: json!({"instanceId":"codex","model":"gpt-5.5"}),
            workspace_strategy: &json!({"type":"worktree","baseRef":"main"}),
            now: "2026-07-17T10:35:09.000Z",
        },
    )
    .unwrap();
    transaction.commit().unwrap();
    let enqueue = |id: &str| {
        commands.dispatch(serde_json::from_value(json!({
            "type":"message.dispatch","commandId":id,"threadId":THREAD,"messageId":format!("message:{id}"),
            "createdBy":"user","creationSource":"web","text":"Follow up","attachments":[],
            "dispatchMode":{"type":"queue_after_active"}
        })).unwrap()).unwrap().run_id
    };
    let remove_run = enqueue("drop");
    let editable_run = enqueue("keep");
    let stopped_at = commands
        .stop_thread(
            serde_json::from_value(json!({
                "type":"thread.stop","commandId":"stop","threadId":THREAD,
            }))
            .unwrap(),
        )
        .unwrap()
        .sequence;
    let before = payload(&database, "messages", "run_id", &editable_run);
    let apply = |input: Value| {
        commands
            .mutate_queued_run(&serde_json::from_value::<QueuedRunMutation>(input).unwrap())
            .unwrap()
    };
    let edit = json!({
        "type":"queued-run.edit","commandId":"edit","threadId":THREAD,"runId":editable_run,
        "text":"  Revised prompt  \n","attachments":[],
    });
    assert_eq!(
        apply(edit.clone()),
        MutationOutcome::Accepted {
            sequence: stopped_at + 1,
            committed: true
        }
    );
    assert_eq!(
        apply(edit),
        MutationOutcome::Accepted {
            sequence: stopped_at + 1,
            committed: false
        }
    );
    let edited = payload(&database, "messages", "run_id", &editable_run);
    let mut expected = before;
    expected["text"] = json!("  Revised prompt  \n");
    expected["updatedAt"] = edited["updatedAt"].clone();
    assert_eq!(edited, expected);
    assert_eq!(events(&database, "edit"), "message.updated");

    let cancel = json!({"type":"queued-run.cancel","commandId":"cancel","threadId":THREAD,"runId":remove_run});
    assert_eq!(
        apply(cancel.clone()),
        MutationOutcome::Accepted {
            sequence: stopped_at + 4,
            committed: true
        }
    );
    assert_eq!(
        apply(cancel),
        MutationOutcome::Accepted {
            sequence: stopped_at + 4,
            committed: false
        }
    );
    assert_eq!(
        events(&database, "cancel"),
        "run.updated,run-attempt.updated,node.updated"
    );
    let cancelled = payload(&database, "runs", "run_id", &remove_run);
    assert_eq!(cancelled["status"], "cancelled");
    assert!(cancelled["queuePosition"].is_null());
    assert_eq!(cancelled["queueHeld"], true);
    assert_eq!(
        payload(&database, "runs", "run_id", &editable_run)["queuePosition"],
        2
    );
    assert_eq!(
        payload(&database, "messages", "run_id", &remove_run)["text"],
        "Follow up"
    );

    let blank = json!({
        "type":"queued-run.edit","commandId":"blank","threadId":THREAD,"runId":editable_run,
        "text":" \u{feff}\t",
    });
    assert!(
        matches!(apply(blank.clone()), MutationOutcome::Rejected(detail) if detail.ends_with("cannot be edited to an empty message."))
    );
    assert!(matches!(
        apply(blank),
        MutationOutcome::PreviouslyRejected(_)
    ));
    let stored: (String, i64) = database.query_row(
        "SELECT status, result_sequence FROM orchestration_command_receipts WHERE command_id = 'blank'",
        [], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(stored, ("rejected".to_owned(), stopped_at + 4));
    let outbox: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM orchestration_v2_effect_outbox",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outbox, 0);
}

fn payload(database: &Connection, table: &str, column: &str, id: &str) -> Value {
    let encoded: String = database
        .query_row(
            &format!(
                "SELECT payload_json FROM orchestration_v2_projection_{table} WHERE {column} = ?1"
            ),
            [id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&encoded).unwrap()
}

fn events(database: &Connection, command_id: &str) -> String {
    database
        .query_row(
            "SELECT group_concat(event_type) FROM (
            SELECT event_type FROM orchestration_events WHERE command_id = ?1 ORDER BY sequence
         )",
            params![command_id],
            |row| row.get(0),
        )
        .unwrap()
}
