use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::*;

fn report() -> Box<Report> {
    serde_json::from_value(json!({
        "trigger": "manual",
        "startedAt": "2026-11-22T20:41:49.872Z",
        "finishedAt": "2026-11-22T20:41:54.415Z",
        "entries": [],
        "counts": {"removed": 0, "kept": 0, "failed": 1},
        "omittedCount": 0,
        "bytesFreed": 0
    }))
    .unwrap()
}

#[test]
fn open_view_keeps_report_folded_or_expanded_while_manual_run_is_pending() {
    let view = Uuid::new_v4();
    let mut panel = Panel {
        writable: Some(true),
        view: Some(view),
        rules: true,
        ..Panel::default()
    };
    let now = OffsetDateTime::parse("2026-11-22T20:42:00Z", &Rfc3339).unwrap();
    panel.report_received(view, Ok(None));
    assert!(panel.may_run());
    assert_eq!(panel.last_run.description(now), "Loading last run…");
    panel.report_received(view, Ok(Some(report())));
    assert!(!panel.expanded);
    panel.expanded = true;
    panel.run_pending = true;
    assert!(!panel.may_run());
    assert_eq!(
        panel.last_run.description(now),
        "Last run just now. 0 B recovered. 1 item couldn't be removed."
    );
    panel.report_received(Uuid::new_v4(), Err("closed view".into()));
    assert!(panel.expanded);
    assert!(matches!(panel.last_run, LastRun::Report(_)));
    panel.report_received(view, Ok(Some(report())));
    assert!(panel.expanded);
}
