use super::*;

#[tokio::test]
async fn codex_launch_persists_reasoning_effort_before_claiming_the_provider_start() {
    let fixture = Fixture::new();
    let selection = json!({
        "instanceId":"codex","model":"gpt-5.4",
        "options":[{"id":"reasoningEffort","value":"high"}]
    });
    let mut request = launch_request();
    request["modelSelection"] = selection.clone();
    let plan = fixture.prepare(request, true).await.unwrap();
    assert_eq!(plan.claim_command()["modelSelection"], selection);
    let claim = plan.persist_initial_claim(true).unwrap();
    let AfterClaim::Complete(commit) = plan.after_claim(claim).unwrap() else {
        panic!("Launch must commit the initial run");
    };
    let preparation = plan.prepare_workspace(&commit).await.unwrap().unwrap();
    let run_id = commit.run_id.as_deref().unwrap();
    let effect_id = format!("effect:command:launch:release:provider-turn.start:{run_id}");
    let turns = super::super::super::turn_commands::TurnCommands::new(fixture.database.clone());
    assert!(turns.claim_start(&effect_id, "worker").unwrap().is_none());
    assert!(release::record_workspace(&plan, &commit, &preparation).unwrap());
    assert!(release::record_release(&plan, &commit, true).unwrap());
    let claimed = turns.claim_start(&effect_id, "worker").unwrap().unwrap();
    assert_eq!(claimed.turn.model, "gpt-5.4");
    assert_eq!(claimed.turn.reasoning_effort.as_deref(), Some("high"));

    let database = Connection::open(&fixture.database).unwrap();
    let stored: String = database
        .query_row(
            "SELECT json_extract(payload_json, '$.modelSelection')
         FROM orchestration_v2_projection_runs WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), selection);
}

#[tokio::test]
async fn codex_reasoning_choice_does_not_admit_other_model_options() {
    let fixture = Fixture::new();
    for (model, option, value) in [
        ("gpt-5.4", "reasoningEffort", "unsupported"),
        ("gpt-5.4", "serviceTier", "high"),
        ("gpt-5.5", "reasoningEffort", "ultra"),
    ] {
        let mut request = launch_request();
        request["modelSelection"] = json!({
            "instanceId":"codex","model":model,"options":[{"id":option,"value":value}]
        });
        let plan = fixture.prepare(request, true).await.unwrap();
        assert!(plan.persist_initial_claim(true).is_err());
    }
}
