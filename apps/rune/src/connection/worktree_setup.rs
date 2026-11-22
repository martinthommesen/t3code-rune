use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorktreeSetup {
    pub(crate) thread_id: String,
    pub(crate) phase: Phase,
    pub(crate) started_at: String,
    pub(crate) ended_at: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) base_ref: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) setup_script: Option<Script>,
    pub(crate) stages: Vec<Stage>,
    pub(crate) error: Option<String>,
    pub(crate) sequence: u64,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Script {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) terminal_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Stage {
    pub(crate) id: StageId,
    pub(crate) status: Status,
    pub(crate) started_at: Option<String>,
    pub(crate) ended_at: Option<String>,
    pub(crate) percent: Option<u8>,
    pub(crate) detail: Option<String>,
    pub(crate) tail: Vec<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StageId {
    Fetch,
    Checkout,
    Submodules,
    SetupScript,
    Agent,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Pending,
    Running,
    Done,
    Skipped,
    Warning,
    Failed,
}
