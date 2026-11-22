use crate::server::turn_commands::TurnCommands;

use super::*;

async fn prepared(fixture: &Fixture) -> (PreparedLaunch, LaunchCommit) {
    let mut request = launch_request();
    request["modelSelection"] = json!({
        "instanceId": "claudeAgent",
        "model": "claude-sonnet-4-6"
    });
    request["generateTitle"] = json!(false);
    let mut plan = fixture.prepare(request, true).await.unwrap();
    let settings = fixture.folder.path().join("settings.json");
    fs::write(&settings, "{}").unwrap();
    plan.settings_path = Some(settings);
    let claim = plan.persist_initial_claim(true).unwrap();
    assert_eq!(claim.sequence, 9);
    let AfterClaim::Complete(commit) = plan.after_claim(claim).unwrap() else {
        panic!("The claim must commit the initial Claude run.");
    };
    (plan, commit)
}

#[tokio::test]
async fn claude_starts_only_after_the_root_workspace_release() {
    let fixture = Fixture::new();
    let (plan, commit) = prepared(&fixture).await;
    let run_id = commit.run_id.as_deref().unwrap();
    let effect_id = format!("effect:command:launch:release:provider-turn.start:{run_id}");
    let turns = TurnCommands::new(fixture.database.clone());
    let preparation = plan.prepare_workspace(&commit).await.unwrap().unwrap();
    assert!(
        turns
            .claim_claude_start(&effect_id, "worker")
            .unwrap()
            .is_none()
    );
    assert!(release::record_workspace(&plan, &commit, &preparation).unwrap());
    assert!(
        turns
            .claim_claude_start(&effect_id, "worker")
            .unwrap()
            .is_none()
    );
    assert!(release::record_release(&plan, &commit, true).unwrap());
    let result = turns
        .claim_claude_start(&effect_id, "worker")
        .unwrap()
        .unwrap();
    assert_eq!(result.workspace, fixture.project);
    assert_eq!(result.model, "claude-sonnet-4-6");
    let database = Connection::open(&fixture.database).unwrap();
    let namespaced: bool = database
        .query_row(
            "SELECT turn_item_id LIKE 'turn-item:provider:claudeAgent:native-item:%'
         FROM orchestration_v2_projection_turn_items WHERE run_id=?1 AND type='command_execution'",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(namespaced);
    assert_eq!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .unwrap()
            .command_type,
        "prepared-run.release"
    );
}

#[tokio::test]
async fn disabling_claude_while_preparing_records_failure_without_start() {
    let fixture = Fixture::new();
    let (plan, commit) = prepared(&fixture).await;
    let workspace = plan.prepare_workspace(&commit).await.unwrap().unwrap();
    assert!(release::record_workspace(&plan, &commit, &workspace).unwrap());
    fs::write(
        plan.settings_path.as_ref().unwrap(),
        json!({
            "projectSettingsOverrides": {
                "project:local": {"providerInstances": {"claudeAgent": {"enabled": false}}}
            }
        })
        .to_string(),
    )
    .unwrap();
    let error = release::record_release(&plan, &commit, true).unwrap_err();
    let message = error.preparation_failure_message();
    assert_eq!(
        message,
        "Workspace preparation failed during release run: Provider instance \"claudeAgent\" is disabled for project project:local."
    );
    assert!(release::record_failure(&plan, &commit, &message).unwrap());
    let database = Connection::open(&fixture.database).unwrap();
    assert!(
        state::receipt(&database, "command:launch:release")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        state::receipt(&database, "command:launch:fail")
            .unwrap()
            .unwrap()
            .command_type,
        "prepared-run.fail"
    );
    let (status, errors, outbox): (String, usize, usize) = database.query_row(
        "SELECT run.status,
            (SELECT COUNT(*) FROM orchestration_v2_projection_turn_items
             WHERE type='error' AND turn_item_id LIKE 'turn-item:provider:claudeAgent:native-item:%'),
            (SELECT COUNT(*) FROM orchestration_v2_effect_outbox)
         FROM orchestration_v2_projection_runs AS run",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!((status.as_str(), errors, outbox), ("failed", 1, 0));
}
