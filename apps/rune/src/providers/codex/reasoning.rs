use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
        if !matches!(model, Self::MODEL_ID | Self::SOL_MODEL) {
            return None;
        }
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::ExtraHigh),
            "max" if model == Self::SOL_MODEL => Some(Self::Max),
            "ultra" if model == Self::SOL_MODEL => Some(Self::Ultra),
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

    fn parse(model: &str, option: &Value) -> Option<Self> {
        if option["id"] != Self::OPTION_ID || option.as_object()?.len() != 2 {
            return None;
        }
        match option["value"].as_str()? {
            "default" => Some(Self::Standard),
            "priority" if model == ReasoningEffort::SOL_MODEL => Some(Self::Priority),
            _ => None,
        }
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
