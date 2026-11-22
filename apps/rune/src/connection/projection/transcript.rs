use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::Message;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationEntry {
    Message(Message),
    TodoList(TodoListEntry),
}

impl ConversationEntry {
    pub(crate) fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(message) => Some(message),
            Self::TodoList(_) => None,
        }
    }

    fn key(&self) -> EntryKey {
        match self {
            Self::Message(message) => EntryKey::Message(message.id.clone()),
            Self::TodoList(list) => EntryKey::TodoList(list.id.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct TodoListEntry {
    pub(crate) id: String,
    pub(crate) steps: Vec<TodoStep>,
    #[serde(rename = "runId")]
    pub(crate) run_id: Option<String>,
    #[serde(skip)]
    pub(crate) completed_work: Option<CompletedWork>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedWork {
    pub(crate) elapsed_ms: Option<i128>,
    pub(crate) first_row: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TimelineRun {
    id: String,
    status: String,
    started_at: Option<String>,
    completed_at: Option<String>,
}

impl TimelineRun {
    fn duration(&self) -> Option<i128> {
        let parse = |raw: Option<&str>| OffsetDateTime::parse(raw?, &Rfc3339).ok();
        let start = parse(self.started_at.as_deref())?;
        let end = parse(self.completed_at.as_deref())?;
        Some((end - start).whole_milliseconds().max(0))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct TodoStep {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) status: TodoStepStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TodoStepStatus {
    Pending,
    Running,
    Completed,
}

#[derive(Eq, Hash, PartialEq)]
enum EntryKey {
    Message(String),
    TodoList(String),
}

#[derive(Default)]
pub(super) struct Transcript {
    entries: Vec<ConversationEntry>,
    index: HashMap<EntryKey, usize>,
    completed_runs: HashMap<String, Option<i128>>,
}

impl Transcript {
    pub(super) fn replace(&mut self, entries: Vec<ConversationEntry>, runs: Vec<TimelineRun>) {
        self.entries.clear();
        self.index.clear();
        self.completed_runs.clear();
        for run in runs {
            self.update_run(run);
        }
        for entry in entries {
            self.upsert(entry);
        }
    }

    pub(super) fn update_run(&mut self, run: TimelineRun) -> bool {
        let changed = if run.status == "completed" {
            let duration = run.duration();
            self.completed_runs.insert(run.id.clone(), duration) != Some(duration)
        } else {
            self.completed_runs.remove(&run.id).is_some()
        };
        changed
            && self.entries.iter().any(|entry| {
                matches!(entry, ConversationEntry::TodoList(list) if list.run_id.as_deref() == Some(run.id.as_str()))
            })
    }

    pub(super) fn upsert(&mut self, entry: ConversationEntry) -> bool {
        let identity = entry.key();
        if let Some(&index) = self.index.get(&identity) {
            let existing = &mut self.entries[index];
            if *existing == entry {
                return false;
            }
            *existing = entry;
        } else {
            self.index.insert(identity, self.entries.len());
            self.entries.push(entry);
        }
        true
    }

    pub(super) fn entries(&self) -> Vec<ConversationEntry> {
        let mut entries = self.entries.clone();
        let mut folded = HashSet::new();
        for entry in &mut entries {
            if let ConversationEntry::TodoList(list) = entry
                && let Some(run_id) = &list.run_id
                && let Some(&elapsed_ms) = self.completed_runs.get(run_id)
            {
                list.completed_work = Some(CompletedWork {
                    elapsed_ms,
                    first_row: folded.insert(run_id.clone()),
                });
            }
        }
        entries
    }
}
