use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use t3_rune::providers::codex::RuntimeMode;

use super::{
    Command, Connection, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop, WorkspaceMode,
};

const KEY: &str = "defaultThreadEnvMode";
const ORIGIN: &str = "newWorktreesStartFromOrigin";
const PERMISSIONS: &str = "defaultRuntimeMode";
const DIRECTORY: &str = "worktreesDirectory";
const BRANCH_NAMING: &str = "branchNamingMode";
const PROJECT_OVERRIDES: &str = "projectSettingsOverrides";

pub(super) enum Edit {
    Mode(Option<WorkspaceMode>),
    StartFromRemote(Option<bool>),
    Permissions(Option<RuntimeMode>),
    WorktreesDirectory(String),
    BranchNaming(Option<BranchNamingMode>),
}

impl Edit {
    fn property(&self, project_scope: bool) -> (&'static str, Value) {
        match self {
            Self::Mode(mode) => (KEY, json!(mode)),
            Self::StartFromRemote(Some(enabled)) => (ORIGIN, json!(enabled)),
            Self::StartFromRemote(None) if project_scope => (ORIGIN, Value::Null),
            Self::StartFromRemote(None) => (ORIGIN, Value::Bool(true)),
            Self::Permissions(Some(mode)) => (PERMISSIONS, json!(mode)),
            Self::Permissions(None) if project_scope => (PERMISSIONS, Value::Null),
            Self::Permissions(None) => (PERMISSIONS, json!(RuntimeMode::FullAccess)),
            Self::WorktreesDirectory(path) => (DIRECTORY, json!(path)),
            Self::BranchNaming(Some(mode)) => (BRANCH_NAMING, json!(mode)),
            Self::BranchNaming(None) if project_scope => (BRANCH_NAMING, Value::Null),
            Self::BranchNaming(None) => (BRANCH_NAMING, json!(BranchNamingMode::Static)),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OriginSetting {
    pub(crate) enabled: bool,
    pub(crate) projects: HashMap<String, bool>,
    pub(crate) writable: bool,
    pub(crate) managed_by: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PermissionPolicy {
    managed_by: Option<String>,
    ceiling: Option<(String, RuntimeMode)>,
}

impl PermissionPolicy {
    fn read(policy: &Value) -> Self {
        Self {
            managed_by: managed_by(policy, PERMISSIONS),
            ceiling: policy["organization"]
                .as_str()
                .filter(|name| !name.is_empty())
                .and_then(|name| {
                    let mode =
                        RuntimeMode::deserialize(&policy["limits"]["maxRuntimeMode"]).ok()?;
                    Some((name.to_owned(), mode))
                }),
        }
    }

    fn denial(&self, mode: RuntimeMode) -> Option<String> {
        let (organization, ceiling) = self.ceiling.as_ref()?;
        (permission_rank(mode) > permission_rank(*ceiling)).then(|| {
            format!(
                "{organization} limits threads to {}.",
                PermissionSetting::label(*ceiling)
            )
        })
    }
}

fn permission_rank(mode: RuntimeMode) -> u8 {
    match mode {
        RuntimeMode::ApprovalRequired => 0,
        RuntimeMode::AutoAcceptEdits => 1,
        RuntimeMode::Auto => 2,
        RuntimeMode::FullAccess => 3,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PermissionSetting {
    pub(crate) environment: RuntimeMode,
    pub(crate) projects: HashMap<String, RuntimeMode>,
    pub(crate) writable: bool,
    policy: PermissionPolicy,
}

impl PermissionSetting {
    pub(crate) fn managed_by(&self) -> Option<&str> {
        self.policy.managed_by.as_deref()
    }

    pub(crate) fn denial(&self, mode: RuntimeMode) -> Option<String> {
        self.policy.denial(mode)
    }

    pub(crate) fn label(mode: RuntimeMode) -> &'static str {
        match mode {
            RuntimeMode::ApprovalRequired => "Supervised",
            RuntimeMode::AutoAcceptEdits => "Auto-accept edits",
            RuntimeMode::Auto => "Auto",
            RuntimeMode::FullAccess => "Full access",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorktreeLocation {
    pub(crate) path: String,
    pub(crate) writable: bool,
    pub(crate) managed_by: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BranchNamingMode {
    #[default]
    Static,
    Semantic,
    Custom,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BranchNamingSetting {
    pub(crate) configured: BranchNamingMode,
    pub(crate) projects: HashMap<String, BranchNamingMode>,
    pub(crate) writable: bool,
    pub(crate) managed_by: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceSetting {
    pub(crate) configured: Option<WorkspaceMode>,
    pub(crate) origin: OriginSetting,
    pub(crate) permission: PermissionSetting,
    pub(crate) worktree_location: Option<WorktreeLocation>,
    pub(crate) branch_naming: BranchNamingSetting,
    pub(crate) projects: HashMap<String, WorkspaceMode>,
    pub(crate) writable: bool,
    pub(crate) managed_by: Option<String>,
}

pub(super) struct Subscription {
    can_write: bool,
    managed_by: Option<String>,
    origin_managed_by: Option<String>,
    permission_policy: PermissionPolicy,
    directory_supported: bool,
    directory_managed_by: Option<String>,
    branch_naming_managed_by: Option<String>,
    pending: Option<Pending>,
    overrides: Map<String, Value>,
    pending_file: Option<Pending>,
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
            origin_managed_by: None,
            permission_policy: PermissionPolicy::default(),
            directory_supported: false,
            directory_managed_by: None,
            branch_naming_managed_by: None,
            pending: None,
            overrides: Map::new(),
            pending_file: None,
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
                    self.managed_by = managed_by(&config["managedPolicy"], KEY);
                    self.origin_managed_by = managed_by(&config["managedPolicy"], ORIGIN);
                    self.permission_policy = PermissionPolicy::read(&config["managedPolicy"]);
                    self.directory_supported =
                        config["environment"]["capabilities"][DIRECTORY].as_bool() == Some(true);
                    self.directory_managed_by = managed_by(&config["managedPolicy"], DIRECTORY);
                    self.branch_naming_managed_by =
                        managed_by(&config["managedPolicy"], BRANCH_NAMING);

                    &config["settings"]
                }
                Some("settingsUpdated") => &event["payload"]["settings"],
                _ => continue,
            };
            if let Some(mode) = configured_mode(settings) {
                self.overrides = settings[PROJECT_OVERRIDES]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                current = Some((mode, settings));
            }
        }
        current.map(|(configured, settings)| WorkspaceSetting {
            configured,
            worktree_location: self.directory_supported.then(|| WorktreeLocation {
                path: settings[DIRECTORY].as_str().unwrap_or("").to_owned(),
                writable: self.can_write && self.directory_managed_by.is_none(),
                managed_by: self.directory_managed_by.clone(),
            }),

            branch_naming: BranchNamingSetting {
                configured: BranchNamingMode::deserialize(&settings[BRANCH_NAMING])
                    .unwrap_or_default(),
                projects: self
                    .overrides
                    .iter()
                    .filter_map(|(id, project)| {
                        BranchNamingMode::deserialize(&project[BRANCH_NAMING])
                            .ok()
                            .map(|mode| (id.clone(), mode))
                    })
                    .collect(),
                writable: self.can_write && self.branch_naming_managed_by.is_none(),
                managed_by: self.branch_naming_managed_by.clone(),
            },
            origin: OriginSetting {
                enabled: settings[ORIGIN].as_bool().unwrap_or(true),
                projects: self
                    .overrides
                    .iter()
                    .filter_map(|(id, project)| {
                        project[ORIGIN]
                            .as_bool()
                            .map(|enabled| (id.clone(), enabled))
                    })
                    .collect(),
                writable: self.can_write && self.origin_managed_by.is_none(),
                managed_by: self.origin_managed_by.clone(),
            },
            permission: PermissionSetting {
                environment: RuntimeMode::deserialize(&settings[PERMISSIONS])
                    .unwrap_or(RuntimeMode::FullAccess),
                projects: self
                    .overrides
                    .iter()
                    .filter_map(|(id, project)| {
                        RuntimeMode::deserialize(&project[PERMISSIONS])
                            .ok()
                            .map(|mode| (id.clone(), mode))
                    })
                    .collect(),
                writable: self.can_write && self.permission_policy.managed_by.is_none(),
                policy: self.permission_policy.clone(),
            },
            projects: self
                .overrides
                .iter()
                .filter_map(|(id, project)| {
                    configured_mode(project)
                        .flatten()
                        .map(|mode| (id.clone(), mode))
                })
                .collect(),
            writable: self.can_write && self.managed_by.is_none(),
            managed_by: self.managed_by.clone(),
        })
    }

    fn patch(&self, project_id: Option<&str>, edit: Edit) -> Value {
        let (key, value) = edit.property(project_id.is_some());
        let Some(project_id) = project_id else {
            return json!({key: value});
        };
        let mut project = self
            .overrides
            .get(project_id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if value.is_null() {
            project.remove(key);
        } else {
            project.insert(key.to_owned(), value);
        }
        // An empty object drops a genuinely empty entry while the server preserves opaque
        // fields which the client projection cannot see.
        json!({PROJECT_OVERRIDES: {project_id: project}})
    }
}

fn configured_mode(settings: &Value) -> Option<Option<WorkspaceMode>> {
    match settings.as_object()?.get(KEY) {
        None | Some(Value::Null) => Some(None),
        Some(value) => WorkspaceMode::deserialize(value).ok().map(Some),
    }
}

fn managed_by(policy: &Value, key: &str) -> Option<String> {
    let organization = policy["organization"].as_str()?;
    let locked = policy["lockedSettings"].as_array()?.iter().any(|entry| {
        entry.as_str().is_some_and(|path| {
            path == key
                || path
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    });
    (locked && !organization.is_empty()).then(|| organization.to_owned())
}

impl Connection {
    pub(crate) fn ensure_workspace_settings(&self) {
        let _ = self.commands.send(Command::EnsureWorkspaceSettings);
    }

    pub(crate) fn update_workspace_mode(
        &self,
        project_id: Option<String>,
        mode: Option<WorkspaceMode>,
    ) -> Option<Uuid> {
        self.update_workspace_setting(project_id, Edit::Mode(mode))
    }

    pub(crate) fn update_worktree_origin(
        &self,
        project_id: Option<String>,
        enabled: Option<bool>,
    ) -> Option<Uuid> {
        self.update_workspace_setting(project_id, Edit::StartFromRemote(enabled))
    }

    pub(crate) fn update_default_permissions(
        &self,
        project_id: Option<String>,
        mode: Option<RuntimeMode>,
    ) -> Option<Uuid> {
        self.update_workspace_setting(project_id, Edit::Permissions(mode))
    }

    pub(crate) fn update_worktree_location(&self, path: String) -> Option<Uuid> {
        self.update_workspace_setting(None, Edit::WorktreesDirectory(path))
    }

    pub(crate) fn update_branch_naming_mode(
        &self,
        project_id: Option<String>,
        mode: Option<BranchNamingMode>,
    ) -> Option<Uuid> {
        self.update_workspace_setting(project_id, Edit::BranchNaming(mode))
    }

    fn update_workspace_setting(&self, project_id: Option<String>, edit: Edit) -> Option<Uuid> {
        let operation = Uuid::new_v4();
        self.commands
            .send(Command::UpdateWorkspaceSetting {
                project_id,
                edit,
                operation,
            })
            .ok()?;
        Some(operation)
    }

    pub(crate) fn read_workspace_project_file(&self, project_id: String) -> Option<Uuid> {
        let operation = Uuid::new_v4();
        self.commands
            .send(Command::ReadWorkspaceProjectFile {
                project_id,
                operation,
            })
            .ok()?;
        Some(operation)
    }
}

impl Session {
    pub(super) async fn save_workspace_setting(
        &mut self,
        project_id: Option<String>,
        edit: Edit,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let settings = &self.workspace_settings;
        let managed_by = match edit {
            Edit::Mode(_) => &settings.managed_by,
            Edit::StartFromRemote(_) => &settings.origin_managed_by,
            Edit::Permissions(_) => &settings.permission_policy.managed_by,
            Edit::WorktreesDirectory(_) => &settings.directory_managed_by,
            Edit::BranchNaming(_) => &settings.branch_naming_managed_by,
        };
        let denied = if matches!(edit, Edit::WorktreesDirectory(_))
            && (project_id.is_some() || !settings.directory_supported)
        {
            Some("Worktree location is unavailable for this scope.".into())
        } else if let Some(organization) = managed_by {
            Some(format!("Managed by {organization}."))
        } else if !settings.can_write {
            Some("This connection lacks permission to change environment settings.".into())
        } else if project_id
            .as_deref()
            .is_some_and(|id| self.shell.project(id).is_none())
        {
            Some("The selected project is unavailable.".into())
        } else if settings.pending.is_some() {
            Some("The setting change is already being saved.".into())
        } else if let Edit::Permissions(mode) = &edit {
            let mode = mode.or_else(|| project_id.is_none().then_some(RuntimeMode::FullAccess));
            mode.and_then(|mode| settings.permission_policy.denial(mode))
        } else {
            None
        };
        if let Some(message) = denied {
            settled(events, operation, Err(message));
            return Ok(());
        }

        let patch = settings.patch(project_id.as_deref(), edit);
        let id = self.allocate_id();
        self.workspace_settings.pending = Some(Pending {
            request: id.clone(),
            operation,
            started: Instant::now(),
        });
        self.send_request(&id, "server.updateSettings", json!({"patch": patch}))
            .await
    }

    pub(super) async fn load_workspace_project_file(
        &mut self,
        project_id: String,
        operation: Uuid,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if let Some(pending) = self.workspace_settings.pending_file.take() {
            self.interrupt(&pending.request).await?;
        }
        let Some(project) = self
            .shell
            .project(&project_id)
            .filter(|_| self.filesystem_read)
        else {
            events.send(Event::WorkspaceProjectFile {
                operation,
                mode: None,
            });
            return Ok(());
        };
        let payload = json!({"cwd": project.workspace_path(), "relativePath": "t3.json", "allowMissing": true});
        let id = self.allocate_id();
        self.workspace_settings.pending_file = Some(Pending {
            request: id.clone(),
            operation,
            started: Instant::now(),
        });
        self.send_request(&id, "projects.readFile", payload).await
    }

    pub(super) fn workspace_project_file_matches(&self, id: &str) -> bool {
        self.workspace_settings
            .pending_file
            .as_ref()
            .is_some_and(|pending| pending.request == id)
    }

    pub(super) fn workspace_project_file_result(
        &mut self,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        if let Some(pending) = self.workspace_settings.pending_file.take() {
            let mode = result.ok().and_then(|file| {
                crate::server::project_file::workspace_mode(file["contents"].as_str()?)
            });
            events.send(Event::WorkspaceProjectFile {
                operation: pending.operation,
                mode,
            });
        }
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
        if self
            .workspace_settings
            .pending_file
            .as_ref()
            .is_some_and(|file| file.started.elapsed() >= REQUEST_TIMEOUT)
        {
            self.workspace_project_file_result(Ok(Value::Null), events);
        }
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
    events.send(Event::WorkspaceSettingSettled { operation, result });
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

    #[test]
    fn replaces_only_the_selected_project_entry_and_clears_only_the_workspace_key() {
        let mut subscription = Subscription::new(true);
        let current = subscription.apply(&[json!({"version":1,"type":"snapshot","config":{"settings":{
            KEY:"worktree",
            PROJECT_OVERRIDES:{"a":{KEY:"local","defaultAutoPull":true},"b":{KEY:"worktree"}}
        }}})]).unwrap();
        assert_eq!(current.configured, Some(WorkspaceMode::Worktree));
        assert_eq!(current.projects["a"], WorkspaceMode::Local);
        assert_eq!(
            subscription.patch(Some("a"), Edit::Mode(Some(WorkspaceMode::Worktree))),
            json!({PROJECT_OVERRIDES:{"a":{KEY:"worktree","defaultAutoPull":true}}})
        );
        assert_eq!(
            subscription.patch(Some("a"), Edit::Mode(None)),
            json!({PROJECT_OVERRIDES:{"a":{"defaultAutoPull":true}}})
        );
        assert_eq!(
            subscription.patch(Some("b"), Edit::Mode(None)),
            json!({PROJECT_OVERRIDES:{"b":{}}})
        );
    }

    #[test]
    fn worktree_origin_reads_inheritance_and_resets_each_scope_without_cross_updates() {
        let mut subscription = Subscription::new(true);
        let current = subscription
            .apply(&[json!({"version":1,"type":"snapshot","config":{
                "settings": {ORIGIN:false, PROJECT_OVERRIDES: {
                    "a": {ORIGIN:true,"defaultAutoPull":true},"b": {ORIGIN:false}
                }},
                "managedPolicy": {"organization": "Example", "lockedSettings": [KEY]}
            }})])
            .unwrap();
        assert!(!current.writable);
        assert!(current.origin.writable);
        assert!(!current.origin.enabled);
        assert!(current.origin.projects["a"]);
        assert!(!current.origin.projects["b"]);
        assert_eq!(
            subscription.patch(None, Edit::StartFromRemote(None)),
            json!({ORIGIN:true})
        );
        assert_eq!(
            subscription.patch(Some("a"), Edit::StartFromRemote(None)),
            json!({PROJECT_OVERRIDES:{"a":{"defaultAutoPull":true}}})
        );
        assert_eq!(
            subscription.patch(Some("a"), Edit::StartFromRemote(Some(false))),
            json!({PROJECT_OVERRIDES:{"a":{ORIGIN:false,"defaultAutoPull":true}}})
        );
        assert!(
            subscription
                .apply(&[json!({"version":1,"type":"settingsUpdated","payload":{"settings":{}}})])
                .unwrap()
                .origin
                .enabled
        );
    }
    #[test]
    fn permissions_resets_follow_the_selected_scope_and_policy() {
        let mut state = Subscription::new(true);
        let frame = json!({"version":1,"type":"snapshot","config":{
            "settings":{"defaultRuntimeMode":"auto-accept-edits","projectSettingsOverrides":{
                "fixture":{"defaultRuntimeMode":"approval-required","defaultAutoPull":true}
            }},
            "managedPolicy":{"organization":"Example","limits":{"maxRuntimeMode":"auto-accept-edits"}}
        }});
        let current = state.apply(&[frame]).unwrap();
        assert_eq!(current.permission.environment, RuntimeMode::AutoAcceptEdits);
        assert_eq!(
            current.permission.projects["fixture"],
            RuntimeMode::ApprovalRequired
        );
        assert_eq!(
            current.permission.denial(RuntimeMode::AutoAcceptEdits),
            None
        );
        assert_eq!(
            current.permission.denial(RuntimeMode::Auto),
            Some("Example limits threads to Auto-accept edits.".into())
        );
        assert_eq!(
            state.patch(Some("fixture"), Edit::Permissions(None)),
            json!({"projectSettingsOverrides":{
                "fixture":{"defaultAutoPull":true}
            }})
        );
        assert_eq!(
            state.patch(None, Edit::Permissions(None)),
            json!({"defaultRuntimeMode":"full-access"})
        );
    }
}
