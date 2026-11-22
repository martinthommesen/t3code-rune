mod command;
mod queue;
mod transcript;

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::launch::ModelSelection;
use super::runtime_approval::{ApprovalDetails, ApprovalProjection, RuntimeRequest};
use super::runtime_question::{QuestionDetails, QuestionProjection};
pub(crate) use command::{CommandEntry, CommandOutputTarget, CommandStatus};
pub(crate) use queue::QueuedMessage;
pub(crate) use transcript::{CompletedWork, ConversationEntry, TodoListEntry, TodoStepStatus};
use transcript::{TimelineRun, Transcript};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) id: String,
    #[serde(rename = "title")]
    pub(crate) name: String,
    #[serde(rename = "workspaceRoot")]
    pub(crate) path: String,
    #[serde(rename = "projectIcon")]
    pub(crate) icon: Option<ProjectIcon>,
    #[serde(rename = "faviconPath")]
    pub(crate) favicon_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProjectIcon {
    Monogram {
        text: String,
        color: String,
    },
    Emoji {
        emoji: String,
    },
    Lucide {
        name: String,
        color: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectRow {
    #[serde(flatten)]
    project: Project,
    pub(super) default_model_selection: Option<ModelSelection>,
    pub(super) archived_at: Option<String>,
    repository_identity: Option<Value>,
}

impl ProjectRow {
    fn retain_repository_identity(&mut self, previous: Option<&Self>) {
        if self.repository_identity.is_none()
            && let Some(previous) = previous.filter(|row| row.project.path == self.project.path)
        {
            self.repository_identity
                .clone_from(&previous.repository_identity);
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Thread {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(rename = "projectId")]
    pub(crate) project_id: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(rename = "updatedAt")]
    pub(crate) updated_at: Option<String>,
    #[serde(flatten)]
    pub(crate) activity: ThreadActivity,
    #[serde(flatten)]
    pub(crate) providers: ThreadProviders,
    #[serde(rename = "worktreePath")]
    pub(crate) worktree_path: Option<String>,
    #[serde(skip)]
    pub(crate) archived: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadActivity {
    latest_user_message_at: Option<String>,
    latest_run_requested_at: Option<String>,
    latest_run_started_at: Option<String>,
    latest_run_completed_at: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadProviders {
    model_selection: Option<ModelSelection>,
    latest_run_id: Option<String>,
    provider_instance_id: Option<String>,
    #[serde(default)]
    provider_instance_history: Vec<String>,
}

impl ThreadProviders {
    pub(crate) fn current_instance(&self) -> Option<&str> {
        let model = self
            .model_selection
            .as_ref()
            .map(ModelSelection::instance_id);
        let runtime = self.provider_instance_id.as_deref();
        if self.latest_run_id.is_some() {
            runtime.or(model)
        } else {
            model.or(runtime)
        }
    }

    pub(crate) fn previous_instances(&self, current: &str) -> impl Iterator<Item = &str> {
        let mut previous = self
            .provider_instance_history
            .iter()
            .rev()
            .map(String::as_str)
            .filter(|instance| *instance != current);
        let recent = previous.next();
        let earlier = previous.next();
        earlier.into_iter().chain(recent)
    }
}

impl Thread {
    pub(crate) fn has_started_run(&self) -> bool {
        self.run_started_at().is_some()
    }

    pub(crate) fn run_started_at(&self) -> Option<&str> {
        self.activity.latest_run_started_at.as_deref()
    }

    pub(crate) fn activity_timestamp(&self) -> Option<OffsetDateTime> {
        let parse = |timestamp: &str| OffsetDateTime::parse(timestamp, &Rfc3339).ok();
        [
            self.activity.latest_user_message_at.as_deref(),
            self.activity.latest_run_requested_at.as_deref(),
            self.activity.latest_run_started_at.as_deref(),
            self.activity.latest_run_completed_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(parse)
        .max()
        .or_else(|| self.updated_at.as_deref().and_then(parse))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) text: String,
    #[serde(rename = "createdBy")]
    pub(crate) created_by: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ThreadLineage {
    relationship_to_parent: Option<String>,
    #[serde(default)]
    side_chat: bool,
}

#[derive(Deserialize, PartialEq, Eq)]
struct ThreadRow {
    #[serde(flatten)]
    thread: Thread,
    lineage: Option<ThreadLineage>,
}

impl ThreadRow {
    fn listed(&self) -> bool {
        self.lineage.as_ref().is_none_or(|lineage| {
            lineage.relationship_to_parent.as_deref() != Some("subagent") && !lineage.side_chat
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ThreadLocation {
    Active,
    Archive,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellSnapshot {
    snapshot_sequence: u64,
    projects: Vec<ProjectRow>,
    threads: Vec<ThreadRow>,
    #[serde(default)]
    archived_threads: Vec<ThreadRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveSnapshot {
    snapshot_sequence: u64,
    threads: Vec<ThreadRow>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
enum ShellItem {
    #[serde(rename = "snapshot")]
    Snapshot {
        snapshot: ShellSnapshot,
        resolved_repository_identity_roots: Option<Vec<String>>,
    },
    #[serde(rename = "project.updated")]
    ProjectUpdated {
        sequence: u64,
        project: Box<ProjectRow>,
    },
    #[serde(rename = "project.removed")]
    ProjectRemoved { sequence: u64, project_id: String },
    #[serde(rename = "thread.updated")]
    ThreadUpdated {
        sequence: u64,
        location: ThreadLocation,
        thread: Box<ThreadRow>,
    },
    #[serde(rename = "thread.removed")]
    ThreadRemoved {
        sequence: u64,
        location: ThreadLocation,
        thread_id: String,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Default)]
pub(crate) struct Shell {
    sequence: u64,
    projects: Vec<ProjectRow>,
    threads: Vec<ThreadRow>,
    archive: Vec<ThreadRow>,
    archive_sequence: Option<u64>,
}

impl Shell {
    pub(crate) fn apply(&mut self, value: Value) -> Result<bool, serde_json::Error> {
        let changed = match serde_json::from_value(value)? {
            ShellItem::Snapshot {
                snapshot,
                resolved_repository_identity_roots,
            } => {
                if let Some(roots) = resolved_repository_identity_roots {
                    return Ok(self.apply_identity_refresh(&snapshot.projects, &roots));
                }
                if snapshot.snapshot_sequence < self.sequence {
                    return Ok(false);
                }
                self.sequence = snapshot.snapshot_sequence;
                let previous: HashMap<_, _> = self
                    .projects
                    .iter()
                    .map(|row| (row.project.id.as_str(), row))
                    .collect();
                self.projects = snapshot
                    .projects
                    .into_iter()
                    .map(|mut row| {
                        row.retain_repository_identity(
                            previous.get(row.project.id.as_str()).copied(),
                        );
                        row
                    })
                    .collect();
                self.threads = unique_threads(snapshot.threads, false);
                if self.archive_sequence.is_none() {
                    self.archive = unique_threads(snapshot.archived_threads, true);
                    self.remove_active_from_archive();
                } else if self.archive_not_newer_than(self.sequence) {
                    self.remove_active_from_archive();
                }
                true
            }
            ShellItem::ProjectUpdated {
                sequence,
                mut project,
            } => self.apply_delta(sequence, |shell| {
                project.retain_repository_identity(shell.project(&project.project.id));
                upsert(&mut shell.projects, *project, |row| &row.project.id)
            }),
            ShellItem::ProjectRemoved {
                sequence,
                project_id,
            } => self.apply_delta(sequence, |shell| {
                remove(&mut shell.projects, |row| row.project.id == project_id)
            }),
            ShellItem::ThreadUpdated {
                sequence,
                location,
                mut thread,
            } => self.apply_delta(sequence, |shell| {
                thread.thread.archived = matches!(location, ThreadLocation::Archive);
                let id = thread.thread.id.clone();
                if thread.thread.archived {
                    let removed = remove(&mut shell.threads, |row| row.thread.id == id);
                    let archived = shell.archive_not_newer_than(sequence)
                        && upsert(&mut shell.archive, *thread, |row| &row.thread.id);
                    removed || archived
                } else {
                    let unarchived = shell.archive_not_newer_than(sequence)
                        && remove(&mut shell.archive, |row| row.thread.id == id);
                    let updated = upsert(&mut shell.threads, *thread, |row| &row.thread.id);
                    unarchived || updated
                }
            }),
            ShellItem::ThreadRemoved {
                sequence,
                location,
                thread_id,
            } => self.apply_delta(sequence, |shell| match location {
                ThreadLocation::Active => {
                    remove(&mut shell.threads, |row| row.thread.id == thread_id)
                }
                ThreadLocation::Archive => {
                    shell.archive_not_newer_than(sequence)
                        && remove(&mut shell.archive, |row| row.thread.id == thread_id)
                }
            }),
            ShellItem::Ignored => false,
        };
        Ok(changed)
    }

    fn apply_identity_refresh(&mut self, candidates: &[ProjectRow], roots: &[String]) -> bool {
        let candidates: HashMap<_, _> = candidates
            .iter()
            .map(|row| (row.project.id.as_str(), row))
            .collect();
        let roots: HashSet<_> = roots.iter().map(String::as_str).collect();
        let mut changed = false;
        for project in &mut self.projects {
            let Some(candidate) = candidates
                .get(project.project.id.as_str())
                .filter(|row| row.project.path == project.project.path)
            else {
                continue;
            };
            let resolved_here = roots.contains(project.project.path.as_str());
            let fills_missing =
                project.repository_identity.is_none() && candidate.repository_identity.is_some();
            if (resolved_here || fills_missing)
                && project.repository_identity != candidate.repository_identity
            {
                project
                    .repository_identity
                    .clone_from(&candidate.repository_identity);
                changed = true;
            }
        }
        changed
    }

    pub(super) fn apply_archive_snapshot(
        &mut self,
        value: Value,
    ) -> Result<bool, serde_json::Error> {
        let snapshot: ArchiveSnapshot = serde_json::from_value(value)?;
        if snapshot.snapshot_sequence < self.sequence
            || !self.archive_not_newer_than(snapshot.snapshot_sequence)
        {
            return Ok(false);
        }
        self.archive_sequence = Some(snapshot.snapshot_sequence);
        self.archive = unique_threads(snapshot.threads, true);
        Ok(true)
    }

    fn archive_not_newer_than(&self, sequence: u64) -> bool {
        self.archive_sequence
            .is_none_or(|archive| sequence >= archive)
    }

    fn remove_active_from_archive(&mut self) {
        let active: HashSet<_> = self
            .threads
            .iter()
            .map(|row| row.thread.id.as_str())
            .collect();
        self.archive
            .retain(|row| !active.contains(row.thread.id.as_str()));
    }

    fn apply_delta(&mut self, sequence: u64, update: impl FnOnce(&mut Self) -> bool) -> bool {
        if sequence <= self.sequence {
            return false;
        }
        self.sequence = sequence;
        update(self)
    }

    pub(crate) fn view(&self) -> (Vec<Project>, Vec<Thread>) {
        let archived: HashSet<_> = self
            .archive
            .iter()
            .map(|row| row.thread.id.as_str())
            .collect();
        (
            self.projects
                .iter()
                .map(|row| row.project.clone())
                .collect(),
            self.threads
                .iter()
                .filter(|row| !archived.contains(row.thread.id.as_str()))
                .chain(self.archive.iter())
                .filter(|row| row.listed())
                .map(|row| row.thread.clone())
                .collect(),
        )
    }

    pub(super) fn project(&self, project_id: &str) -> Option<&ProjectRow> {
        self.projects
            .iter()
            .find(|row| row.project.id == project_id)
    }
}

fn unique_threads(threads: Vec<ThreadRow>, archived: bool) -> Vec<ThreadRow> {
    let mut seen = HashSet::new();
    threads
        .into_iter()
        .filter_map(|mut row| {
            row.thread.archived = archived;
            seen.insert(row.thread.id.clone()).then_some(row)
        })
        .collect()
}

fn upsert<T: Eq>(rows: &mut Vec<T>, row: T, id: impl Fn(&T) -> &str) -> bool {
    if let Some(existing) = rows.iter_mut().find(|existing| id(existing) == id(&row)) {
        if *existing == row {
            return false;
        }
        *existing = row;
    } else {
        rows.push(row);
    }
    true
}

fn remove<T>(rows: &mut Vec<T>, matches: impl Fn(&T) -> bool) -> bool {
    let count = rows.len();
    rows.retain(|row| !matches(row));
    count != rows.len()
}

#[derive(Deserialize)]
struct ThreadProjection {
    messages: Vec<Message>,
    #[serde(default)]
    runs: Vec<TimelineRun>,
    #[serde(rename = "visibleTurnItems")]
    visible_turn_items: Option<Vec<TimelineRow>>,
    #[serde(default, rename = "runtimeRequests")]
    runtime_requests: Vec<RuntimeRequest>,
    #[serde(default, rename = "turnItems")]
    turn_items: Vec<RuntimeRequestItem>,
}

impl ThreadProjection {
    fn take_runtime_requests(&mut self) -> (ApprovalProjection, QuestionProjection) {
        let mut approvals = ApprovalProjection::default();
        let mut questions = QuestionProjection::default();
        for request in self.runtime_requests.drain(..) {
            if request.kind == "user_input" {
                questions.request(request);
            } else {
                approvals.request(request);
            }
        }
        for item in self.turn_items.drain(..) {
            match item {
                RuntimeRequestItem::ApprovalRequest(details) => {
                    approvals.detail(details);
                }
                RuntimeRequestItem::UserInputRequest(details) => {
                    questions.detail(details);
                }
                RuntimeRequestItem::Other => {}
            }
        }
        (approvals, questions)
    }

    fn visible_entries(self) -> Vec<ConversationEntry> {
        let Some(rows) = self.visible_turn_items else {
            return self
                .messages
                .into_iter()
                .map(ConversationEntry::Message)
                .collect();
        };
        let mut messages = self
            .messages
            .into_iter()
            .map(|message| (message.id.clone(), message))
            .collect::<HashMap<_, _>>();
        rows.into_iter()
            .filter_map(|row| {
                let (role, entry) = match row.item {
                    TimelineItem::UserMessage(entry) => ("user", entry),
                    TimelineItem::AssistantMessage(entry) => ("assistant", entry),
                    TimelineItem::TodoList(list) => return Some(ConversationEntry::TodoList(list)),
                    TimelineItem::CommandExecution(mut command) => {
                        if command.is_workspace_preparation() {
                            return None;
                        }
                        command.set_source(row.source_thread_id, row.source_item_id);
                        return Some(ConversationEntry::Command(command));
                    }
                    _ => return None,
                };
                if let Some(mut message) = messages.remove(&entry.message_id) {
                    if let Some(text) = entry.text {
                        message.text = text;
                    }
                    Some(ConversationEntry::Message(message))
                } else {
                    entry.into_message(role).map(ConversationEntry::Message)
                }
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct TimelineRow {
    item: TimelineItem,
    #[serde(rename = "sourceThreadId")]
    source_thread_id: Option<String>,
    #[serde(rename = "sourceItemId")]
    source_item_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TimelineItem {
    UserMessage(TimelineMessage),
    AssistantMessage(TimelineMessage),
    TodoList(TodoListEntry),
    CommandExecution(Box<CommandEntry>),
    ApprovalRequest(ApprovalDetails),
    UserInputRequest(QuestionDetails),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RuntimeRequestItem {
    ApprovalRequest(ApprovalDetails),
    UserInputRequest(QuestionDetails),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineMessage {
    message_id: String,
    text: Option<String>,
    created_by: Option<String>,
}

impl TimelineMessage {
    fn into_message(self, role: &str) -> Option<Message> {
        Some(Message {
            id: self.message_id,
            role: role.into(),
            text: self.text?,
            created_by: self.created_by,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ThreadEvent {
    #[serde(rename = "message.updated")]
    MessageUpdated { payload: Message },
    #[serde(rename = "turn-item.updated")]
    ItemUpdated { payload: TimelineItem },
    #[serde(rename = "run.updated")]
    RunUpdated { payload: TimelineRun },
    #[serde(rename = "runtime-request.updated")]
    RuntimeRequestUpdated { payload: RuntimeRequest },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
enum ThreadItem {
    Snapshot {
        snapshot_sequence: u64,
        projection: ThreadProjection,
    },
    Event {
        sequence: u64,
        event: ThreadEvent,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Default)]
pub(crate) struct Conversation {
    sequence: u64,
    transcript: Transcript,
    queued: Vec<QueuedMessage>,
    pub(super) approvals: ApprovalProjection,
    pub(super) questions: QuestionProjection,
}

impl Conversation {
    pub(crate) fn apply(&mut self, value: Value) -> Result<bool, serde_json::Error> {
        let queued = (value["kind"] == "snapshot"
            && value["snapshotSequence"]
                .as_u64()
                .is_some_and(|sequence| sequence >= self.sequence))
        .then(|| queue::snapshot(&value["projection"]));
        let changed = match serde_json::from_value(value)? {
            ThreadItem::Snapshot {
                snapshot_sequence,
                mut projection,
            } if snapshot_sequence >= self.sequence => {
                self.sequence = snapshot_sequence;
                self.queued = queued.unwrap_or_default();
                (self.approvals, self.questions) = projection.take_runtime_requests();
                let runs = std::mem::take(&mut projection.runs);
                self.transcript.replace(projection.visible_entries(), runs);
                true
            }
            ThreadItem::Event { sequence, event } if sequence > self.sequence => {
                self.sequence = sequence;
                match event {
                    ThreadEvent::RunUpdated { payload } => self.transcript.update_run(payload),
                    ThreadEvent::MessageUpdated { payload } => {
                        self.transcript.upsert(ConversationEntry::Message(payload))
                    }
                    ThreadEvent::ItemUpdated {
                        payload: TimelineItem::AssistantMessage(entry),
                    } => entry.into_message("assistant").is_some_and(|message| {
                        self.transcript.upsert(ConversationEntry::Message(message))
                    }),
                    ThreadEvent::ItemUpdated {
                        payload: TimelineItem::TodoList(list),
                    } => self.transcript.upsert(ConversationEntry::TodoList(list)),
                    ThreadEvent::ItemUpdated {
                        payload: TimelineItem::CommandExecution(command),
                    } => {
                        !command.is_workspace_preparation()
                            && self.transcript.upsert(ConversationEntry::Command(command))
                    }
                    ThreadEvent::ItemUpdated {
                        payload: TimelineItem::ApprovalRequest(details),
                    } => self.approvals.detail(details),
                    ThreadEvent::ItemUpdated {
                        payload: TimelineItem::UserInputRequest(details),
                    } => self.questions.detail(details),
                    ThreadEvent::RuntimeRequestUpdated { payload }
                        if payload.kind == "user_input" =>
                    {
                        self.questions.request(payload)
                    }
                    ThreadEvent::RuntimeRequestUpdated { payload } => {
                        self.approvals.request(payload)
                    }
                    ThreadEvent::ItemUpdated { .. } => false,
                    ThreadEvent::Ignored => false,
                }
            }
            _ => false,
        };
        Ok(changed)
    }

    pub(crate) fn entries(&self) -> Vec<ConversationEntry> {
        self.transcript.entries()
    }

    pub(crate) fn queued(&self) -> &[QueuedMessage] {
        &self.queued
    }

    #[cfg(test)]
    pub(crate) fn messages(&self) -> Vec<Message> {
        self.transcript
            .entries()
            .iter()
            .filter_map(ConversationEntry::message)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Answer, QuestionResponse};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn live_question_waits_for_its_details_and_rejects_invalid_or_stale_responses() {
        let active = json!({"id":"q", "kind":"user_input", "status":"pending",
            "createdAt":"2026-11-22T13:00:00Z", "responseCapability":{"type":"live"}});
        let details = json!({"type":"user_input_request", "requestId":"q", "status":"waiting",
            "questions":[{"id":"choice", "header":"Pick", "question":"Which one?", "allowCustomAnswer":false,
                "options":[{"label":"First", "description":"Description", "value":"first"}]}]});
        let snapshot = |sequence| {
            json!({"kind":"snapshot", "snapshotSequence":sequence,
            "projection":{"messages":[], "runtimeRequests":[active], "turnItems":[details]}})
        };
        let mut conversation = Conversation::default();
        assert!(
            conversation
                .apply(json!({"kind":"event", "sequence":1, "event":{
            "type":"runtime-request.updated", "payload":active}}))
                .unwrap()
        );
        assert!(conversation.questions.visible().is_empty());
        assert!(conversation.apply(snapshot(2)).unwrap());
        assert_eq!(
            conversation.questions.visible()[0].questions[0].options[0].label,
            "First"
        );
        let mut response = QuestionResponse {
            thread_id: "thread".into(),
            request_id: "q".into(),
            command_id: Uuid::new_v4(),
            answers: [("choice".into(), Answer::Text("first".into()))].into(),
        };
        assert!(conversation.questions.allows(&response));
        assert_eq!(response.payload()["answers"], json!({"choice":"first"}));
        response
            .answers
            .insert("choice".into(), Answer::Text("First".into()));
        assert!(!conversation.questions.allows(&response));
        let mut resolved = active.clone();
        resolved["status"] = "resolved".into();
        assert!(
            conversation
                .apply(json!({"kind":"event", "sequence":3, "event":{
            "type":"runtime-request.updated", "payload":resolved}}))
                .unwrap()
        );
        assert!(!conversation.apply(snapshot(2)).unwrap());
        assert!(conversation.questions.visible().is_empty());
    }

    #[test]
    fn conversation_uses_wire_timeline_order_and_renders_streaming_text_once() {
        let mut conversation = Conversation::default();
        assert!(conversation.apply(json!({
            "kind": "snapshot", "snapshotSequence": 5,
            "projection": {
                "messages": [
                    {"id":"hidden", "role":"user", "text":"Cancelled"},
                    {"id":"prompt", "role":"user", "text":"Existing prompt", "createdBy":"agent"}
                ],
                "visibleTurnItems": [
                    {"item":{"type":"user_message", "messageId":"prompt", "text":"Existing prompt"}},
                    {"item":{"type":"assistant_message", "messageId":"reply", "text":"Working"}},
                    {"item":{"type":"command_execution", "command":"ls"}}
                ]
            },
        })).unwrap());
        assert_eq!(
            conversation.messages()[0].created_by.as_deref(),
            Some("agent")
        );
        assert_eq!(conversation.messages()[1].text, "Working");
        assert_eq!(conversation.messages().len(), 2);
        assert!(conversation.apply(json!({
            "kind": "event", "sequence": 6,
            "event": {"type":"turn-item.updated", "payload":{"type":"assistant_message", "messageId":"reply", "text":"Done"}},
        })).unwrap());
        assert_eq!(conversation.messages()[1].text, "Done");
        assert_eq!(conversation.messages().len(), 2);
    }

    #[test]
    fn task_list_updates_keep_the_original_position_between_visible_messages() {
        let task = |status| {
            json!({"type":"todo_list", "id":"task-list", "runId":"run", "steps":[
                {"id":"step-1", "text":"Inspect package.json", "status":status},
            ]})
        };
        let mut conversation = Conversation::default();
        assert!(conversation.apply(json!({
            "kind":"snapshot", "snapshotSequence":8,"projection":{
                "messages": [], "visibleTurnItems": [
                    {"item":{"type":"user_message", "messageId":"prompt", "text":"Start"}},
                    {"item":task("running")},
                    {"item":{"type":"assistant_message", "messageId":"reply", "text":"Done"}},
                ],
            },
        })).unwrap());
        assert!(conversation.apply(json!({
            "kind":"event", "sequence":9,"event":{"type":"turn-item.updated", "payload":task("completed")},
        })).unwrap());
        let entries = conversation.entries();
        let [
            ConversationEntry::Message(first),
            ConversationEntry::TodoList(list),
            ConversationEntry::Message(last),
        ] = entries.as_slice()
        else {
            panic!("expected the task list between the two messages");
        };
        assert_eq!(first.id, "prompt");
        assert_eq!(list.steps[0].status, TodoStepStatus::Completed);
        assert!(list.completed_work.is_none());
        assert_eq!(last.id, "reply");
        assert!(conversation.apply(json!({
            "kind":"event", "sequence":10,"event":{"type":"run.updated", "payload":{
                "id":"run", "status":"completed",
                "startedAt":"2026-11-22T00:00:00.000Z", "completedAt":"2026-11-22T00:00:03.154Z",
            }},
        })).unwrap());
        let entries = conversation.entries();
        assert!(matches!(&entries[1], ConversationEntry::TodoList(list) if
            list.completed_work.as_ref().is_some_and(|work| work.first_row && work.elapsed_ms == Some(3154))));
    }

    #[test]
    fn project_updates_and_archiving_preserve_the_rest_of_the_shell() {
        let mut shell = Shell::default();
        let project = json!({"id": "p", "title": "Old name", "workspaceRoot": "/workspace"});
        let thread = json!({"id": "t", "projectId": "p", "title": "First", "status": "idle"});
        let archived = json!({"id": "old", "projectId": "p", "title": "Archived"});
        let child = json!({
            "id": "child", "projectId": "p", "title": "Child", "status": "running",
            "lineage": {"relationshipToParent": "subagent"}
        });
        assert!(shell.apply(json!({"kind": "snapshot", "snapshot": {
            "snapshotSequence": 5, "projects": [project.clone()], "threads": [thread.clone(), child],
            "archivedThreads": [archived],
        }})).unwrap());
        assert_eq!(shell.view().1[0].id, "t");
        assert_eq!(shell.view().1.len(), 2);
        assert!(shell.view().1[1].archived);
        assert!(shell.threads.iter().any(|row| row.thread.id == "child"));

        assert!(
            !shell
                .apply(json!({"kind": "snapshot", "snapshot": {
            "snapshotSequence": 6, "projects": [project], "threads": [],
        }, "resolvedRepositoryIdentityRoots": ["/workspace"]}))
                .unwrap()
        );
        assert_eq!(shell.view().1.len(), 2);

        assert!(
            shell
                .apply(json!({"kind": "project.updated", "sequence": 6,
                    "project": {"id": "p", "title": "Renamed", "workspaceRoot": "/workspace"}
                }))
                .unwrap()
        );
        assert!(
            shell
                .apply(json!({"kind": "thread.updated", "sequence": 7,
                    "location": "archive", "thread": thread.clone()
                }))
                .unwrap()
        );
        assert!(
            !shell
                .apply(json!({"kind": "thread.updated", "sequence": 6,
                    "location": "active", "thread": thread
                }))
                .unwrap()
        );
        let (projects, threads) = shell.view();
        assert_eq!(projects[0].name, "Renamed");
        assert_eq!(threads.len(), 2);
        assert!(threads.iter().all(|thread| thread.archived));
        assert!(
            !shell
                .apply(json!({"kind": "thread.removed", "sequence": 8,
                    "location": "active", "threadId": "t"
                }))
                .unwrap()
        );
        assert!(
            shell
                .apply(json!({"kind": "thread.removed", "sequence": 9,
                    "location": "archive", "threadId": "t"
                }))
                .unwrap()
        );
        assert_eq!(shell.view().1[0].id, "old");
    }

    #[test]
    fn marked_repository_refresh_updates_only_existing_same_root_metadata() {
        let mut shell = Shell::default();
        let identity = |name: &str| {
            json!({"canonicalKey": format!("example.test/team/{name}"), "locator": {
                "source": "git-remote", "remoteName": "origin",
                "remoteUrl": format!("https://example.test/team/{name}.git"),
            }})
        };
        let previous = identity("previous");
        let resolved = identity("resolved");
        let project = |id: &str, root: &str, title: &str, repository: Value| json!({"id":id, "title":title, "workspaceRoot":root, "repositoryIdentity":repository});
        let thread = json!({"id":"latest", "projectId":"p", "title":"Latest thread"});
        assert!(
            shell
                .apply(json!({"kind":"snapshot", "snapshot": {
                    "snapshotSequence": 8,
                    "projects": [project("p", "/p", "Latest", Value::Null),
                                 project("q", "/q", "Other project", previous.clone())],
                    "threads": [thread],
                }}))
                .unwrap()
        );

        let marked = |projects: Vec<Value>, roots: &[&str]| {
            json!({"kind":"snapshot", "resolvedRepositoryIdentityRoots": roots, "snapshot": {
                "snapshotSequence":4, "projects":projects, "threads":[], "archivedThreads":[],
            }})
        };
        let refresh = marked(
            vec![
                project("p", "/p", "Stale title", resolved.clone()),
                project("q", "/changed", "Wrong root", Value::Null),
                project("r", "/r", "Absent", resolved.clone()),
            ],
            &["/p", "/q", "/changed", "/r"],
        );
        assert!(shell.apply(refresh.clone()).unwrap());
        assert!(!shell.apply(refresh).unwrap());
        assert_eq!(shell.sequence, 8);
        assert_eq!(shell.view().1[0].id, "latest");
        assert_eq!(shell.project("p").unwrap().project.name, "Latest");
        assert_eq!(
            shell.project("p").unwrap().repository_identity,
            Some(resolved.clone())
        );
        assert_eq!(
            shell.project("q").unwrap().repository_identity,
            Some(previous.clone())
        );
        assert!(shell.project("r").is_none());

        assert!(
            shell
                .apply(marked(
                    vec![
                        project("p", "/p", "Ignored", Value::Null),
                        project("q", "/q", "Ignored", Value::Null),
                    ],
                    &["/p"]
                ))
                .unwrap()
        );
        assert!(shell.project("p").unwrap().repository_identity.is_none());
        assert!(
            shell
                .apply(marked(
                    vec![
                        project("p", "/p", "Ignored", resolved.clone()),
                        project("q", "/q", "Ignored", resolved),
                    ],
                    &[]
                ))
                .unwrap()
        );
        assert!(shell.project("p").unwrap().repository_identity.is_some());
        assert_eq!(
            shell.project("q").unwrap().repository_identity,
            Some(previous)
        );
    }

    #[test]
    fn authoritative_project_updates_preserve_unresolved_same_root_identity() {
        let mut shell = Shell::default();
        let identity = json!({"canonicalKey":"example.test/team/repo", "locator": {
            "source":"git-remote", "remoteName":"origin",
            "remoteUrl":"https://example.test/team/repo.git",
        }});
        let project = |title: &str, root: &str, repository: Value| json!({"id":"p", "title":title, "workspaceRoot":root, "repositoryIdentity":repository});
        let snapshot = |sequence: u64, project: Value| {
            json!({"kind":"snapshot", "snapshot": {
                "snapshotSequence":sequence, "projects":[project], "threads":[],
            }})
        };
        assert!(
            shell
                .apply(snapshot(5, project("First", "/root", identity.clone())))
                .unwrap()
        );
        assert!(
            shell
                .apply(snapshot(6, project("Second", "/root", Value::Null)))
                .unwrap()
        );
        assert!(
            shell
                .apply(json!({"kind":"project.updated", "sequence":7,
                    "project":project("Current", "/root", Value::Null),
                }))
                .unwrap()
        );
        let current = shell.project("p").unwrap();
        assert_eq!(current.project.name, "Current");
        assert_eq!(current.repository_identity, Some(identity));
        assert!(
            shell
                .apply(json!({"kind":"project.updated", "sequence":8,
                    "project":project("Moved", "/other", Value::Null),
                }))
                .unwrap()
        );
        assert!(shell.project("p").unwrap().repository_identity.is_none());
    }

    #[test]
    fn archive_snapshot_preserves_the_active_cursor_and_wins_over_delayed_archive_changes() {
        let mut shell = Shell::default();
        let thread = json!({"id": "t", "projectId": "p", "title": "Before archive"});
        let archived = json!({"id": "t", "projectId": "p", "title": "Archived"});
        assert!(
            shell
                .apply(json!({"kind": "snapshot", "snapshot": {
                    "snapshotSequence": 5, "projects": [], "threads": [thread],
                }}))
                .unwrap()
        );
        let archive = json!({"snapshotSequence": 8, "projects": [], "threads": [archived]});
        assert!(shell.apply_archive_snapshot(archive.clone()).unwrap());
        assert_eq!(shell.sequence, 5);
        assert!(shell.view().1[0].archived);

        assert!(
            !shell
                .apply(json!({"kind": "thread.removed", "sequence": 7,
                    "location": "archive", "threadId": "t",
                }))
                .unwrap()
        );
        assert_eq!(shell.view().1[0].title, "Archived");
        assert!(shell.apply(json!({"kind": "thread.updated", "sequence": 9,
            "location": "active", "thread": {"id": "t", "projectId": "p", "title": "Unarchived"},
        })).unwrap());
        assert!(!shell.apply_archive_snapshot(archive).unwrap());
        assert_eq!(shell.view().1[0].title, "Unarchived");
        assert!(!shell.view().1[0].archived);
    }

    #[test]
    fn shell_activity_uses_valid_wire_timestamps_before_the_update_fallback() {
        let row: ThreadRow = serde_json::from_value(json!({
            "id": "thread", "projectId": "project", "title": "Conversation",
            "updatedAt": "2026-11-22T13:15:00Z",
            "latestUserMessageAt": "invalid",
            "latestRunRequestedAt": "2026-11-22T13:01:00Z",
            "latestRunStartedAt": null,
            "latestRunCompletedAt": "2026-11-22T14:03:00+01:00"
        }))
        .unwrap();
        let fallback: ThreadRow = serde_json::from_value(json!({
            "id": "thread", "projectId": "project", "title": "Conversation",
            "updatedAt": "2026-11-22T13:15:00Z"
        }))
        .unwrap();
        let timestamp = |time| Some(OffsetDateTime::parse(time, &Rfc3339).unwrap());
        assert_eq!(
            row.thread.activity_timestamp(),
            timestamp("2026-11-22T13:03:00Z")
        );
        assert_eq!(
            fallback.thread.activity_timestamp(),
            timestamp("2026-11-22T13:15:00Z")
        );
    }

    #[test]
    fn command_approval_joins_snapshot_details_and_rejects_stale_updates() {
        use super::super::ApprovalDecision;

        let mut thread = Conversation::default();
        let mut request = json!({
            "id": "approval", "kind": "command", "status": "pending",
            "createdAt": "2026-11-22T12:00:00Z",
            "responseCapability": {"type": "live", "providerSessionId": "provider"}
        });
        let snapshot = json!({"kind": "snapshot", "snapshotSequence": 10, "projection": {
            "messages": [], "runtimeRequests": [request.clone()], "turnItems": [{
                "type": "approval_request", "status": "waiting", "requestId": "approval",
                "requestKind": "command", "prompt": "git status",
                "options": [{"decision": "accept", "label": "Allow once"},
                    {"decision": "acceptAlways", "label": "Always allow"}]
            }]
        }});
        assert!(thread.apply(snapshot.clone()).unwrap());
        let visible = thread.approvals.visible();
        assert_eq!(visible[0].command.as_deref(), Some("git status"));
        assert_eq!(visible[0].choices[0].label, "Allow once");
        assert!(
            thread
                .approvals
                .allows("approval", ApprovalDecision::Accept)
        );
        assert!(
            !thread
                .approvals
                .allows("approval", ApprovalDecision::Decline)
        );
        request["status"] = json!("resolved");
        assert!(
            thread
                .apply(json!({"kind": "event", "sequence": 11, "event": {
                    "type": "runtime-request.updated", "payload": request
                }}))
                .unwrap()
        );
        assert!(!thread.apply(snapshot).unwrap());
        assert!(thread.approvals.visible().is_empty());
    }

    #[test]
    fn message_updates_replace_text_in_place_and_ignore_replayed_sequences() {
        let mut thread = Conversation::default();
        assert!(
            thread
                .apply(json!({"kind": "snapshot", "snapshotSequence": 10,
                    "projection": {"messages": [
                        {"id": "1", "role": "user", "text": "Question", "createdBy": "agent"},
                        {"id": "2", "role": "assistant", "text": "A"},
                        {"id": "2", "role": "assistant", "text": "An"}
                    ]}
                }))
                .unwrap()
        );
        assert!(thread.apply(json!({"kind": "event", "sequence": 11, "event": {
            "type": "message.updated", "payload": {"id": "2", "role": "assistant", "text": "Answer"}
        }})).unwrap());
        assert!(thread.apply(json!({"kind": "event", "sequence": 12, "event": {
            "type": "message.updated", "payload": {"id": "3", "role": "user", "text": "Follow-up"}
        }})).unwrap());
        assert!(!thread.apply(json!({"kind": "event", "sequence": 10, "event": {
            "type": "message.updated", "payload": {"id": "2", "role": "assistant", "text": "Stale"}
        }})).unwrap());
        let messages = thread.messages();
        assert_eq!(messages[0].created_by.as_deref(), Some("agent"));
        assert!(messages[1].created_by.is_none());
        let transcript: Vec<_> = messages.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(transcript, ["Question", "Answer", "Follow-up"]);
    }
}
