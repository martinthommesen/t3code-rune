use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    ExtraHigh,
    Max,
    Ultra,
}

impl ReasoningEffort {
    pub const MODEL_ID: &str = "gpt-5.4";
    pub const OPTION_ID: &str = "reasoningEffort";
    pub const SOL_MODEL: &str = "gpt-5.6-sol";

    pub fn for_model(model: &str, value: &str) -> Option<Self> {
        let maximum = Self::maximum_for_model(model)?;
        let effort = match value {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::ExtraHigh,
            "max" => Self::Max,
            "ultra" => Self::Ultra,
            _ => return None,
        };
        (effort <= maximum).then_some(effort)
    }

    fn maximum_for_model(model: &str) -> Option<Self> {
        match model {
            Self::MODEL_ID | "gpt-5.5" => Some(Self::ExtraHigh),
            "gpt-5.6-luna" | "gpt-6-luna" => Some(Self::Max),
            Self::SOL_MODEL
            | "gpt-5.6-terra"
            | "gpt-6-sol"
            | "gpt-6-astra"
            | "gpt-6.1-sol"
            | "gpt-6.1-terra"
            | "gpt-6.2-sol"
            | "gpt-6.2-astra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExtraHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::ExtraHigh => "Extra High",
            Self::Max => "Max",
            Self::Ultra => "Ultra",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTier {
    Standard,
    Priority,
}

impl ServiceTier {
    pub const OPTION_ID: &str = "serviceTier";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "default",
            Self::Priority => "priority",
        }
    }

    pub fn for_model(model: &str, value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Standard),
            "priority"
                if model != ReasoningEffort::MODEL_ID
                    && ReasoningEffort::maximum_for_model(model).is_some() =>
            {
                Some(Self::Priority)
            }
            _ => None,
        }
    }

    fn parse(model: &str, option: &Value) -> Option<Self> {
        if option["id"] != Self::OPTION_ID || option.as_object()?.len() != 2 {
            return None;
        }
        Self::for_model(model, option["value"].as_str()?)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReasoningOptions {
    pub effort: ReasoningEffort,
    pub service_tier: Option<ServiceTier>,
}

impl ReasoningOptions {
    pub fn parse(model: &str, options: &Value) -> Option<Self> {
        let (item, service_tier) = match options.as_array()?.as_slice() {
            [item] => (item, None),
            [first, second] if first["id"] == ServiceTier::OPTION_ID => {
                (second, Some(ServiceTier::parse(model, first)?))
            }
            [first, second] => (first, Some(ServiceTier::parse(model, second)?)),
            _ => return None,
        };
        if item["id"] != ReasoningEffort::OPTION_ID || item.as_object()?.len() != 2 {
            return None;
        }
        Some(Self {
            effort: ReasoningEffort::for_model(model, item["value"].as_str()?)?,
            service_tier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_catalogue_limits_for_the_observed_codex_models() {
        for (model, maximum) in [
            ("gpt-5.5", ReasoningEffort::ExtraHigh),
            ("gpt-5.6-luna", ReasoningEffort::Max),
            ("gpt-5.6-sol", ReasoningEffort::Ultra),
            ("gpt-5.6-terra", ReasoningEffort::Ultra),
            ("gpt-6-luna", ReasoningEffort::Max),
            ("gpt-6-sol", ReasoningEffort::Ultra),
            ("gpt-6-astra", ReasoningEffort::Ultra),
            ("gpt-6.1-sol", ReasoningEffort::Ultra),
            ("gpt-6.1-terra", ReasoningEffort::Ultra),
            ("gpt-6.2-sol", ReasoningEffort::Ultra),
            ("gpt-6.2-astra", ReasoningEffort::Ultra),
        ] {
            let options = |effort| {
                json!([
                    {"id":"reasoningEffort","value":effort},
                    {"id":"serviceTier","value":"priority"},
                ])
            };
            assert_eq!(
                ReasoningOptions::parse(model, &options(maximum.as_str())),
                Some(ReasoningOptions {
                    effort: maximum,
                    service_tier: Some(ServiceTier::Priority)
                }),
            );
            assert_eq!(
                ReasoningOptions::parse(model, &options("ultra")).is_some(),
                maximum == ReasoningEffort::Ultra,
            );
        }
        assert!(ReasoningEffort::for_model("unknown", "medium").is_none());
        assert!(ServiceTier::for_model("unknown", "priority").is_none());
    }

    #[test]
    fn sol_accepts_the_reported_service_tiers_in_either_option_order() {
        for tier in [ServiceTier::Standard, ServiceTier::Priority] {
            let effort = json!({"id":"reasoningEffort","value":"ultra"});
            let service_tier = json!({"id":"serviceTier","value":tier.as_str()});
            for options in [json!([effort, service_tier]), json!([service_tier, effort])] {
                assert_eq!(
                    ReasoningOptions::parse(ReasoningEffort::SOL_MODEL, &options),
                    Some(ReasoningOptions {
                        effort: ReasoningEffort::Ultra,
                        service_tier: Some(tier),
                    })
                );
            }
        }
        let options = json!([
            {"id":"reasoningEffort","value":"high"},
            {"id":"serviceTier","value":"priority"},
        ]);
        assert!(ReasoningOptions::parse(ReasoningEffort::MODEL_ID, &options).is_none());
    }
}
