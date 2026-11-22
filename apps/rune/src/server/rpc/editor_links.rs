use std::path::PathBuf;

use serde_json::{Value, json};

use crate::server::{credentials::Permissions, external_editor, filesystem_metadata};

pub(super) enum Call {
    Metadata(filesystem_metadata::Input),
    Open(external_editor::Input),
}

impl Call {
    pub(super) fn decode(tag: &str, payload: Value) -> Result<Self, String> {
        match tag {
            "filesystem.getMetadata" => {
                filesystem_metadata::Input::decode(&payload).map(Self::Metadata)
            }
            "shell.openInEditor" => external_editor::Input::decode(payload).map(Self::Open),
            _ => unreachable!("known editor RPC"),
        }
    }

    pub(super) fn denied(&self, permissions: Permissions) -> Option<Value> {
        let (scope, permission) = match self {
            Self::Metadata(_) if !permissions.filesystem_read => {
                ("orchestration:read", "filesystem:read")
            }
            Self::Open(_) if !permissions.orchestration_operate => {
                ("orchestration:operate", "orchestration:operate")
            }
            _ => return None,
        };
        Some(json!({
            "_tag": "EnvironmentAuthorizationError",
            "requiredScope": scope,
            "requiredPermission": permission,
            "message": format!("The authenticated token is missing required scope: {permission}."),
        }))
    }

    pub(super) async fn run(self, state_dir: PathBuf) -> Result<Value, Value> {
        match self {
            Self::Metadata(input) => tokio::task::spawn_blocking(move || {
                serde_json::to_value(filesystem_metadata::get(input, &state_dir))
                    .expect("filesystem metadata serializes")
            }).await.map_err(|_| json!({"_tag":"InvalidRequest","message":"Could not read filesystem metadata."})),
            Self::Open(input) => external_editor::open(input).await.map(|()| Value::Null),
        }
    }
}
