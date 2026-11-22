use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use super::super::store::{ShellDeltaBatch, Store};
use super::{RequestId, RpcError, defect, failure};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    after_sequence: Option<u64>,
    #[serde(default)]
    request_completion_marker: bool,
}

pub(super) struct Subscription {
    pub(super) id: RequestId,
    cursor: u64,
    subagents: HashSet<String>,
}

pub(super) struct Update {
    pub(super) subscription: Option<Subscription>,
    pub(super) frame: Option<Value>,
}

impl Subscription {
    pub(super) fn start(store: &Store, id: RequestId, payload: Value, live: bool) -> Update {
        let input = match serde_json::from_value::<Input>(payload) {
            Ok(input) => input,
            Err(error) => return Update::terminal(defect(id, error.to_string())),
        };
        if live {
            if let Some(cursor) = input.after_sequence {
                match store.live_shell_deltas(cursor, None) {
                    Ok(Some(batch)) => {
                        return Self::delta(id, batch, input.request_completion_marker);
                    }
                    Ok(None) => (),
                    Err(_) => return Update::terminal(read_error(id)),
                }
            }
            return Self::snapshot(store, id, input.request_completion_marker);
        }

        match store.shell() {
            Ok(shell) => {
                let mut snapshot = json!(shell);
                snapshot["schemaVersion"] = json!(2);
                let items = with_marker(
                    vec![json!({"kind":"snapshot","snapshot":snapshot})],
                    input.request_completion_marker,
                );
                Update::terminal(chunk(&id, items))
            }
            Err(_) => Update::terminal(read_error(id)),
        }
    }

    pub(super) fn refresh(self, store: &Store) -> Update {
        match store.live_shell_deltas(self.cursor, Some(&self.subagents)) {
            Ok(Some(batch)) => Self::delta(self.id, batch, false),
            Ok(None) => Self::snapshot(store, self.id, false),
            Err(_) => Update::terminal(read_error(self.id)),
        }
    }

    fn delta(id: RequestId, batch: ShellDeltaBatch, marker: bool) -> Update {
        let items = with_marker(batch.items, marker);
        let frame = (!items.is_empty()).then(|| chunk(&id, items));
        Update {
            subscription: Some(Self {
                id,
                cursor: batch.sequence,
                subagents: batch.subagent_thread_ids,
            }),
            frame,
        }
    }

    fn snapshot(store: &Store, id: RequestId, marker: bool) -> Update {
        let Ok(item) = store.live_shell_snapshot() else {
            return Update::terminal(read_error(id));
        };
        let snapshot = &item["snapshot"];
        let Some(cursor) = snapshot["snapshotSequence"].as_u64() else {
            return Update::terminal(read_error(id));
        };
        let subagents = snapshot["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|thread| thread["lineage"]["relationshipToParent"] == "subagent")
            .filter_map(|thread| thread["id"].as_str().map(str::to_owned))
            .collect();
        let frame = chunk(&id, with_marker(vec![item], marker));
        Update {
            subscription: Some(Self {
                id,
                cursor,
                subagents,
            }),
            frame: Some(frame),
        }
    }
}

impl Update {
    fn terminal(frame: Value) -> Self {
        Self {
            subscription: None,
            frame: Some(frame),
        }
    }
}

fn with_marker(mut items: Vec<Value>, requested: bool) -> Vec<Value> {
    if requested {
        items.push(json!({"kind":"synchronized"}));
    }
    items
}

fn read_error(id: RequestId) -> Value {
    failure(
        id,
        RpcError::OrchestrationV2GetShellSnapshotError {
            message: "Could not read projects and threads.",
        },
    )
}

fn chunk(id: &RequestId, values: Vec<Value>) -> Value {
    json!({"_tag":"Chunk","requestId":id,"values":values})
}
