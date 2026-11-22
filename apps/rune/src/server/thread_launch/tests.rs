mod claude;
mod codex_reasoning;

use std::fs;
use std::process::Command;

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;

struct Fixture {
    folder: TempDir,
    database: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let folder = tempfile::tempdir().unwrap();
        let database = folder.path().join("statev2.sqlite");
        let project = folder.path().join("project");
        fs::create_dir(&project).unwrap();
        let sql = Connection::open(&database).unwrap();
        sql.execute_batch(include_str!("../bootstrap/schema.sql"))
            .unwrap();
        sql.execute(
            "INSERT INTO projection_projects (
                project_id, title, workspace_root, created_at, updated_at, scripts_json
             ) VALUES ('project:local', 'Project', ?1, '2026-06-20', '2026-06-20', '[]')",
            [project.to_str().unwrap()],
        )
        .unwrap();
        Self {
            folder,
            database,
            project,
        }
    }

    fn draft(&self, thread_id: &str, project_id: &str, command_id: Option<&str>) {
        let sql = Connection::open(&self.database).unwrap();
        sql.execute(
            "INSERT INTO orchestration_v2_projection_threads (
                thread_id, project_id, title, default_provider, runtime_mode, interaction_mode,
                created_at, updated_at, payload_json
             ) VALUES (?1, ?2, 'Draft', 'codex', 'full-access', 'default', '2026-06-20', '2026-06-20', '{}')",
            [thread_id, project_id],
        )
        .unwrap();
        if let Some(command_id) = command_id {
            sql.execute(
                "INSERT INTO orchestration_command_receipts (
                    command_id, aggregate_kind, aggregate_id, command_type, accepted_at, result_sequence, status
                 ) VALUES (?1, 'thread', ?2, 'thread.create', '2026-06-20', 4, 'accepted')",
                params![command_id, thread_id],
            )
            .unwrap();
        }
    }

    async fn prepare(&self, request: Value, enabled: bool) -> Result<PreparedLaunch, LaunchError> {
        prepare(
            serde_json::from_value(request).unwrap(),
            LaunchContext {
                database: &self.database,
                scratch_root: &self.folder.path().join("scratch"),
                settings_path: None,
                provider_enabled: enabled,
                terminals: None,
            },
        )
        .await
    }
}

fn launch_request() -> Value {
    json!({
        "commandId": " command:launch ",
        "projectId": "project:local",
        "threadId": "thread:local",
        "title": "  Understand this project \u{feff}",
        "modelSelection": { "provider": "codex", "model": " gpt-5.1-codex " },
        "runtimeMode": "full-access",
        "interactionMode": "default",
        "workspaceStrategy": { "type": "root" },
        "generateTitle": true,
        "initialMessage": {
            "messageId": "message:local",
            "text": "  Read the project.\n",
            "attachments": []
        }
    })
}

#[tokio::test]
async fn creates_the_atomic_claim_payload_from_a_real_workspace_without_mutating_it() {
    let fixture = Fixture::new();
    let initialized = Command::new("git")
        .current_dir(&fixture.project)
        .args(["init", "--initial-branch=workspace-branch", "--template="])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let before = fs::read(&fixture.database).unwrap();
    let plan = fixture.prepare(launch_request(), true).await.unwrap();
    assert_eq!(plan.workspace_root, fixture.project);
    assert_eq!(
        plan.claim_command(),
        json!({
            "type": "thread.create",
            "commandId": "command:launch",
            "threadId": "thread:local",
            "projectId": "project:local",
            "title": "Understand this project",
            "modelSelection": { "instanceId": "codex", "model": "gpt-5.1-codex" },
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "branch": "workspace-branch",
            "worktreePath": null,
            "createdBy": "user",
            "creationSource": "web",
            "initialMessage": {
                "messageId": "message:local",
                "text": "  Read the project.\n",
                "attachments": [],
                "titleSeed": "Understand this project",
                "workspaceStrategy": {"type":"root"}
            }
        })
    );
    assert_eq!(fs::read(&fixture.database).unwrap(), before);
    assert!(fixture.project.join(".git/HEAD").is_file());
    assert!(!fixture.project.join(".git/index").exists());
}

#[tokio::test]
async fn initial_message_to_a_reused_draft_defers_start_with_its_own_receipt() {
    let fixture = Fixture::new();
    fixture.draft("thread:local", "project:local", None);
    let mut request = launch_request();
    request["reuseExistingThread"] = json!(true);
    request["workspaceStrategy"]["branch"] = json!(" branch ");
    let plan = fixture.prepare(request, true).await.unwrap();
    assert_eq!(
        plan.claim_command(),
        json!({
            "type": "thread.metadata.update",
            "commandId": "command:launch",
            "threadId": "thread:local",
            "expectedEmpty": true
        })
    );
    let claim = DispatchCommit {
        sequence: 8,
        replayed: false,
        thread_id: Some("thread:local".to_owned()),
        run_id: None,
    };
    let AfterClaim::Dispatch(dispatch) = plan.after_claim(claim).unwrap() else {
        panic!("A reused draft needs a separate initial message.");
    };
    assert_eq!(
        dispatch.command,
        json!({
            "type": "message.dispatch",
            "commandId": "command:launch:initial-message",
            "threadId": "thread:local",
            "messageId": "message:local",
            "text": "  Read the project.\n",
            "attachments": [],
            "titleSeed": "Understand this project",
            "modelSelection": { "instanceId": "codex", "model": "gpt-5.1-codex" },
            "dispatchMode": {"type":"defer_start", "workspaceStrategy":{"type":"root","branch":"branch"}},
            "createdBy": "user",
            "creationSource": "web"
        })
    );
    let commit = plan
        .after_initial_message(
            dispatch,
            DispatchCommit {
                sequence: 12,
                replayed: false,
                thread_id: None,
                run_id: Some("run:accepted".into()),
            },
        )
        .unwrap();
    assert!(commit.schedule_preparation);
    assert!(plan.prepare_workspace(&commit).await.unwrap().is_none());
    let sql = Connection::open(&fixture.database).unwrap();
    sql.execute(
        "INSERT INTO orchestration_v2_projection_runs (
            run_id, thread_id, ordinal, provider, status, requested_at, payload_json
         ) VALUES ('run:accepted', 'thread:local', 1, 'codex', 'preparing', '2026-06-20', '{}')",
        [],
    )
    .unwrap();
    let workspace = plan.prepare_workspace(&commit).await.unwrap().unwrap();
    assert_eq!(workspace.workspace_root, fixture.project);
    assert_eq!(
        workspace.metadata_command,
        json!({
            "type": "thread.metadata.update",
            "commandId": "command:launch:workspace",
            "threadId": "thread:local",
            "branch": "branch",
            "worktreePath": null
        })
    );
    assert_eq!(
        workspace.release_command.unwrap(),
        json!({
            "type": "prepared-run.release",
            "commandId": "command:launch:release",
            "threadId": "thread:local",
            "runId": "run:accepted"
        })
    );
    sql.execute(
        "UPDATE orchestration_v2_projection_runs SET status = 'starting' WHERE run_id = 'run:accepted'",
        [],
    )
    .unwrap();
    assert!(plan.prepare_workspace(&commit).await.unwrap().is_none());
    let projection = json!({
        "thread": {"id":"thread:local","projectId":"project:local"},
        "runs":[{"id":"run:accepted"}],
    });
    let result = plan.result(&commit, projection.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({
            "threadId": "thread:local",
            "projection": projection,
            "resumed": false,
            "sequence": 12
        })
    );
}

#[tokio::test]
async fn a_replay_recovers_the_claim_id_and_resumes_only_after_the_message_was_accepted() {
    let fixture = Fixture::new();
    fixture.draft("thread:original", "project:local", Some("command:launch"));
    let mut request = launch_request();
    request.as_object_mut().unwrap().remove("threadId");
    let plan = fixture.prepare(request.clone(), false).await.unwrap();
    assert_eq!(plan.thread_id, "thread:original");
    let claim = || DispatchCommit {
        sequence: 4,
        replayed: true,
        thread_id: None,
        run_id: None,
    };
    let accepted_turn = || DispatchCommit {
        sequence: 12,
        replayed: false,
        thread_id: None,
        run_id: Some("run:accepted".into()),
    };
    let AfterClaim::Dispatch(first) = plan.after_claim(claim()).unwrap() else {
        panic!("Claim-only replay still needs its initial message.");
    };
    assert!(
        !plan
            .after_initial_message(first, accepted_turn())
            .unwrap()
            .resumed
    );
    let sql = Connection::open(&fixture.database).unwrap();
    sql.execute(
        "INSERT INTO orchestration_command_receipts (
           command_id, aggregate_kind, aggregate_id, command_type, accepted_at, result_sequence, status
         ) VALUES ('command:launch:initial-message', 'thread', 'thread:original',
                   'message.dispatch', '2026-06-20', 12, 'accepted')",
        [],
    ).unwrap();
    let replay = fixture.prepare(request, false).await.unwrap();
    let AfterClaim::Dispatch(again) = replay.after_claim(claim()).unwrap() else {
        panic!("Fallback command should go through its own receipt.");
    };
    assert!(
        replay
            .after_initial_message(again, accepted_turn())
            .unwrap()
            .resumed
    );
}

#[tokio::test]
async fn rejects_cross_project_reuse_and_disabled_new_launches() {
    let fixture = Fixture::new();
    fixture.draft("thread:local", "different-project", None);
    let mut request = launch_request();
    request["reuseExistingThread"] = json!(true);
    let error = fixture.prepare(request, true).await.err().unwrap();
    assert!(error.to_string().contains("Only an empty active thread"));
    let disabled = fixture
        .prepare(launch_request(), false)
        .await
        .err()
        .unwrap();
    assert_eq!(
        serde_json::to_value(disabled).unwrap()["cause"]["name"],
        "ThreadManagementProviderInstanceDisabledError"
    );
}

#[tokio::test]
async fn a_launch_with_no_message_returns_the_committed_projection_without_message_sequence() {
    let fixture = Fixture::new();
    fixture.draft("thread:local", "project:local", None);
    let mut request = launch_request();
    request.as_object_mut().unwrap().remove("initialMessage");
    let plan = fixture.prepare(request, true).await.unwrap();
    let claim = DispatchCommit {
        sequence: 4,
        replayed: false,
        thread_id: Some("thread:local".to_owned()),
        run_id: None,
    };
    let AfterClaim::Complete(commit) = plan.after_claim(claim).unwrap() else {
        panic!("No initial message needs no second dispatch.");
    };
    assert!(commit.schedule_preparation);
    assert!(
        plan.prepare_workspace(&commit)
            .await
            .unwrap()
            .unwrap()
            .release_command
            .is_none()
    );
    let result = plan
        .result(
            &commit,
            json!({
                "thread": {"id":"thread:local", "projectId":"project:local"},
                "runs": []
            }),
        )
        .unwrap();
    assert!(
        serde_json::to_value(result)
            .unwrap()
            .get("sequence")
            .is_none()
    );
}

#[tokio::test]
async fn atomic_claim_has_one_receipt_and_title_effect_and_waits_for_release() {
    let fixture = Fixture::new();
    let plan = fixture.prepare(launch_request(), true).await.unwrap();
    let (first, second) = std::thread::scope(|scope| {
        let left = scope.spawn(|| plan.persist_initial_claim(true));
        let right = scope.spawn(|| plan.persist_initial_claim(true));
        (
            left.join().unwrap().unwrap(),
            right.join().unwrap().unwrap(),
        )
    });
    assert_ne!(first.replayed, second.replayed);
    assert_eq!(first.sequence, second.sequence);
    assert_eq!(first.run_id, second.run_id);
    assert!(plan.persist_initial_claim(false).unwrap().replayed);
    let database = Connection::open(&fixture.database).unwrap();
    let event_types: Vec<String> = database
        .prepare("SELECT event_type FROM orchestration_events ORDER BY sequence")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        event_types,
        [
            "thread.created",
            "thread.metadata-updated",
            "thread.metadata-updated",
            "provider-thread.updated",
            "run.created",
            "run-attempt.created",
            "node.updated",
            "message.updated",
            "turn-item.updated",
            "turn-item.updated",
        ]
    );
    let receipt: (u64, bool, bool) = database
        .query_row(
            "SELECT COUNT(*), receipt.result_sequence = ?1, receipt.accepted_at = event.occurred_at
         FROM orchestration_command_receipts AS receipt
         JOIN orchestration_events AS event ON event.sequence = receipt.result_sequence",
            [first.sequence],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(receipt, (1, true, true));
    let (status, preparation): (String, String) = database
        .query_row(
            "SELECT status, json_extract(payload_json, '$.workspacePreparation.type')
         FROM orchestration_v2_projection_runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "preparing");
    assert_eq!(preparation, "root");
    let (effects, id, effect): (u64, String, String) = database
        .query_row(
            "SELECT COUNT(*), effect_id, payload_json FROM orchestration_v2_effect_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(effects, 1);
    assert_eq!(id, "effect:command:launch:thread-title.generate");
    assert_eq!(
        serde_json::from_str::<Value>(&effect).unwrap(),
        json!({
            "type": "thread-title.generate",
            "kind": {"type":"initial", "messageId":"message:local"}
        })
    );
    let scope_count: u64 = database
        .query_row(
            "SELECT COUNT(*) FROM orchestration_v2_projection_checkpoint_scopes",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(scope_count, 0);
}

#[tokio::test]
async fn title_effect_failure_rolls_back_the_thread_and_first_run() {
    let fixture = Fixture::new();
    let database = Connection::open(&fixture.database).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER fail_title_effect BEFORE INSERT ON orchestration_v2_effect_outbox
         WHEN new.effect_type = 'thread-title.generate'
         BEGIN SELECT RAISE(ABORT, 'test title effect failure'); END;",
        )
        .unwrap();
    let plan = fixture.prepare(launch_request(), true).await.unwrap();
    assert!(plan.persist_initial_claim(true).is_err());
    for table in [
        "orchestration_events",
        "orchestration_command_receipts",
        "orchestration_v2_projection_threads",
        "orchestration_v2_projection_runs",
        "orchestration_v2_projection_messages",
        "orchestration_v2_turn_item_positions",
        "orchestration_v2_effect_outbox",
    ] {
        let count: u64 = database
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "rows left in {table}");
    }
}

#[tokio::test]
async fn new_claim_cannot_move_an_existing_message_to_a_different_thread() {
    let fixture = Fixture::new();
    let first = fixture.prepare(launch_request(), true).await.unwrap();
    first.persist_initial_claim(true).unwrap();
    let mut conflicting = launch_request();
    conflicting["commandId"] = json!("command:second");
    conflicting["threadId"] = json!("thread:second");
    let second = fixture.prepare(conflicting, true).await.unwrap();
    assert!(second.persist_initial_claim(true).is_err());
    let database = Connection::open(&fixture.database).unwrap();
    assert!(state::thread(&database, "thread:second").unwrap().is_none());
    let original_owner: String = database.query_row(
        "SELECT thread_id FROM orchestration_v2_projection_messages WHERE message_id = 'message:local'",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(original_owner, "thread:local");
}

#[tokio::test]
async fn committed_release_is_claimable_by_the_worker_and_replays_without_new_events() {
    let fixture = Fixture::new();
    let plan = fixture.prepare(launch_request(), true).await.unwrap();
    let claim = plan.persist_initial_claim(true).unwrap();
    let AfterClaim::Complete(commit) = plan.after_claim(claim).unwrap() else {
        panic!("The claim must commit its initial run.");
    };
    let preparation = plan.prepare_workspace(&commit).await.unwrap().unwrap();
    let run_id = commit.run_id.as_deref().unwrap();
    let effect_id = format!("effect:command:launch:release:provider-turn.start:{run_id}");
    let turns = super::super::turn_commands::TurnCommands::new(fixture.database.clone());
    assert!(turns.claim_start(&effect_id, "worker").unwrap().is_none());
    assert!(release::record_release(&plan, &commit, true).is_err());

    assert!(release::record_workspace(&plan, &commit, &preparation).unwrap());
    assert!(!release::record_workspace(&plan, &commit, &preparation).unwrap());
    assert!(turns.claim_start(&effect_id, "worker").unwrap().is_none());
    assert!(release::record_release(&plan, &commit, true).unwrap());
    assert!(!release::record_release(&plan, &commit, false).unwrap());
    let claim = turns.claim_start(&effect_id, "worker").unwrap().unwrap();
    assert_eq!(claim.thread.cwd, fixture.project);
    assert_eq!(claim.run_id, run_id);
    let database = Connection::open(&fixture.database).unwrap();
    let events: Vec<String> = database
        .prepare(
            "SELECT event_type FROM orchestration_events
          WHERE command_id = 'command:launch:release' ORDER BY sequence",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        events,
        [
            "checkpoint-scope.created",
            "node.updated",
            "turn-item.updated",
            "run.updated"
        ]
    );
    let receipt: (String, u64) = database
        .query_row(
            "SELECT command_type, result_sequence FROM orchestration_command_receipts
          WHERE command_id = 'command:launch:release'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(receipt, ("prepared-run.release".to_owned(), 15));
}

#[tokio::test]
async fn release_cannot_start_a_run_interrupted_after_workspace_metadata() {
    let fixture = Fixture::new();
    let plan = fixture.prepare(launch_request(), true).await.unwrap();
    let claim = plan.persist_initial_claim(true).unwrap();
    let AfterClaim::Complete(commit) = plan.after_claim(claim).unwrap() else {
        panic!("The claim must commit its initial run.");
    };
    let preparation = plan.prepare_workspace(&commit).await.unwrap().unwrap();
    assert!(release::record_workspace(&plan, &commit, &preparation).unwrap());
    let database = Connection::open(&fixture.database).unwrap();
    database
        .execute(
            "UPDATE orchestration_v2_projection_runs SET status = 'interrupted',
           payload_json = json_set(payload_json, '$.status', 'interrupted')",
            [],
        )
        .unwrap();
    assert!(release::record_release(&plan, &commit, true).is_err());
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .is_none()
    );
    let starts: u64 = database.query_row(
        "SELECT COUNT(*) FROM orchestration_v2_effect_outbox WHERE effect_type = 'provider-turn.start'",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(starts, 0);
}

#[tokio::test]
async fn launch_returns_the_committed_projection_and_wakes_the_worker_once() {
    use std::time::Duration;
    use tokio::sync::Notify;

    let fixture = Fixture::new();
    let scratch = fixture.folder.path().join("scratch");
    let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let track = updates.clone();
    let released = std::sync::Arc::new(Notify::new());
    let signal_release = released.clone();
    let result = launch(
        serde_json::from_value(launch_request()).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &scratch,
            settings_path: None,
            provider_enabled: true,
            terminals: None,
        },
        move |thread_id, released| {
            track.lock().unwrap().push((thread_id.to_owned(), released));
            if released {
                signal_release.notify_one();
            }
        },
    )
    .await
    .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["threadId"], "thread:local");
    assert_eq!(result["resumed"], false);
    assert_eq!(result["sequence"], 10);
    assert_eq!(result["projection"]["runs"][0]["status"], "preparing");
    tokio::time::timeout(Duration::from_secs(5), released.notified())
        .await
        .unwrap();
    let track = updates.clone();
    let replay = launch(
        serde_json::from_value(launch_request()).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &scratch,
            settings_path: None,
            provider_enabled: false,
            terminals: None,
        },
        move |thread_id, released| track.lock().unwrap().push((thread_id.to_owned(), released)),
    )
    .await
    .unwrap();
    let replay = serde_json::to_value(replay).unwrap();
    assert_eq!(replay["resumed"], true);
    assert_eq!(replay["sequence"], 10);
    assert_eq!(replay["projection"]["runs"][0]["status"], "starting");
    assert_eq!(
        *updates.lock().unwrap(),
        [
            ("thread:local".to_owned(), false),
            ("thread:local".to_owned(), false),
            ("thread:local".to_owned(), true),
        ]
    );
}

#[tokio::test]
async fn launch_finishes_release_after_the_caller_disconnects() {
    use std::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    use tokio::sync::Notify;

    let fixture = Fixture::new();
    let committed = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let resume = Arc::new((Mutex::new(false), Condvar::new()));
    let task_database = fixture.database.clone();
    let scratch = fixture.folder.path().join("scratch");
    let first = AtomicBool::new(true);
    let (signal_claim, signal_release, wait_for_resume) =
        (committed.clone(), released.clone(), resume.clone());
    let call = tokio::spawn(async move {
        launch(
            serde_json::from_value(launch_request()).unwrap(),
            LaunchContext {
                database: &task_database,
                scratch_root: &scratch,
                settings_path: None,
                provider_enabled: true,
                terminals: None,
            },
            move |_, released| {
                if released {
                    signal_release.notify_one();
                } else if first.swap(false, Ordering::SeqCst) {
                    signal_claim.notify_one();
                    let (lock, wait) = &*wait_for_resume;
                    drop(wait.wait_timeout_while(
                        lock.lock().unwrap(),
                        Duration::from_secs(5),
                        |ready| !*ready,
                    ));
                }
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), committed.notified())
        .await
        .unwrap();
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    *resume.0.lock().unwrap() = true;
    resume.1.notify_one();
    tokio::time::timeout(Duration::from_secs(5), released.notified())
        .await
        .unwrap();
    let database = Connection::open(&fixture.database).unwrap();
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .unwrap()
            .accepted
    );
}

#[tokio::test]
async fn launch_returns_before_a_preparation_failure_is_recorded() {
    use std::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::sync::Notify;

    let fixture = Fixture::new();
    let database = Connection::open(&fixture.database).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER block_launch_release
        BEFORE INSERT ON orchestration_events
        WHEN NEW.command_id = 'command:launch:release'
        BEGIN SELECT RAISE(ABORT, 'injected workspace failure'); END;",
        )
        .unwrap();
    let preparation = Arc::new((Mutex::new(false), Condvar::new()));
    let before_failure = preparation.clone();
    let failed = Arc::new(Notify::new());
    let signal_failure = failed.clone();
    let commits = AtomicUsize::new(0);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        launch(
            serde_json::from_value(launch_request()).unwrap(),
            LaunchContext {
                database: &fixture.database,
                scratch_root: &fixture.folder.path().join("scratch"),
                settings_path: None,
                provider_enabled: true,
                terminals: None,
            },
            move |_, released| {
                assert!(!released);
                match commits.fetch_add(1, Ordering::SeqCst) {
                    1 => {
                        let (lock, signal) = &*before_failure;
                        drop(signal.wait_timeout_while(
                            lock.lock().unwrap(),
                            Duration::from_secs(5),
                            |ready| !*ready,
                        ));
                    }
                    2 => signal_failure.notify_one(),
                    _ => (),
                }
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let before = serde_json::to_value(result).unwrap();
    assert_eq!(before["projection"]["runs"][0]["status"], "preparing");
    assert!(
        state::receipt(&database, "command:launch:fail")
            .unwrap()
            .is_none()
    );
    *preparation.0.lock().unwrap() = true;
    preparation.1.notify_one();
    tokio::time::timeout(Duration::from_secs(5), failed.notified())
        .await
        .unwrap();
    let receipt = state::receipt(&database, "command:launch:fail")
        .unwrap()
        .unwrap();
    assert!(receipt.accepted);
    assert_eq!(receipt.command_type, "prepared-run.fail");
    let sequence: u64 = database.query_row(
        "SELECT result_sequence FROM orchestration_command_receipts WHERE command_id = 'command:launch:fail'",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(sequence, 16);
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .is_none()
    );
    let item: String = database
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_turn_items WHERE type='error'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let item: Value = serde_json::from_str(&item).unwrap();
    assert_eq!(
        item["failure"],
        json!({
            "class": "validation_error",
            "message": "Workspace preparation failed during release run: injected workspace failure",
            "code": "workspace_preparation_failed",
            "retryable": false,
        })
    );
    let start: i64 = database.query_row("SELECT COUNT(*) FROM orchestration_v2_effect_outbox WHERE effect_type='provider-turn.start'", [], |row| row.get(0)).unwrap();
    assert_eq!(start, 0);
}

#[tokio::test]
async fn existing_worktree_start_and_checkpoint_use_the_bound_workspace() {
    use crate::server::turn_commands::TurnCommands;
    use crate::server::turn_commands::provider_turn::NativeTurnUpdate;
    use std::time::Duration;
    use t3_rune::providers::codex::{
        SettlementStatus, ThreadId, TurnId, TurnKey, TurnSnapshot, TurnStatus, TurnTokenUsage,
    };
    use tokio::sync::Notify;

    let fixture = Fixture::new();
    let run_git = |arguments: &[&str]| {
        let output = Command::new("git")
            .current_dir(&fixture.project)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_git(&["init", "-q", "-b", "fixture-main"]);
    run_git(&["commit", "--allow-empty", "-qm", "First commit"]);
    let worktree = fixture.folder.path().join("linked");
    run_git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "fixture-linked",
        worktree.to_str().unwrap(),
    ]);
    let ready = std::sync::Arc::new(Notify::new());
    let signal = ready.clone();
    let mut request = launch_request();
    request["workspaceStrategy"] = json!({"type":"existing_worktree","worktreePath": worktree});
    let accepted = launch(
        serde_json::from_value(request).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &fixture.folder.path().join("scratch"),
            settings_path: None,
            provider_enabled: true,
            terminals: None,
        },
        move |_, effect| {
            if effect {
                signal.notify_one();
            }
        },
    )
    .await
    .unwrap();
    let accepted = serde_json::to_value(accepted).unwrap();
    assert_eq!(
        accepted["projection"]["thread"]["worktreePath"],
        json!(worktree)
    );
    assert_eq!(accepted["projection"]["thread"]["branch"], Value::Null);
    tokio::time::timeout(Duration::from_secs(5), ready.notified())
        .await
        .unwrap();

    let sql = Connection::open(&fixture.database).unwrap();
    let restart = fixture.folder.path().join("restart.sqlite");
    sql.execute("VACUUM INTO ?1", [restart.to_str()]).unwrap();
    assert_eq!(
        TurnCommands::new(restart).recover_lost_starts().unwrap(),
        ["thread:local"]
    );
    let run = accepted["projection"]["runs"][0]["id"].as_str().unwrap();
    let effect_id = format!("effect:command:launch:release:provider-turn.start:{run}");
    let commands = TurnCommands::new(fixture.database.clone());
    let set_scope = |cwd: &Path| {
        sql.execute(
            "UPDATE orchestration_v2_projection_checkpoint_scopes
                    SET payload_json=json_set(payload_json, '$.cwd',?1)",
            [cwd.to_str()],
        )
        .unwrap();
    };
    set_scope(&fixture.project);
    assert!(
        commands
            .claim_start(&effect_id, "worker")
            .unwrap()
            .is_none()
    );
    set_scope(&worktree);
    let claim = commands.claim_start(&effect_id, "worker").unwrap().unwrap();
    assert_eq!(claim.turn.cwd, worktree);
    commands.capture_baseline(&claim).await.unwrap();

    let key = TurnKey {
        thread_id: ThreadId::new("native-thread"),
        turn_id: TurnId::new("native-turn"),
    };
    commands.record_session_ready(&claim, false).unwrap();
    commands
        .bind_native_thread(&claim, &key.thread_id)
        .unwrap()
        .unwrap();
    assert!(
        commands
            .accept_provider_start(
                &claim,
                &TurnSnapshot {
                    key: key.clone(),
                    status: TurnStatus::Running,
                    started_at: None,
                    completed_at: None,
                    error: None,
                }
            )
            .unwrap()
    );
    commands
        .record_provider_turn(
            "thread:local",
            run,
            &key,
            NativeTurnUpdate::Started { started_at: None },
        )
        .unwrap()
        .unwrap();
    fs::write(
        worktree.join("linked.txt"),
        "Present only in the linked worktree.\n",
    )
    .unwrap();
    let usage: TurnTokenUsage = serde_json::from_value(json!({
        "usageScope":"main_agent","usageStatus":"complete","hasSubagents":false,"models":[],
        "inputTokens":0,"cachedInputTokens":0,"cacheCreationTokens":0,"outputTokens":0,"reasoningOutputTokens":0,
    })).unwrap();
    commands
        .record_provider_turn(
            "thread:local",
            run,
            &key,
            NativeTurnUpdate::Settled {
                status: SettlementStatus::Completed,
                completed_at: None,
                usage: &usage,
            },
        )
        .unwrap()
        .unwrap();
    commands
        .await_checkpoint("thread:local", run, &key)
        .unwrap()
        .unwrap();
    commands
        .capture_checkpoint(&format!("effect:checkpoint.capture:{run}"), "worker")
        .await
        .unwrap()
        .unwrap();
    let summary: String = sql.query_row("SELECT payload_json FROM orchestration_v2_projection_checkpoints WHERE ordinal_within_scope = 1", [], |row| row.get(0)).unwrap();
    let summary: Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["status"], "ready");
    assert_eq!(summary["files"][0]["path"], "linked.txt");
    assert!(!fixture.project.join("linked.txt").exists());
}

#[tokio::test]
async fn a_worktree_setup_script_does_not_get_silently_skipped() {
    let fixture = Fixture::new();
    let worktree = fixture.folder.path().join("linked");
    fs::create_dir(&worktree).unwrap();
    let settings = fixture.folder.path().join("settings.json");
    fs::write(&settings, r#"
        { "defaultProjectScripts": [{"id":"setup","name":"Prepare","command":"echo prepared","icon":"play","runOnWorktreeCreate":true}] }
    "#).unwrap();
    let mut request = launch_request();
    request["workspaceStrategy"] = json!({"type":"existing_worktree","worktreePath":worktree});
    let result = launch(
        serde_json::from_value(request).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &fixture.folder.path().join("scratch"),
            settings_path: Some(&settings),
            provider_enabled: true,
            terminals: None,
        },
        |_, _| panic!("No launch should commit when automatic setup cannot run"),
    )
    .await;
    let error = result.err().unwrap();
    assert!(error.to_string().contains("automatic setup scripts"));
    let sql = Connection::open(&fixture.database).unwrap();
    assert!(state::receipt(&sql, "command:launch").unwrap().is_none());
}

#[cfg(unix)]
fn checkout_hook(project: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let hook = project.join(".git/hooks/post-checkout");
    fs::write(&hook, script).unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).unwrap();
}

fn local_git(project: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(project)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
        ])
        .args(arguments)
        .output()
        .unwrap()
}

fn git_succeeded(project: &Path, arguments: &[&str]) {
    let output = local_git(project, arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_new_worktree_uses_checkout_configuration_and_the_project_branch_prefix() {
    use std::time::Duration;

    let fixture = Fixture::new();
    git_succeeded(&fixture.project, &["init", "-q", "-b", "main"]);
    git_succeeded(&fixture.project, &["config", "checkout.workers", "2"]);
    fs::create_dir(fixture.project.join("src")).unwrap();
    fs::write(fixture.project.join("src/file.rs"), "fn main() {}\n").unwrap();
    fs::write(
        fixture.project.join("other.txt"),
        "outside the sparse checkout\n",
    )
    .unwrap();
    fs::write(fixture.project.join("patterns.txt"), "/src/\n").unwrap();
    fs::write(
        fixture.project.join("t3.json"),
        r#"{"worktreeSparsePatternsPath":"patterns.txt"}"#,
    )
    .unwrap();
    git_succeeded(&fixture.project, &["add", "--all"]);
    git_succeeded(&fixture.project, &["commit", "-qm", "Initial"]);
    checkout_hook(
        &fixture.project,
        "#!/bin/sh\ncommon=$(git rev-parse --path-format=absolute --git-common-dir)\ngit config --get checkout.workers > \"$common/worker-count\"\n",
    );
    let settings = fixture.folder.path().join("settings.json");
    let worktrees = fixture.folder.path().join("managed-checkouts");
    fs::write(
        &settings,
        json!({
            "worktreesDirectory": worktrees,
            "branchNamingMode":"static",
            "branchNamePrefix":"global",
            "projectSettingsOverrides":{"project:local":{"branchNamePrefix":"Dev Team/Work--Tree"}}
        })
        .to_string(),
    )
    .unwrap();
    let mut request = launch_request();
    request.as_object_mut().unwrap().remove("initialMessage");
    request["workspaceStrategy"] = json!({"type":"worktree","baseRef":"main"});
    let result = launch(
        serde_json::from_value(request).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &fixture.folder.path().join("scratch"),
            settings_path: Some(&settings),
            provider_enabled: true,
            terminals: None,
        },
        |_, _| {},
    )
    .await
    .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert!(result["projection"]["thread"]["worktreePath"].is_null());
    let checkout = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let database = Connection::open(&fixture.database).unwrap();
            let thread = state::thread(&database, "thread:local").unwrap().unwrap();
            if let (Some(path), Some(branch)) = (thread.worktree_path, thread.branch) {
                let configured = local_git(
                    &fixture.project,
                    &["config", "--get", &format!("branch.{branch}.t3-merge-base")],
                );
                if configured.status.success() {
                    assert!(branch.starts_with("Dev-Team/Work-Tree/"));
                    let suffix = branch.rsplit('/').next().unwrap();
                    assert_eq!(suffix.len(), 8);
                    assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
                    assert_eq!(configured.stdout.trim_ascii(), b"main");
                    break path;
                }
            }
            drop(database);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(checkout.starts_with(&worktrees));
    assert!(checkout.join("src/file.rs").is_file());
    assert!(!checkout.join("other.txt").exists());
    assert!(fixture.project.join("other.txt").is_file());
    assert_eq!(
        fs::read_to_string(fixture.project.join(".git/worker-count"))
            .unwrap()
            .trim(),
        "2"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_new_worktree_terminates_its_checkout_and_prevents_release() {
    use std::time::Duration;

    let fixture = Fixture::new();
    git_succeeded(&fixture.project, &["init", "-q", "-b", "main"]);
    git_succeeded(
        &fixture.project,
        &["commit", "--allow-empty", "-qm", "Initial"],
    );
    checkout_hook(
        &fixture.project,
        "#!/bin/sh\ncommon=$(git rev-parse --path-format=absolute --git-common-dir)\ntouch \"$common/checkout-started\"\nsleep 12\ntouch \"$common/checkout-finished\"\n",
    );
    let scratch = fixture.folder.path().join("scratch");
    let mut request = launch_request();
    request["workspaceStrategy"] = json!({"type":"worktree","baseRef":"main","branch":"feature"});
    let result = launch(
        serde_json::from_value(request).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &scratch,
            settings_path: None,
            provider_enabled: true,
            terminals: None,
        },
        |_, released| assert!(!released),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap()["projection"]["runs"][0]["status"],
        "preparing"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.project.join(".git/checkout-started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            cancel_worktree_setup(&fixture.database, "thread:local")
        )
        .await
        .unwrap()
    );
    assert!(!fixture.project.join(".git/checkout-finished").exists());
    assert!(
        !local_git(
            &fixture.project,
            &["show-ref", "--verify", "--quiet", "refs/heads/feature"]
        )
        .status
        .success()
    );
    let database = Connection::open(&fixture.database).unwrap();
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .is_none()
    );
    assert!(
        state::receipt(&database, "command:launch:fail")
            .unwrap()
            .unwrap()
            .accepted
    );
    let item: String = database
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_turn_items WHERE type='error'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&item).unwrap()["failure"]["message"],
        "Workspace preparation failed: Worktree setup cancelled."
    );
    assert!(!cancel_worktree_setup(&fixture.database, "thread:local").await);
}

#[cfg(unix)]
#[tokio::test]
async fn a_generated_worktree_renames_the_owned_checkout_before_starting_codex() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::sync::Notify;
    let fixture = Fixture::new();
    git_succeeded(&fixture.project, &["init", "-q", "-b", "main"]);
    git_succeeded(
        &fixture.project,
        &["commit", "--allow-empty", "-qm", "Initial"],
    );
    git_succeeded(&fixture.project, &["branch", "Dev-Team/new-cafe-ui"]);
    let codex = fixture.folder.path().join("codex-fixture");
    let codex_home = fixture.folder.path().join("codex-home");
    fs::create_dir(&codex_home).unwrap();
    fs::write(
        &codex,
        r#"#!/bin/sh
if [ "$1" != "exec" ]; then
  printf '[]'
  exit 0
fi
common=$(git rev-parse --path-format=absolute --git-common-dir)
printf '%s\n' "$@" > "$common/branch-argv"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-last-message) shift; output=$1 ;;
    --output-schema) shift; cp "$1" "$common/branch-schema" ;;
  esac
  shift
done
cat > "$common/branch-prompt"
printf '%s' '{"branch":"New Café UI"}' > "$output"
"#,
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let settings = fixture.folder.path().join("settings.json");
    fs::write(&settings, json!({
        "branchNamingMode":"static",
        "branchNamePrefix":"global",
        "providerInstances":{"codex":{
            "driver":"codex","label":"Codex","enabled":true,
            "config":{"binaryPath":codex,"homePath":codex_home}
        }},
        "projectSettingsOverrides":{"project:local":{
            "branchNamePrefix":"Dev Team",
            "sourceControlWriterModelSelection":{"instanceId":"codex","model":"gpt-5.4-mini","options":[{"id":"reasoningEffort","value":"medium"}]}
        }}
    }).to_string()).unwrap();
    let mut request = launch_request();
    request["workspaceStrategy"] = json!({"type":"worktree","baseRef":"main"});
    let ready = std::sync::Arc::new(Notify::new());
    let notify = ready.clone();
    let initial = launch(
        serde_json::from_value(request).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &fixture.folder.path().join("scratch"),
            settings_path: Some(&settings),
            provider_enabled: true,
            terminals: None,
        },
        move |_, released| {
            if released {
                notify.notify_one();
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(initial).unwrap()["projection"]["thread"]["branch"],
        Value::Null
    );
    tokio::time::timeout(Duration::from_secs(10), ready.notified())
        .await
        .unwrap();
    let database = Connection::open(&fixture.database).unwrap();
    let thread = state::thread(&database, "thread:local").unwrap().unwrap();
    let branch = thread.branch.unwrap();
    let path = thread.worktree_path.unwrap();
    assert_eq!(branch, "Dev-Team/new-cafe-ui-1");
    assert_eq!(
        local_git(&path, &["symbolic-ref", "--short", "HEAD"])
            .stdout
            .trim_ascii(),
        branch.as_bytes()
    );
    assert!(
        state::receipt(&database, "command:launch:branch-rename")
            .unwrap()
            .unwrap()
            .accepted
    );
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .unwrap()
            .accepted
    );
    let args = fs::read_to_string(fixture.project.join(".git/branch-argv")).unwrap();
    assert!(
        args.contains("--ephemeral\n--skip-git-repo-check\n-s\nread-only\n--model\ngpt-5.4-mini\n")
    );
    assert!(args.contains("model_reasoning_effort=\"medium\""));
    let prompt = fs::read_to_string(fixture.project.join(".git/branch-prompt")).unwrap();
    assert!(prompt.ends_with("User message:\n  Read the project.\n"));
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(fixture.project.join(".git/branch-schema")).unwrap(),
    )
    .unwrap();
    assert_eq!(schema["required"], json!(["branch"]));
}

#[tokio::test]
async fn a_replayed_checkout_recovers_a_git_rename_before_its_receipt() {
    use std::time::Duration;
    use tokio::sync::{Notify, watch};

    let fixture = Fixture::new();
    git_succeeded(&fixture.project, &["init", "-q", "-b", "main"]);
    git_succeeded(
        &fixture.project,
        &["commit", "--allow-empty", "-qm", "Initial"],
    );
    let mut request = launch_request();
    request["workspaceStrategy"] = json!({"type":"worktree","baseRef":"main"});
    let plan = fixture.prepare(request.clone(), true).await.unwrap();
    let initial = plan.persist_initial_claim(true).unwrap();
    let AfterClaim::Complete(commit) = plan.after_claim(initial).unwrap() else {
        panic!("The launch must persist a preparing run");
    };
    assert!(release::record_progress(&plan, &commit, release::ProgressPhase::Worktree).unwrap());
    let (_cancel, mut cancelled) = watch::channel(false);
    let mut checkout = worktree::LocalWorktree::acquire(
        &plan.input,
        &plan.input.workspace_strategy,
        &fixture.project,
        &plan.default_worktrees_root,
        None,
        &mut cancelled,
        None,
    )
    .await
    .unwrap();
    let path = checkout.path().to_owned();
    let expected = checkout.branch().to_owned();
    let pending = PendingWorkspace {
        workspace_root: fixture.project.clone(),
        strategy: WorkspaceStrategy::ExistingWorktree {
            branch: Some(Trimmed(expected.clone())),
            worktree_path: Trimmed(path.to_str().unwrap().to_owned()),
        },
        checkout: None,
    };
    let workspace = plan.workspace_preparation(&commit, pending).await;
    assert!(release::record_workspace(&plan, &commit, &workspace).unwrap());
    checkout
        .rename_branch("already-renamed", true)
        .await
        .unwrap();
    drop(checkout);
    let ready = std::sync::Arc::new(Notify::new());
    let signal = ready.clone();
    let result = launch(
        serde_json::from_value(request).unwrap(),
        LaunchContext {
            database: &fixture.database,
            scratch_root: &fixture.folder.path().join("scratch"),
            settings_path: None,
            provider_enabled: true,
            terminals: None,
        },
        move |_, released| {
            if released {
                signal.notify_one();
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(result).unwrap()["resumed"], true);
    tokio::time::timeout(Duration::from_secs(5), ready.notified())
        .await
        .unwrap();
    let database = Connection::open(&fixture.database).unwrap();
    let thread = state::thread(&database, "thread:local").unwrap().unwrap();
    assert_eq!(thread.branch.as_deref(), Some("already-renamed"));
    assert_eq!(thread.worktree_path.as_deref(), Some(path.as_path()));
    assert!(
        state::receipt(&database, "command:launch:branch-rename")
            .unwrap()
            .unwrap()
            .accepted
    );
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .unwrap()
            .accepted
    );
}

#[tokio::test]
async fn a_reserved_checkout_requires_proof_of_completed_git_work_before_adoption() {
    use tokio::sync::watch;

    let fixture = Fixture::new();
    git_succeeded(&fixture.project, &["init", "-q", "-b", "main"]);
    git_succeeded(
        &fixture.project,
        &["commit", "--allow-empty", "-qm", "Initial"],
    );
    let mut request = launch_request();
    request["workspaceStrategy"] =
        json!({"type":"worktree","baseRef":"main","branch":"owned-branch"});
    let plan = fixture.prepare(request, true).await.unwrap();
    let (_cancel, cancelled) = watch::channel(false);
    let acquire = || async {
        let mut receiver = cancelled.clone();
        worktree::LocalWorktree::acquire(
            &plan.input,
            &plan.input.workspace_strategy,
            &fixture.project,
            &plan.default_worktrees_root,
            None,
            &mut receiver,
            None,
        )
        .await
    };
    let checkout = acquire().await.unwrap();
    let path = checkout.path().to_owned();
    drop(checkout);
    assert_eq!(acquire().await.unwrap().path(), path);
    let admin = local_git(&path, &["rev-parse", "--absolute-git-dir"]);
    let admin = PathBuf::from(String::from_utf8(admin.stdout).unwrap().trim());
    fs::remove_file(admin.join("t3-launch-checkout-ready")).unwrap();
    let error = acquire().await.err().unwrap();
    assert!(
        error
            .to_string()
            .contains("reserved worktree checkout did not finish")
    );
    assert!(path.is_dir());
    assert!(
        local_git(
            &fixture.project,
            &["show-ref", "--verify", "--quiet", "refs/heads/owned-branch"]
        )
        .status
        .success()
    );
}

#[tokio::test]
async fn an_owned_remote_checkout_keeps_its_fetched_base_after_a_restart() {
    use tokio::sync::watch;

    let fixture = Fixture::new();
    let project = &fixture.project;
    let remote = fixture.folder.path().join("origin.git");
    let remote_path = remote.to_str().unwrap();
    git_succeeded(project, &["init", "-q", "-b", "main"]);
    git_succeeded(project, &["commit", "--allow-empty", "-qm", "Initial"]);
    git_succeeded(
        project,
        &["init", "--bare", "-q", "-b", "main", remote_path],
    );
    git_succeeded(project, &["remote", "add", "origin", remote_path]);
    git_succeeded(project, &["push", "-qu", "origin", "main"]);

    let git_value = |cwd: &Path, arguments: &[&str]| {
        let output = local_git(cwd, arguments);
        assert!(output.status.success(), "{:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let checkout_original_commit = git_value(project, &["rev-parse", "HEAD"]);
    let advance_remote = |parent: &str, message: &str| {
        let commit = git_value(
            project,
            &["commit-tree", "HEAD^{tree}", "-p", parent, "-m", message],
        );
        git_succeeded(
            &remote,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                project.to_str().unwrap(),
                &format!("{commit}:refs/heads/main"),
            ],
        );
        commit
    };
    let fetched_commit = advance_remote(&checkout_original_commit, "First remote change");
    let mut request = launch_request();
    request["workspaceStrategy"] = json!({
        "type":"worktree", "baseRef":"main", "branch":"owned-branch", "startFromOrigin":true
    });
    let plan = fixture.prepare(request, true).await.unwrap();
    let acquire = || async {
        let (_cancel, mut cancelled) = watch::channel(false);
        worktree::LocalWorktree::acquire(
            &plan.input,
            &plan.input.workspace_strategy,
            project,
            &plan.default_worktrees_root,
            None,
            &mut cancelled,
            None,
        )
        .await
        .unwrap()
    };
    let checkout_path = acquire().await.path().to_owned();
    assert_eq!(
        git_value(&checkout_path, &["rev-parse", "HEAD"]),
        fetched_commit
    );
    let moved = advance_remote(&fetched_commit, "Second remote change");
    assert_eq!(git_value(&remote, &["rev-parse", "main"]), moved);

    let resumed = acquire().await;
    assert_eq!(resumed.path(), checkout_path);
    assert_eq!(
        git_value(resumed.path(), &["rev-parse", "HEAD"]),
        fetched_commit
    );
    assert_eq!(
        git_value(project, &["rev-parse", "origin/main"]),
        fetched_commit
    );
    assert_eq!(
        git_value(project, &["rev-parse", "HEAD"]),
        checkout_original_commit
    );
}
