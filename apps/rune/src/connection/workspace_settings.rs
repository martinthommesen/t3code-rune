use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{
    Command, Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop, WorkspaceMode,
};

const KEY: &str = "defaultThreadEnvMode";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceSetting {
    pub(crate) configured: Option<WorkspaceMode>,
    pub(crate) writable: bool,
    pub(crate) managed_by: Option<String>,
}

pub(super) struct Subscription {
    can_write: bool,
    managed_by: Option<String>,
    pending: Option<Pending>,
}

struct Pending {
    request: String,
    operation: Uuid,
    started: Instant,
}

impl Subscription {
    pub(super) fn new(can_write: bool) -> Self {
        Self {
            can_write,
            managed_by: None,
            pending: None,
        }
    }

    pub(super) fn apply(&mut self, frames: &[Value]) -> Option<WorkspaceSetting> {
        let mut current = None;
        for event in frames {
            if event["version"] != 1 {
                continue;
            }
            let settings = match event["type"].as_str() {
                Some("snapshot") => {
                    let config = &event["config"];
                    self.managed_by = managed_by(&config["managedPolicy"]);
                    &config["settings"]
                }
                Some("settingsUpdated") => &event["payload"]["settings"],
                _ => continue,
            };
            if let Some(mode) = configured_mode(settings) {
                current = Some(mode);
            }
        }
        current.map(|configured| WorkspaceSetting {
            configured,
            writable: self.can_write && self.managed_by.is_none(),
            managed_by: self.managed_by.clone(),
        })
    }
}

fn configured_mode(settings: &Value) -> Option<Option<WorkspaceMode>> {
    match settings.as_object()?.get(KEY) {
        None | Some(Value::Null) => Some(None),
        Some(value) => WorkspaceMode::deserialize(value).ok().map(Some),
    }
}

fn managed_by(policy: &Value) -> Option<String> {
    let organization = policy["organization"].as_str()?;
    let locked = policy["lockedSettings"].as_array()?.iter().any(|entry| {
        entry
            .as_str()
            .is_some_and(|path| path == KEY || path.starts_with("defaultThreadEnvMode."))
    });
    (locked && !organization.is_empty()).then(|| organization.to_owned())
}

impl Connection {
    pub(crate) fn ensure_workspace_settings(&self) {
        let _ = self.commands.send(Command::EnsureWorkspaceSettings);
    }

    pub(crate) fn update_workspace_mode(&self, mode: Option<WorkspaceMode>) -> Option<Uuid> {
        let operation = Uuid::new_v4();
        self.commands
            .send(Command::UpdateWorkspaceMode { mode, operation })
            .ok()?;
        Some(operation)
    }
}

impl Session {
    pub(super) async fn save_workspace_mode(
        &mut self,
        mode: Option<WorkspaceMode>,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let settings = &self.workspace_settings;
        let denied = if let Some(organization) = &settings.managed_by {
            Some(format!("Managed by {organization}."))
        } else if !settings.can_write {
            Some("This connection lacks permission to change environment settings.".into())
        } else if settings.pending.is_some() {
            Some("The workspace setting is already being saved.".into())
        } else {
            None
        };
        if let Some(message) = denied {
            settled(events, operation, Err(message));
            return Ok(());
        }

        let id = self.allocate_id();
        self.workspace_settings.pending = Some(Pending {
            request: id.clone(),
            operation,
            started: Instant::now(),
        });
        self.send_request(&id, "server.updateSettings", json!({"patch": {KEY: mode}}))
            .await
    }

    pub(super) fn workspace_write_matches(&self, id: &str) -> bool {
        self.workspace_settings
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request == id)
    }

    pub(super) fn workspace_write_result(
        &mut self,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        if let Some(pending) = self.workspace_settings.pending.take() {
            // The subscription owns the displayed value. A later settings notification may
            // already include another client’s change by the time this result arrives.
            let result = result.and_then(|value| {
                configured_mode(&value)
                    .map(|_| ())
                    .ok_or_else(|| "The environment returned an invalid workspace setting.".into())
            });
            settled(events, pending.operation, result);
        }
    }

    pub(super) fn expire_workspace_write(&mut self, events: &EventSender<impl Fn()>) {
        let expired = self
            .workspace_settings
            .pending
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= REQUEST_TIMEOUT);
        if expired {
            self.fail_workspace_write(
                events,
                "The environment did not confirm the workspace setting in time.",
            );
        }
    }

    pub(super) fn fail_workspace_write(&mut self, events: &EventSender<impl Fn()>, message: &str) {
        if let Some(pending) = self.workspace_settings.pending.take() {
            settled(events, pending.operation, Err(message.into()));
        }
    }
}

fn settled(events: &EventSender<impl Fn()>, operation: Uuid, result: Result<(), String>) {
    events.send(Event::WorkspaceModeSettled { operation, result });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_raw_workspace_value_from_snapshots_and_ordered_live_changes() {
        let mut subscription = Subscription::new(true);
        let initial = subscription
            .apply(&[json!({"version":1, "type":"snapshot",
                "config":{"settings":{}}
            })])
            .unwrap();
        assert!(initial.writable);
        assert_eq!(initial.configured, None);
        let next = subscription.apply(&[
            json!({"version":1,"type":"settingsUpdated","payload":{"settings":{KEY:"worktree"}}}),
            json!({"version":1,"type":"settingsUpdated","payload":{"settings":{KEY:"local"}}}),
        ]).unwrap();
        assert_eq!(next.configured, Some(WorkspaceMode::Local));

        let reset = subscription
            .apply(&[json!({"version":1,"type":"settingsUpdated","payload":{"settings":{}}})])
            .unwrap();
        assert_eq!(reset.configured, None);

        let locked = subscription.apply(&[json!({"version":1,"type":"snapshot","config":{
            "settings":{KEY:"worktree"},"managedPolicy":{"organization":"Example","lockedSettings":[KEY]}
        }})]).unwrap();
        assert!(!locked.writable);
        assert_eq!(locked.managed_by.as_deref(), Some("Example"));
    }
}
