use super::report::Trigger;
use super::*;

fn failed_report() -> Value {
    json!({
        "trigger": "manual",
        "startedAt": "2026-11-22T20:41:49.872Z",
        "finishedAt": "2026-11-22T20:41:54.415Z",
        "entries": [{
            "kind": "worktree",
            "outcome": "failed",
            "reason": "git worktree remove: fatal: cannot remove a locked working tree",
            "path": "/managed/backup-worktree",
            "threadId": "thread",
            "threadTitle": "Paused thread"
        }],
        "counts": {"removed": 0, "kept": 0, "failed": 1},
        "omittedCount": 0
    })
}

#[test]
fn report_contract_distinguishes_required_nulls_from_defaults() {
    let value = failed_report();
    let report: Report = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(report._trigger, Trigger::Manual);
    assert_eq!(report.bytes_freed, 0.0);
    let entry = &report.entries[0];
    assert_eq!(entry.kind, Kind::Worktree);
    assert_eq!(entry.outcome, Outcome::Failed);
    assert_eq!(entry.project_path, None);
    assert_eq!(entry.bytes, None);
    assert_eq!(entry.files, None);
    let mut incomplete = value;
    incomplete["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("path");
    assert!(serde_json::from_value::<Report>(incomplete.clone()).is_err());
    incomplete["entries"][0]["path"] = Value::Null;
    assert!(serde_json::from_value::<Report>(incomplete).is_ok());
}

#[test]
fn cleanup_rule_availability_uses_environment_and_custom_project_rules() {
    let mut settings = json!({"storageCleanup": {"worktreeOnDelete": true}});
    assert!(has_rules(&settings));
    settings["worktreeCleanup"] = json!({"mode": "off"});
    assert!(!has_rules(&settings));
    settings["projectSettingsOverrides"] = json!({"example": {"worktreeCleanup": {
        "mode": "custom", "rules": {"worktreeSettledAfterDays": 0}
    }}});
    assert!(has_rules(&settings));
    settings["projectSettingsOverrides"]["example"]["worktreeCleanup"] = json!({"mode": "off"});
    assert!(!has_rules(&settings));
    settings["storageCleanup"]["logsAfterDays"] = json!(5);
    assert!(has_rules(&settings));
}

#[test]
fn nullable_stream_and_failed_entries_in_success_exit_stay_distinct() {
    let (event_tx, receiver) = std::sync::mpsc::channel();
    let events = EventSender {
        event_tx,
        notify: || {},
    };
    let subscription = Uuid::new_v4();
    let mut client = Client::new(true, true);
    client.report = Some((subscription, "18".into()));
    client.run = Some("19".into());
    assert!(client.chunk(vec![Value::Null, failed_report()], &events));
    client.exit("19", Ok(failed_report()), &events);
    assert!(
        matches!(receiver.try_recv().unwrap(), Event::StorageCleanup(Update::Report {
        view, result: Ok(None)
    }) if view == subscription)
    );
    assert!(
        matches!(receiver.try_recv().unwrap(), Event::StorageCleanup(Update::Report {
        view, result: Ok(Some(report))
    }) if view == subscription && report.entries[0].outcome == Outcome::Failed)
    );
    assert!(
        matches!(receiver.try_recv().unwrap(), Event::StorageCleanup(Update::Finished(Ok(report)))
        if report.counts.failed == 1.0)
    );
    assert!(client.receives("18"));
    assert!(!client.receives_exit("19"));
}
