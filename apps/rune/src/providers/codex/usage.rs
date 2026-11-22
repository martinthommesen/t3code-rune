use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{SettlementStatus, TurnId};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeTokenCounts {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageSnapshot {
    pub total: NativeTokenCounts,
    pub last: NativeTokenCounts,
    pub model_context_window: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnContextUsage {
    pub used_tokens: u64,
    pub max_tokens: Option<u64>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub updated_at: String,
}

impl TokenUsageSnapshot {
    pub fn context_usage(self, updated_at: String) -> TurnContextUsage {
        TurnContextUsage {
            used_tokens: self.last.total_tokens,
            max_tokens: self.model_context_window,
            input_tokens: self.last.input_tokens,
            cached_input_tokens: self.last.cached_input_tokens,
            output_tokens: self.last.output_tokens,
            reasoning_output_tokens: self.last.reasoning_output_tokens,
            updated_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UsageScope {
    MainAgent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnTokenUsage {
    pub usage_status: UsageStatus,
    usage_scope: UsageScope,
    pub has_subagents: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(flatten)]
    pub totals: Option<TokenTotals>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

impl TurnTokenUsage {
    pub(super) fn unavailable() -> Self {
        Self {
            usage_status: UsageStatus::Unavailable,
            usage_scope: UsageScope::MainAgent,
            has_subagents: false,
            models: None,
            totals: None,
        }
    }
}

#[derive(Default)]
pub(super) struct ThreadUsage {
    baseline: Option<NativeTokenCounts>,
    active: Option<ActiveUsage>,
    known_model: Option<String>,
    rerouted_models: HashMap<TurnId, String>,
}

struct ActiveUsage {
    turn_id: TurnId,
    totals: TokenTotals,
    observed: bool,
    has_subagents: bool,
}

impl ThreadUsage {
    pub(super) fn begin(&mut self, turn_id: &TurnId) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.turn_id == *turn_id)
        {
            return;
        }
        self.active = Some(ActiveUsage {
            turn_id: turn_id.clone(),
            totals: TokenTotals {
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_tokens: Some(0),
                output_tokens: 0,
                reasoning_tokens: 0,
            },
            observed: false,
            has_subagents: false,
        });
    }

    pub(super) fn update(&mut self, turn_id: &TurnId, snapshot: TokenUsageSnapshot) {
        let previous = self.baseline.replace(snapshot.total);
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.turn_id == *turn_id)
        else {
            return;
        };
        let current = snapshot.total;
        let delta = previous
            .and_then(|previous| current.difference(previous))
            .unwrap_or_else(|| snapshot.last.into());
        active.observed |= delta.input_tokens > 0
            || delta.cached_input_tokens > 0
            || delta.output_tokens > 0
            || delta.reasoning_tokens > 0;
        let totals = &mut active.totals;
        totals.input_tokens = totals.input_tokens.saturating_add(delta.input_tokens);
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(delta.cached_input_tokens);
        totals.output_tokens = totals.output_tokens.saturating_add(delta.output_tokens);
        totals.reasoning_tokens = totals
            .reasoning_tokens
            .saturating_add(delta.reasoning_tokens);
        totals.cache_creation_tokens = totals
            .cache_creation_tokens
            .zip(delta.cache_creation_tokens)
            .map(|(total, delta)| total.saturating_add(delta));
    }

    pub(super) fn model(&mut self, model: String) {
        self.known_model = Some(model);
    }

    pub(super) fn reroute(&mut self, turn_id: &TurnId, model: String) {
        self.rerouted_models.insert(turn_id.clone(), model);
        self.known_model = None;
    }

    pub(super) fn note_subagent(&mut self, turn_id: &TurnId) {
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.turn_id == *turn_id)
        {
            active.has_subagents = true;
        }
    }

    pub(super) fn finish(&mut self, turn_id: &TurnId, status: SettlementStatus) -> TurnTokenUsage {
        let model = self
            .rerouted_models
            .remove(turn_id)
            .or_else(|| self.known_model.clone());
        let models = model.map(|model| vec![model]);
        let mut result = TurnTokenUsage {
            models,
            ..TurnTokenUsage::unavailable()
        };
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.turn_id != *turn_id)
        {
            return result;
        }
        let active = self
            .active
            .take()
            .expect("the active Codex turn was just checked");
        result.has_subagents = active.has_subagents;
        if active.observed {
            let mut totals = active.totals;
            totals.cached_input_tokens = totals.cached_input_tokens.min(totals.input_tokens);
            totals.cache_creation_tokens = totals
                .cache_creation_tokens
                .map(|value| value.min(totals.input_tokens));
            totals.reasoning_tokens = totals.reasoning_tokens.min(totals.output_tokens);
            result.usage_status = if status == SettlementStatus::Completed {
                UsageStatus::Complete
            } else {
                UsageStatus::Partial
            };
            result.totals = Some(totals);
        }
        result
    }
}

impl NativeTokenCounts {
    fn difference(self, previous: Self) -> Option<TokenTotals> {
        Some(TokenTotals {
            input_tokens: self.input_tokens.checked_sub(previous.input_tokens)?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_sub(previous.cached_input_tokens)?,
            cache_creation_tokens: self
                .cache_write_input_tokens
                .zip(previous.cache_write_input_tokens)
                .and_then(|(current, previous)| current.checked_sub(previous)),
            output_tokens: self.output_tokens.checked_sub(previous.output_tokens)?,
            reasoning_tokens: self
                .reasoning_output_tokens
                .checked_sub(previous.reasoning_output_tokens)?,
        })
    }
}

impl From<NativeTokenCounts> for TokenTotals {
    fn from(counts: NativeTokenCounts) -> Self {
        Self {
            input_tokens: counts.input_tokens,
            cached_input_tokens: counts.cached_input_tokens,
            cache_creation_tokens: counts.cache_write_input_tokens,
            output_tokens: counts.output_tokens,
            reasoning_tokens: counts.reasoning_output_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(
        input: u64,
        cached: u64,
        writes: Option<u64>,
        output: u64,
        reasoning: u64,
    ) -> NativeTokenCounts {
        NativeTokenCounts {
            total_tokens: input + output,
            input_tokens: input,
            cached_input_tokens: cached,
            cache_write_input_tokens: writes,
            output_tokens: output,
            reasoning_output_tokens: reasoning,
        }
    }

    #[test]
    fn uses_cumulative_baseline_resets_and_model_reroutes() {
        let mut usage = ThreadUsage::default();
        let prior = TurnId::new("prior");
        let current = TurnId::new("current");
        usage.model("original".into());
        usage.begin(&current);
        usage.update(
            &prior,
            TokenUsageSnapshot {
                total: counts(10, 5, Some(0), 9, 2),
                last: counts(7, 6, None, 4, 1),
                model_context_window: None,
            },
        );
        usage.update(
            &current,
            TokenUsageSnapshot {
                total: counts(14, 11, Some(7), 12, 6),
                last: counts(13, 8, Some(3), 6, 2),
                model_context_window: None,
            },
        );
        usage.update(
            &current,
            TokenUsageSnapshot {
                total: counts(2, 1, Some(1), 1, 0),
                last: counts(3, 2, Some(2), 1, 1),
                model_context_window: None,
            },
        );
        usage.reroute(&current, "rerouted".into());
        usage.note_subagent(&current);
        let result = usage.finish(&current, SettlementStatus::Interrupted);
        assert_eq!(result.usage_status, UsageStatus::Partial);
        assert_eq!(result.models.as_deref(), Some(&["rerouted".to_owned()][..]));
        assert!(result.has_subagents);
        assert_eq!(
            result.totals,
            Some(TokenTotals {
                input_tokens: 7,
                cached_input_tokens: 7,
                cache_creation_tokens: Some(7),
                output_tokens: 4,
                reasoning_tokens: 4,
            })
        );

        let future = TurnId::new("future");
        usage.begin(&future);
        assert_eq!(
            usage.finish(&future, SettlementStatus::Completed),
            TurnTokenUsage::unavailable(),
        );
    }
}
