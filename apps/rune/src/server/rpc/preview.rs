use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use super::super::{
    browser_preview::{Open, Preview},
    store::Store,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TabRequest {
    thread_id: String,
    tab_id: Option<String>,
}

pub(super) enum Call {
    Open(Open),
    List(TabRequest),
    Close(TabRequest),
    Subscribe,
}

impl Call {
    pub(super) fn decode(tag: &str, payload: Value) -> Result<Self, String> {
        match tag {
            "preview.open" => serde_json::from_value(payload)
                .map(Self::Open)
                .map_err(|error| error.to_string()),
            "preview.list" => serde_json::from_value(payload)
                .map(Self::List)
                .map_err(|error| error.to_string()),
            "preview.close" => serde_json::from_value(payload)
                .map(Self::Close)
                .map_err(|error| error.to_string()),
            "subscribePreviewEvents" if payload.is_object() => Ok(Self::Subscribe),
            _ => Err("Expected object".into()),
        }
    }

    pub(super) fn needs_operation(&self) -> bool {
        matches!(self, Self::Open(_) | Self::Close(_))
    }

    pub(super) async fn run(self, store: &Store, preview: &Arc<Preview>) -> Result<Value, Value> {
        match self {
            Self::Open(input) => {
                let known = store
                    .terminal_workspace(&input.thread_id)
                    .unwrap_or_default()
                    .is_some();
                if !known {
                    return Err(
                        json!({"_tag":"PreviewSessionLookupError","threadId":input.thread_id,"tabId":""}),
                    );
                }
                preview.open(input).map(|snapshot| json!(snapshot))
            }
            Self::List(input) => Ok(json!(preview.list(&input.thread_id))),
            Self::Close(input) => {
                preview.close(&input.thread_id,input.tab_id.as_deref()).await
                    .map_err(|_| json!({"_tag":"PreviewSessionLookupError","threadId":input.thread_id,"tabId":input.tab_id.unwrap_or_default()}))?;
                Ok(Value::Null)
            }
            Self::Subscribe => unreachable!("preview event streams have a dedicated dispatcher"),
        }
    }
}

pub(super) fn denied() -> Value {
    json!({"_tag":"EnvironmentAuthorizationError","message":"The authenticated token is missing required scope: preview:operate.","requiredScope":"orchestration:operate","requiredPermission":"preview:operate"})
}
