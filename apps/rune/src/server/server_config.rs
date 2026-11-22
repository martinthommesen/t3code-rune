use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::{Environment, environment_description, turn_commands::timestamp};

mod keybindings;
mod providers;

pub(super) use keybindings::edit::Edit as KeybindingEdit;
pub(crate) use keybindings::parse_keybinding_condition;

const FLAGS: &[&str] = &[
    "environmentThemes",
    "usageLimitSources",
    "usageLimitsCommand",
    "workspaceSnapshotsOnDemand",
    "providerCatalogsOnDemand",
    "providerCatalogDigests",
    "providerStatusDeltas",
];

pub(super) struct Host {
    cwd: PathBuf,
    probe: Mutex<()>,
    pub(super) keybindings: Arc<keybindings::edit::Edits>,
}

impl Host {
    pub(super) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            probe: Mutex::new(()),
            keybindings: Arc::default(),
        }
    }
}

pub(super) fn validate_input(payload: &Value) -> Result<(), String> {
    let fields = payload.as_object().ok_or("Expected object")?;
    if let Some(key) = FLAGS.iter().find(|&&key| {
        fields
            .get(key)
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
    }) {
        return Err(format!("Expected boolean | null\n  at [\"{key}\"]"));
    }
    Ok(())
}

pub(super) async fn snapshot(environment: &Environment) -> Result<Value, Value> {
    let local = environment.local.as_ref().expect("local configuration");
    let settings = local.settings.clone();
    let keybindings_path = settings.path().with_file_name("keybindings.json");
    let profile = keybindings_path
        .parent()
        .expect("settings have a profile directory");
    let ((keybindings, issues), settings) = tokio::task::spawn_blocking({
        let keybindings_path = keybindings_path.clone();
        let keybindings = Arc::clone(&local.config.keybindings);
        move || {
            let settings = settings
                .read()
                .map_err(|error| settings_failure(settings.path(), &error.to_string()))?;
            Ok::<_, Value>((keybindings.initial(&keybindings_path)?, settings))
        }
    })
    .await
    .map_err(|_| {
        settings_failure(
            local.settings.path(),
            "Could not load environment configuration.",
        )
    })??;
    let providers = {
        let _probe = local.config.probe.lock().await;
        providers::initial(&settings, &local.config.cwd).await
    };
    let (description, scratch_root) = environment_description(environment).await;
    let mut snapshot = json!({
        "version": 1,
        "type": "snapshot",
        "config": {
            "environment": description,
            "auth": {
                "policy": "desktop-managed-local",
                "bootstrapMethods": ["desktop-bootstrap"],
                "sessionMethods": ["bearer-access-token"],
                "sessionCookieName": format!("t3_session_{}", environment.port),
            },
            "cwd": local.config.cwd.to_string_lossy(),
            "hostname": hostname::get().ok().map(|value| value.to_string_lossy().into_owned()).unwrap_or_default(),
            "keybindingsConfigPath": keybindings_path.to_string_lossy(),
            "keybindings": keybindings,
            "issues": issues,
            "providers": providers,
            "availableEditors": super::external_editor::available_editors().await,
            "observability": {
                "logsDirectoryPath": profile.join("logs").to_string_lossy(),
                "localTracingEnabled": false,
                "otlpTracesEnabled": false,
                "otlpMetricsEnabled": false,
                "otlpLogsEnabled": false,
            },
            "settings": settings.for_client(),
            "shellResumeCompletionMarker": true,
            "newProjectsRoot": local.named_projects_root.to_string_lossy(),
            "serverTime": timestamp(OffsetDateTime::now_utc()),
        },
    });
    if let Some(root) = scratch_root {
        snapshot["config"]["scratchWorkspaceRoot"] = root.to_string_lossy().as_ref().into();
    }
    Ok(snapshot)
}

fn settings_failure(path: &Path, message: &str) -> Value {
    json!({
        "_tag": "ServerSettingsError",
        "operation": "read",
        "settingsPath": path.to_string_lossy(),
        "message": message,
    })
}
