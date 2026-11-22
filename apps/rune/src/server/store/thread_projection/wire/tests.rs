use super::*;

#[test]
fn wire_projection_filters_command_diff_handoff_and_tool_records_in_both_lists() {
    let command = json!({
        "id":"cmd", "threadId":"thread", "type":"command_execution", "exitCode":0,
        "input":"test", "output":"ENOENT included as successful search result",
    });
    let tool = json!({
        "id":"tool", "threadId":"thread", "type":"dynamic_tool", "input":{"argument":"body"},
        "output":{"structuredContent":{"threadId":"created","secret":"raw-body"}},
    });
    let mut projection = json!({
        "checkpoints":[],
        "contextHandoffs":[{"id":"handoff", "summaryText":"raw-body", "history":[1,2], "delivery":{}}],
        "turnItems":[
            command, tool,
            {"id":"file", "threadId":"thread", "type":"file_change", "status":"completed","diffStr":"raw-body","oldStr":"raw-body","newStr":"raw-body"},
            {"id":"marker", "threadId":"thread", "type":"handoff", "summary":"raw-body"},
        ],
        "visibleTurnItems":[{"item":command},{"item":tool}],
    });
    project(&mut projection).unwrap();
    assert_eq!(projection["turnItems"][0]["outputOmitted"], true);
    assert!(
        projection["turnItems"][0]
            .get("outputIndicatesFailure")
            .is_none()
    );
    assert!(projection["turnItems"][0].get("output").is_none());
    assert_eq!(
        projection["turnItems"][1]["output"],
        json!({"threadId":"created"})
    );
    assert_eq!(projection["turnItems"][1]["outputOmitted"], true);
    for position in [0, 1] {
        assert_eq!(
            projection["turnItems"][position],
            projection["visibleTurnItems"][position]["item"]
        );
    }
    assert_eq!(
        projection["contextHandoffs"],
        json!([{"id":"handoff","summaryText":""}])
    );
    assert!(!projection.to_string().contains("raw-body"));
}

#[test]
fn wire_checkpoint_prefix_keeps_totals_and_tool_summary_keeps_utf16_boundaries() {
    let file = json!({"path":"test", "kind":"modified", "additions":3, "deletions":1});
    let checkpoint = json!({"files":vec![file; 201]});
    let input = format!("  {}😀{}\nnext", "x".repeat(158), "z".repeat(16_400));
    let mut projection = json!({
        "checkpoints":[checkpoint], "contextHandoffs":[], "visibleTurnItems":[],
        "turnItems":[{"id":"tool","threadId":"thread","type":"dynamic_tool","input":input}],
    });
    project(&mut projection).unwrap();
    assert_eq!(
        projection["checkpoints"][0]["files"]
            .as_array()
            .unwrap()
            .len(),
        200
    );
    assert_eq!(
        projection["checkpoints"][0]["filesTotal"],
        json!({"count":201,"additions":603,"deletions":201})
    );
    assert_eq!(
        projection["turnItems"][0]["input"],
        json!({"summary":format!("{}…", "x".repeat(158)),"truncated":true})
    );
}

#[test]
fn recorded_failure_hint_and_exit_code_precedence_match_projected_commands() {
    for (exit, output, expected) in [
        (None, "exit code : 2", true),
        (None, "exit code 1x", false),
        (None, "EXITED WITH EXIT CODE 1x", true),
        (Some(0), "no such file", false),
        (Some(1), " ", true),
    ] {
        let mut command = json!({"type":"command_execution","output":output});
        if let Some(exit) = exit {
            command["exitCode"] = exit.into();
        }
        project_item(&mut command).unwrap();
        assert_eq!(
            command["outputIndicatesFailure"] == true,
            expected,
            "{exit:?} {output:?}"
        );
        assert_eq!(command["outputOmitted"] == true, !output.trim().is_empty());
        assert!(command.get("output").is_none());
    }
}
