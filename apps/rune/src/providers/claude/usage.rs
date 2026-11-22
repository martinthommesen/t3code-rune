use std::collections::BTreeMap;

use serde::Serialize;

use super::{ModelUsage, TurnResult, Usage};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub max_tokens: Option<u64>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_ttl_ms: Option<u64>,
}

impl ContextUsage {
    pub fn from_assistant(usage: Usage, max_tokens: Option<u64>) -> Option<Self> {
        let input_tokens = usage.input_with_cache().filter(|&tokens| tokens > 0)?;
        let output_tokens = usage.output_tokens?;
        let prompt_cache_ttl_ms = usage.cache_creation.and_then(|cache| {
            if cache.ephemeral_1h_input_tokens.unwrap_or(0) != 0 {
                Some(60 * 60_000)
            } else if cache.ephemeral_5m_input_tokens.unwrap_or(0) != 0 {
                Some(5 * 60_000)
            } else {
                None
            }
        });
        Some(Self {
            used_tokens: input_tokens.saturating_add(output_tokens),
            max_tokens,
            input_tokens,
            cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
            output_tokens,
            reasoning_output_tokens: 0,
            prompt_cache_ttl_ms,
        })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage<'a> {
    usage_status: &'static str,
    usage_scope: &'static str,
    has_subagents: bool,
    models: [&'a str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    other_usage: Option<Counts>,
}

pub struct ProcessLedger {
    previous: Option<BTreeMap<String, Counts>>,
}

impl Default for ProcessLedger {
    fn default() -> Self {
        Self {
            previous: Some(BTreeMap::new()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Counts {
    input_tokens: u64,
    output_tokens: u64,
}

impl Counts {
    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
        }
    }

    fn subtract(self, earlier: Self) -> Option<Self> {
        Some(Self {
            input_tokens: self.input_tokens.checked_sub(earlier.input_tokens)?,
            output_tokens: self.output_tokens.checked_sub(earlier.output_tokens)?,
        })
    }

    fn from_model(entry: &ModelUsage) -> Option<Self> {
        Some(Self {
            input_tokens: entry
                .input_tokens?
                .saturating_add(entry.cache_read_input_tokens.unwrap_or(0))
                .saturating_add(entry.cache_creation_input_tokens.unwrap_or(0)),
            output_tokens: entry.output_tokens?,
        })
    }
}

impl ProcessLedger {
    pub fn settle<'a>(&mut self, result: &TurnResult, model: &'a str) -> TurnUsage<'a> {
        let observed: Option<BTreeMap<String, Counts>> = result
            .model_usage
            .as_ref()
            .filter(|models| !models.is_empty())
            .and_then(|models| {
                models
                    .iter()
                    .map(|(model, usage)| Some((model.clone(), Counts::from_model(usage)?)))
                    .collect()
            });
        let growth = self
            .previous
            .as_ref()
            .zip(observed.as_ref())
            .and_then(|(previous, next)| {
                if previous.keys().any(|model| !next.contains_key(model)) {
                    return None;
                }
                next.iter()
                    .try_fold(Counts::default(), |total, (model, counts)| {
                        let before = previous.get(model).copied().unwrap_or_default();
                        Some(total.add(counts.subtract(before)?))
                    })
            });
        self.previous = if growth.is_none() && self.previous.is_some() {
            None
        } else {
            observed
        };

        let usage = result.usage.unwrap_or_default();
        let counts = [
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
            usage.output_tokens,
        ];
        let known = counts.iter().any(Option::is_some);
        let positive = counts.into_iter().flatten().any(|count| count != 0);
        let mut normalized = TurnUsage {
            usage_status: "unavailable",
            usage_scope: "main_agent",
            has_subagents: false,
            models: [model],
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            other_usage: None,
        };
        if !known || (result.subtype != "success" && !positive) {
            return normalized;
        }
        normalized.input_tokens = usage.input_with_cache();
        normalized.output_tokens = usage.output_tokens;
        normalized.usage_status = if result.succeeded()
            && normalized.input_tokens.is_some()
            && normalized.output_tokens.is_some()
        {
            "complete"
        } else {
            "partial"
        };
        normalized.cached_input_tokens = usage.cache_read_input_tokens;
        normalized.cache_creation_tokens = usage.cache_creation_input_tokens;
        normalized.reasoning_tokens = usage
            .output_tokens_details
            .and_then(|detail| detail.thinking_tokens)
            .zip(usage.output_tokens)
            .map(|(reasoning, output)| reasoning.min(output));
        normalized.other_usage = growth.and_then(|growth| {
            growth.subtract(Counts {
                input_tokens: normalized.input_tokens?,
                output_tokens: normalized.output_tokens?,
            })
        });
        normalized
    }
}

impl Usage {
    fn input_with_cache(self) -> Option<u64> {
        Some(
            self.input_tokens?
                .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_creation_input_tokens.unwrap_or(0)),
        )
    }
}
