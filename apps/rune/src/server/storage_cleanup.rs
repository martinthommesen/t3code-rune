pub(super) mod files;

use std::sync::Arc;

use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, watch};

use super::turn_commands::timestamp;

const REPORT_ENTRY_LIMIT: usize = 200;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Trigger {
    Automatic,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Kind {
    Worktree,
    BrowserArtifacts,
    Logs,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Outcome {
    Removed,
    Kept,
    Failed,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Entry {
    pub(super) kind: Kind,
    pub(super) outcome: Outcome,
    pub(super) reason: String,
    pub(super) path: Option<String>,
    pub(super) project_path: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) thread_title: Option<String>,
    pub(super) bytes: Option<u64>,
    pub(super) files: Option<u64>,
}

#[derive(Default, Serialize)]
struct Counts {
    removed: usize,
    kept: usize,
    failed: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Report {
    trigger: Trigger,
    started_at: String,
    finished_at: String,
    entries: Vec<Entry>,
    counts: Counts,
    omitted_count: usize,
    bytes_freed: u64,
}

impl Report {
    pub(super) fn finish(
        trigger: Trigger,
        started_at: OffsetDateTime,
        mut entries: Vec<Entry>,
    ) -> Self {
        let mut counts = Counts::default();
        let mut bytes_freed = 0_u64;
        for entry in &entries {
            match entry.outcome {
                Outcome::Removed => counts.removed += 1,
                Outcome::Kept => counts.kept += 1,
                Outcome::Failed => counts.failed += 1,
            }
            if entry.outcome != Outcome::Kept {
                bytes_freed = bytes_freed.saturating_add(entry.bytes.unwrap_or(0));
            }
        }
        let omitted_count = entries.len().saturating_sub(REPORT_ENTRY_LIMIT);
        entries.sort_by_key(|entry| match entry.outcome {
            Outcome::Failed => 0,
            Outcome::Removed => 1,
            Outcome::Kept if entry.kind == Kind::Worktree && entry.thread_id.is_none() => 2,
            Outcome::Kept => 3,
        });
        entries.truncate(REPORT_ENTRY_LIMIT);
        Self {
            trigger,
            started_at: timestamp(started_at),
            finished_at: timestamp(OffsetDateTime::now_utc()),
            entries,
            counts,
            omitted_count,
            bytes_freed,
        }
    }
}

pub(super) type ManualRequest = oneshot::Sender<Result<Arc<Report>, String>>;

#[derive(Clone)]
pub(super) struct Service {
    latest: watch::Receiver<Option<Arc<Report>>>,
    manual: mpsc::Sender<ManualRequest>,
}

pub(super) struct Worker {
    latest: watch::Sender<Option<Arc<Report>>>,
    pub(super) manual: mpsc::Receiver<ManualRequest>,
}

impl Service {
    pub(super) fn pair() -> (Self, Worker) {
        let (reports, latest) = watch::channel(None);
        let (manual, requests) = mpsc::channel(16);
        (
            Self { latest, manual },
            Worker {
                latest: reports,
                manual: requests,
            },
        )
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Option<Arc<Report>>> {
        self.latest.clone()
    }

    pub(super) async fn run_now(&self) -> Result<Arc<Report>, String> {
        let (send, result) = oneshot::channel();
        self.manual
            .send(send)
            .await
            .map_err(|_| "Storage cleanup worker stopped.".to_owned())?;
        result
            .await
            .map_err(|_| "Storage cleanup worker stopped.".to_owned())?
    }
}

impl Worker {
    pub(super) fn publish(&self, report: Report, manual: Option<ManualRequest>) {
        let report = Arc::new(report);
        self.latest.send_replace(Some(Arc::clone(&report)));
        if let Some(request) = manual {
            let _ = request.send(Ok(report));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(outcome: Outcome, bytes: u64) -> Entry {
        Entry {
            kind: Kind::Worktree,
            outcome,
            reason: "Fixture outcome".to_owned(),
            path: None,
            project_path: None,
            thread_id: Some("thread:fixture".to_owned()),
            thread_title: None,
            bytes: Some(bytes),
            files: None,
        }
    }

    #[test]
    fn totals_include_entries_omitted_from_the_visible_report() {
        let mut entries: Vec<_> = (0..201).map(|_| entry(Outcome::Kept, 100)).collect();
        entries.push(entry(Outcome::Removed, 9));
        entries.push(entry(Outcome::Failed, 2));
        let report = Report::finish(Trigger::Automatic, OffsetDateTime::now_utc(), entries);
        assert_eq!(report.entries.len(), 200);
        assert!(report.entries[0].outcome == Outcome::Failed);
        assert!(report.entries[1].outcome == Outcome::Removed);
        assert_eq!(report.omitted_count, 3);
        assert_eq!(report.counts.kept, 201);
        assert_eq!(report.bytes_freed, 11);
    }

    #[tokio::test]
    async fn a_manual_result_is_retained_for_new_and_existing_subscribers() {
        let (service, mut worker) = Service::pair();
        let mut existing = service.subscribe();
        assert!(existing.borrow_and_update().is_none());
        let caller = tokio::spawn({
            let service = service.clone();
            async move { service.run_now().await.unwrap() }
        });
        let request = worker.manual.recv().await.unwrap();
        let report = Report::finish(Trigger::Manual, OffsetDateTime::now_utc(), Vec::new());
        worker.publish(report, Some(request));
        let result = caller.await.unwrap();
        existing.changed().await.unwrap();
        let reopened = service.subscribe();
        assert!(Arc::ptr_eq(existing.borrow().as_ref().unwrap(), &result));
        assert!(Arc::ptr_eq(reopened.borrow().as_ref().unwrap(), &result));
    }
}
