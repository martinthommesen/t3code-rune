use serde_json::{Value, json};
use uuid::Uuid;

use super::{Command, Connection, Event, EventSender, Session, SessionStop};

const REPORT: &str = "server.getStorageCleanupReport";
const RUN: &str = "server.runStorageCleanup";

mod report;

use report::has_rules;
pub(crate) use report::{Entry, Kind, Outcome, Report};

pub(crate) enum Update {
    Ready {
        writable: bool,
    },
    Rules(bool),
    Report {
        view: Uuid,
        result: Result<Option<Box<Report>>, String>,
    },
    Finished(Result<Box<Report>, String>),
}

pub(super) enum Intent {
    Open(Uuid),
    Close(Uuid),
    Run,
}

pub(super) struct Client {
    supported: bool,
    writable: bool,
    rules: bool,
    report: Option<(Uuid, String)>,
    run: Option<String>,
}

impl Client {
    pub(super) fn new(supported: bool, writable: bool) -> Self {
        Self {
            supported,
            writable,
            rules: false,
            report: None,
            run: None,
        }
    }

    pub(super) fn publish_access(&self, events: &EventSender<impl Fn()>) {
        if self.supported {
            events.send(Event::StorageCleanup(Update::Ready {
                writable: self.writable,
            }));
        }
    }

    pub(super) fn apply_config(&mut self, values: &[Value], events: &EventSender<impl Fn()>) {
        if !self.supported {
            return;
        }
        let settings = values
            .iter()
            .rev()
            .find_map(|event| match event["type"].as_str() {
                Some("snapshot") => event["config"].get("settings"),
                Some("settingsUpdated") => event["payload"].get("settings"),
                _ => None,
            });
        if let Some(settings) = settings {
            self.rules = has_rules(settings);
            events.send(Event::StorageCleanup(Update::Rules(self.rules)));
        }
    }

    pub(super) fn receives(&self, request_id: &str) -> bool {
        self.report.as_ref().is_some_and(|(_, id)| id == request_id)
    }

    pub(super) fn receives_exit(&self, request_id: &str) -> bool {
        self.receives(request_id) || self.run.as_deref() == Some(request_id)
    }

    pub(super) fn chunk(&self, values: Vec<Value>, events: &EventSender<impl Fn()>) -> bool {
        let Some((view, _)) = self.report.as_ref() else {
            return false;
        };
        for value in values {
            let result = serde_json::from_value::<Option<Box<Report>>>(value)
                .map_err(|error| format!("Invalid cleanup report: {error}"));
            let failed = result.is_err();
            events.send(Event::StorageCleanup(Update::Report {
                view: *view,
                result,
            }));
            if failed {
                return false;
            }
        }
        true
    }

    pub(super) fn exit(
        &mut self,
        request_id: &str,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        if self.run.as_deref() == Some(request_id) {
            self.run = None;
            let result = result.and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|error| format!("Invalid cleanup result: {error}"))
            });
            events.send(Event::StorageCleanup(Update::Finished(result)));
        } else if let Some((view, _)) = self.report.take() {
            events.send(Event::StorageCleanup(Update::Report {
                view,
                result: result.map(|_| None),
            }));
        }
    }

    pub(super) fn connection_lost(&mut self, events: &EventSender<impl Fn()>) {
        if self.run.take().is_some() {
            events.send(Event::StorageCleanup(Update::Finished(Err(
                "The connection closed before confirming Cleanup.".into(),
            ))));
        }
    }
}

impl Connection {
    pub(crate) fn open_storage_cleanup(&self, view: Uuid) {
        let _ = self
            .commands
            .send(Command::StorageCleanup(Intent::Open(view)));
    }

    pub(crate) fn close_storage_cleanup(&self, view: Uuid) {
        let _ = self
            .commands
            .send(Command::StorageCleanup(Intent::Close(view)));
    }

    pub(crate) fn run_storage_cleanup(&self) {
        let _ = self.commands.send(Command::StorageCleanup(Intent::Run));
    }
}

impl Session {
    pub(super) async fn storage_cleanup(
        &mut self,
        intent: Intent,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        match intent {
            Intent::Open(view) if self.storage_cleanup.supported => {
                if self
                    .storage_cleanup
                    .report
                    .as_ref()
                    .is_some_and(|(current, _)| view == *current)
                {
                    return Ok(());
                }
                if let Some((_, prior)) = self.storage_cleanup.report.take() {
                    self.interrupt(&prior).await?;
                }
                if self.providers_request_id.is_none() {
                    self.subscribe_provider_catalog().await?;
                }
                let id = self.allocate_id();
                self.send_request(&id, REPORT, json!({})).await?;
                self.storage_cleanup.report = Some((view, id));
            }
            Intent::Close(view) => {
                if let Some((_, id)) = self
                    .storage_cleanup
                    .report
                    .take_if(|(current, _)| *current == view)
                {
                    self.interrupt(&id).await?;
                }
            }
            Intent::Run if self.storage_cleanup.run.is_none() => {
                if !self.storage_cleanup.supported
                    || !self.storage_cleanup.writable
                    || !self.storage_cleanup.rules
                {
                    events.send(Event::StorageCleanup(Update::Finished(Err(
                        "Cleanup is unavailable on this connection.".into(),
                    ))));
                    return Ok(());
                }
                let id = self.allocate_id();
                self.storage_cleanup.run = Some(id.clone());
                self.send_request(&id, RUN, json!({})).await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) async fn stop_invalid_cleanup_report(&mut self) -> Result<(), SessionStop> {
        if let Some((_, id)) = self.storage_cleanup.report.take() {
            self.interrupt(&id).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
