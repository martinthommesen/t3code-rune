use serde::Deserialize;

use t3_rune::providers::codex::ReasoningEffort;

use crate::connection::ModelSelection;

#[derive(Deserialize)]
pub(super) struct ModelSwitch {
    from: ModelSelection,
    to: ModelSelection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReasoningChange {
    from: ReasoningEffort,
    to: ReasoningEffort,
}

impl ModelSwitch {
    pub(super) fn reasoning_change(self) -> Option<ReasoningChange> {
        let from = self.from.reasoning_effort()?;
        let to = self.to.reasoning_effort()?;
        (from != to).then_some(ReasoningChange { from, to })
    }
}

impl ReasoningChange {
    pub(crate) fn label(self) -> String {
        format!("{} → {}", self.from.label(), self.to.label())
    }
}
