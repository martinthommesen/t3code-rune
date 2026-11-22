use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub(super) type Layouts = BTreeMap<String, BTreeMap<String, SavedTerminalLayout>>;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct SavedTerminalLayout {
    pub(crate) terminal_open: bool,
    pub(crate) terminal_ids: Vec<String>,
    pub(crate) active_terminal_id: String,
    pub(crate) terminal_groups: Vec<SavedTerminalGroup>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedTerminalGroup {
    pub(crate) id: String,
    pub(crate) terminal_ids: Vec<String>,
    #[serde(default)]
    pub(crate) split_direction: SplitDirection,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SplitDirection {
    #[default]
    Horizontal,
    Vertical,
}
