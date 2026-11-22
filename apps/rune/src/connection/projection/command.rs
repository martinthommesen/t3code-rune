use serde::Deserialize;

use super::CompletedWork;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandOutputTarget {
    pub(crate) thread_id: String,
    pub(crate) item_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandEntry {
    pub(crate) id: String,
    thread_id: String,
    pub(crate) run_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) input: String,
    pub(crate) output: Option<String>,
    #[serde(default)]
    pub(crate) output_omitted: bool,
    #[serde(default)]
    pub(crate) output_indicates_failure: bool,
    pub(crate) exit_code: Option<i64>,
    pub(crate) status: CommandStatus,
    pub(crate) updated_at: Option<String>,
    provider_turn_id: Option<String>,
    #[serde(skip)]
    pub(crate) completed_work: Option<CompletedWork>,
    #[serde(skip)]
    pub(crate) group: Option<CommandGroup>,
    #[serde(skip)]
    source: Option<CommandOutputTarget>,
}

impl CommandEntry {
    pub(super) fn set_source(&mut self, thread_id: Option<String>, item_id: Option<String>) {
        if let (Some(thread_id), Some(item_id)) = (thread_id, item_id) {
            self.source = Some(CommandOutputTarget { thread_id, item_id });
        }
    }

    pub(crate) fn target(&self) -> CommandOutputTarget {
        self.source.clone().unwrap_or_else(|| CommandOutputTarget {
            thread_id: self.thread_id.clone(),
            item_id: self.id.clone(),
        })
    }

    pub(crate) fn failed(&self) -> bool {
        self.status == CommandStatus::Failed
            || self
                .exit_code
                .map_or(self.output_indicates_failure, |code| code != 0)
    }

    pub(super) fn is_workspace_preparation(&self) -> bool {
        self.provider_turn_id.is_none() && self.input == "Preparing workspace"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandGroup {
    pub(crate) id: String,
    pub(crate) first: bool,
    pub(crate) count: usize,
    pub(crate) active_input: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CommandStatus {
    Idle,
    Pending,
    Running,
    Waiting,
    Completed,
    Cancelled,
    Interrupted,
    Failed,
    #[serde(other)]
    Unknown,
}

impl CommandStatus {
    pub(crate) fn is_active(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Pending | Self::Running | Self::Waiting
        )
    }

    pub(crate) fn is_stopped(self) -> bool {
        matches!(self, Self::Cancelled | Self::Interrupted)
    }

    pub(super) fn shows_progress(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }
}
