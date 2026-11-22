use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::command::{CommandEntry, CommandGroup, CommandOutputTarget, CommandStatus};
use super::{Message, WorkspaceFailure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationEntry {
    Message(Message),
    TodoList(TodoListEntry),
    Command(Box<CommandEntry>),
    WorkspaceFailure(Box<WorkspaceFailure>),
}

impl ConversationEntry {
    pub(crate) fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(message) => Some(message),
            Self::TodoList(_) | Self::Command(_) | Self::WorkspaceFailure(_) => None,
        }
    }

    pub(crate) fn continues_work_before(&self, next: &Self) -> bool {
        matches!(self, Self::TodoList(_) | Self::Command(_))
            && matches!(next, Self::TodoList(_) | Self::Command(_))
            && self.run_id() == next.run_id()
    }

    fn key(&self) -> EntryKey {
        match self {
            Self::Message(message) => EntryKey::Message(message.id.clone()),
            Self::TodoList(list) => EntryKey::TodoList(list.id.clone()),
            Self::Command(command) => EntryKey::Command(command.target()),
            Self::WorkspaceFailure(failure) => EntryKey::WorkspaceFailure(failure.id.clone()),
        }
    }

    fn run_id(&self) -> Option<&str> {
        match self {
            Self::TodoList(list) => list.run_id.as_deref(),
            Self::Command(command) => command.run_id.as_deref(),
            Self::WorkspaceFailure(failure) => failure.run_id.as_deref(),
            Self::Message(_) => None,
        }
    }

    fn completed_work(&mut self) -> Option<&mut Option<CompletedWork>> {
        match self {
            Self::TodoList(list) => Some(&mut list.completed_work),
            Self::Command(command) => Some(&mut command.completed_work),
            Self::Message(_) | Self::WorkspaceFailure(_) => None,
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
    workspace_preparation: Option<serde_json::Value>,
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
    Command(CommandOutputTarget),
    WorkspaceFailure(String),
}

#[derive(Default)]
pub(super) struct Transcript {
    entries: Vec<ConversationEntry>,
    index: HashMap<EntryKey, usize>,
    completed_runs: HashMap<String, Option<i128>>,
    active_runs: HashSet<String>,
    failed_preparations: HashSet<String>,
}

impl Transcript {
    pub(super) fn replace(&mut self, entries: Vec<ConversationEntry>, runs: Vec<TimelineRun>) {
        self.entries.clear();
        self.index.clear();
        self.completed_runs.clear();
        self.active_runs.clear();
        self.failed_preparations.clear();
        for run in runs {
            self.update_run(run);
        }
        for entry in entries {
            self.upsert(entry);
        }
    }

    pub(super) fn update_run(&mut self, run: TimelineRun) -> bool {
        let retry_changed = if run.status == "failed" && run.workspace_preparation.is_some() {
            self.failed_preparations.insert(run.id.clone())
        } else {
            self.failed_preparations.remove(&run.id)
        };
        let active_changed = if run.status == "running" {
            self.active_runs.insert(run.id.clone())
        } else {
            self.active_runs.remove(&run.id)
        };
        let completed_changed = if run.status == "completed" {
            let duration = run.duration();
            self.completed_runs.insert(run.id.clone(), duration) != Some(duration)
        } else {
            self.completed_runs.remove(&run.id).is_some()
        };
        (active_changed || completed_changed || retry_changed)
            && self
                .entries
                .iter()
                .any(|entry| entry.run_id() == Some(run.id.as_str()))
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
        entries.retain(|entry| !matches!(entry, ConversationEntry::WorkspaceFailure(failure) if !failure.is_failed()));
        let mut folded = HashSet::new();
        for entry in &mut entries {
            if let ConversationEntry::WorkspaceFailure(failure) = entry {
                failure.retryable = failure
                    .run_id
                    .as_ref()
                    .is_some_and(|id| self.failed_preparations.contains(id));
            }
            if let Some(run_id) = entry.run_id()
                && let Some(&elapsed_ms) = self.completed_runs.get(run_id)
            {
                let run_id = run_id.to_owned();
                if let Some(work) = entry.completed_work() {
                    *work = Some(CompletedWork {
                        elapsed_ms,
                        first_row: folded.insert(run_id),
                    });
                }
            }
        }
        group_commands(&mut entries, &self.active_runs);
        entries
    }

    pub(super) fn can_retry_workspace_preparation(&self, thread_id: &str, run_id: &str) -> bool {
        self.failed_preparations.contains(run_id)
            && self.entries.iter().any(|entry| {
                matches!(entry, ConversationEntry::WorkspaceFailure(failure)
                if failure.is_failed() && failure.run_id.as_deref() == Some(run_id)
                    && failure.thread_id.as_deref().is_none_or(|id| id == thread_id))
            })
    }
}

fn group_commands(entries: &mut [ConversationEntry], active_runs: &HashSet<String>) {
    let mut rest = entries;
    while let Some((entry, tail)) = rest.split_first_mut() {
        let ConversationEntry::Command(first) = entry else {
            rest = tail;
            continue;
        };
        let length = tail.iter().take_while(|entry| matches!(entry, ConversationEntry::Command(command) if command.run_id == first.run_id)).count();
        let (following, next) = tail.split_at_mut(length);
        let continues = next.is_empty()
            && first
                .run_id
                .as_ref()
                .is_some_and(|run| active_runs.contains(run));
        rest = next;
        let mut running = None;
        let mut last = first.as_ref();
        let mut stopped = false;
        for command in
            std::iter::once(first.as_ref()).chain(following.iter().filter_map(
                |entry| match entry {
                    ConversationEntry::Command(command) => Some(command.as_ref()),
                    _ => None,
                },
            ))
        {
            if command.status.shows_progress() {
                running = Some(command);
            }
            stopped |= command.status.is_stopped();
            last = command;
        }
        let active_input = running
            .or_else(|| {
                (continues && last.status == CommandStatus::Completed && !last.failed())
                    .then_some(last)
            })
            .map(|command| command.input.clone());
        if length == 0 && active_input.is_none() && !stopped {
            continue;
        }
        let mut group = CommandGroup {
            id: first.id.clone(),
            first: true,
            count: length + 1,
            active_input,
        };
        first.group = Some(group.clone());
        group.first = false;
        group.active_input = None;
        for entry in following {
            if let ConversationEntry::Command(command) = entry {
                command.group = Some(group.clone());
            }
        }
    }
}
