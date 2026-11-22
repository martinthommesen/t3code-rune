use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use super::super::repository_identity::{Change, Identities, enrich_project, enrich_projects};
use super::super::store::{ShellDeltaBatch, Store};
use super::{RequestId, RpcError, defect, failure};

mod identity;

pub(super) use identity::batch_changes;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    after_sequence: Option<u64>,
    http_snapshot_fallback: Option<bool>,
    known_project_identities: Option<String>,
    #[serde(default)]
    request_completion_marker: bool,
    #[serde(default)]
    omit_idle_subagents: bool,
    #[serde(default)]
    omit_older_settled: bool,
}

pub(super) struct Subscription {
    pub(super) id: RequestId,
    cursor: u64,
    subagents: HashSet<String>,
    omit_idle_subagents: bool,
    omit_older_settled: bool,
    previous_enrichment: Option<(HashSet<String>, Value)>,
}

pub(super) struct Update {
    pub(super) subscription: Option<Subscription>,
    pub(super) frame: Option<Value>,
    pub(super) complete_on_ack: Option<RequestId>,
}

pub(super) struct Enrichment {
    sequence: u64,
    projects: Vec<Value>,
    roots: Vec<String>,
}

impl Enrichment {
    pub(super) fn read(
        store: &Store,
        identities: &Identities,
        changes: Option<Vec<Change>>,
    ) -> rusqlite::Result<Self> {
        let (sequence, mut projects) = store.live_project_metadata()?;
        let roots = match changes {
            Some(changes) => identity::changed_projects(&mut projects, changes),
            None => enrich_projects(&mut projects, identities),
        };
        Ok(Self {
            sequence,
            projects,
            roots,
        })
    }
}

impl Subscription {
    pub(super) fn start(
        store: &Store,
        id: RequestId,
        payload: Value,
        identities: Option<&Identities>,
    ) -> Update {
        let input = match serde_json::from_value::<Input>(payload) {
            Ok(input) => input,
            Err(error) => return Update::terminal(defect(id, error.to_string())),
        };
        if input.http_snapshot_fallback == Some(false) {
            return Update::terminal(defect(
                id,
                "httpSnapshotFallback must be true when set.".into(),
            ));
        }
        let marker = input.request_completion_marker;
        if let Some(identities) = identities {
            let subscription = Self {
                id,
                cursor: 0,
                subagents: HashSet::new(),
                omit_idle_subagents: input.omit_idle_subagents,
                omit_older_settled: input.omit_older_settled,
                previous_enrichment: None,
            };
            if let Some(cursor) = input.after_sequence {
                match store.live_shell_deltas(cursor, None) {
                    Ok(Some(batch)) => {
                        return subscription.resume(
                            store,
                            identities,
                            batch,
                            input.known_project_identities.as_deref(),
                            marker,
                        );
                    }
                    Ok(None) if input.http_snapshot_fallback == Some(true) => {
                        let mut update = Update::terminal(chunk(
                            &subscription.id,
                            vec![json!({"kind": "snapshot-required"})],
                        ));
                        update.complete_on_ack = Some(subscription.id);
                        return update;
                    }
                    Ok(None) => (),
                    Err(_) => return Update::terminal(read_error(subscription.id)),
                }
            }
            return subscription.snapshot(store, identities, marker);
        }

        match store.shell() {
            Ok(shell) => {
                let mut snapshot = json!(shell);
                snapshot["schemaVersion"] = json!(2);
                let items =
                    with_marker(vec![json!({"kind":"snapshot","snapshot":snapshot})], marker);
                Update::terminal(chunk(&id, items))
            }
            Err(_) => Update::terminal(read_error(id)),
        }
    }

    pub(super) fn refresh(mut self, store: &Store, identities: &Identities) -> Update {
        match store.live_shell_deltas(self.cursor, Some(&self.subagents)) {
            Ok(Some(batch)) => {
                let mut items = Vec::with_capacity(batch.items.len());
                self.append_delta(&mut items, batch, identities);
                self.publish(items, false)
            }
            Ok(None) => self.snapshot(store, identities, false),
            Err(_) => Update::terminal(read_error(self.id)),
        }
    }

    pub(super) fn enrichment_frame(&mut self, enrichment: &Enrichment) -> Option<Value> {
        let mut items = Vec::new();
        self.append_enrichment(
            &mut items,
            enrichment.sequence,
            &enrichment.projects,
            enrichment.roots.clone(),
        );
        (!items.is_empty()).then(|| chunk(&self.id, items))
    }

    fn resume(
        mut self,
        store: &Store,
        identities: &Identities,
        batch: ShellDeltaBatch,
        known: Option<&str>,
        marker: bool,
    ) -> Update {
        let Ok((_, mut projects)) = store.live_project_metadata() else {
            return Update::terminal(read_error(self.id));
        };
        let mut roots = enrich_projects(&mut projects, identities);
        identity::resume_roots(&projects, &mut roots, known);
        let mut items = Vec::with_capacity(batch.items.len() + 2);
        self.append_enrichment(&mut items, batch.sequence, &projects, roots);
        self.append_delta(&mut items, batch, identities);
        self.publish(items, marker)
    }

    fn append_delta(
        &mut self,
        items: &mut Vec<Value>,
        batch: ShellDeltaBatch,
        identities: &Identities,
    ) {
        self.cursor = batch.sequence;
        self.subagents = batch.subagent_thread_ids;
        for mut item in batch.items {
            match item["kind"].as_str() {
                Some("project.updated") => {
                    self.previous_enrichment = None;
                    enrich_project(&mut item["project"], identities);
                }
                Some("project.removed") => self.previous_enrichment = None,
                _ => (),
            }
            items.push(item);
        }
    }

    fn snapshot(mut self, store: &Store, identities: &Identities, marker: bool) -> Update {
        let Ok((mut item, subagents)) =
            store.live_subscriber_shell_snapshot(self.omit_idle_subagents, self.omit_older_settled)
        else {
            return Update::terminal(read_error(self.id));
        };
        let snapshot = &mut item["snapshot"];
        let Some(cursor) = snapshot["snapshotSequence"].as_u64() else {
            return Update::terminal(read_error(self.id));
        };
        self.cursor = cursor;
        self.previous_enrichment = None;
        self.subagents = subagents;
        let projects = snapshot["projects"]
            .as_array_mut()
            .expect("project snapshot");
        let roots = enrich_projects(projects, identities);
        let mut enrichment = Vec::new();
        self.append_enrichment(&mut enrichment, cursor, projects, roots);
        let mut items = vec![item];
        items.extend(enrichment);
        self.publish(items, marker)
    }

    fn append_enrichment(
        &mut self,
        items: &mut Vec<Value>,
        sequence: u64,
        projects: &[Value],
        roots: Vec<String>,
    ) {
        if roots.is_empty() {
            return;
        }
        let roots_set: HashSet<_> = roots.iter().cloned().collect();
        let marked = identity::marked(sequence, projects, roots);
        let projects = &marked["snapshot"]["projects"];
        if self
            .previous_enrichment
            .as_ref()
            .is_some_and(|(old_roots, old_projects)| {
                *old_roots == roots_set && old_projects == projects
            })
        {
            return;
        }
        self.previous_enrichment = Some((roots_set, projects.clone()));
        items.push(marked);
    }

    fn publish(self, items: Vec<Value>, marker: bool) -> Update {
        let items = with_marker(items, marker);
        let frame = (!items.is_empty()).then(|| chunk(&self.id, items));
        Update {
            subscription: Some(self),
            frame,
            complete_on_ack: None,
        }
    }
}

impl Update {
    fn terminal(frame: Value) -> Self {
        Self {
            subscription: None,
            frame: Some(frame),
            complete_on_ack: None,
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
