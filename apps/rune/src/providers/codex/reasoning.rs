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

    pub fn for_model(model: &str, value: &str) -> Option<Self> {
        const SOL_MODEL: &str = "gpt-5.6-sol";
        if !matches!(model, Self::MODEL_ID | SOL_MODEL) {
            return None;
        }
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::ExtraHigh),
            "max" if model == SOL_MODEL => Some(Self::Max),
            "ultra" if model == SOL_MODEL => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn from_options(model: &str, options: &Value) -> Option<Self> {
        ReasoningOptions::parse(model, options).map(|options| options.effort)
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
pub struct ReasoningOptions {
    pub effort: ReasoningEffort,
    pub standard_service_tier: bool,
}

impl ReasoningOptions {
    pub fn parse(model: &str, options: &Value) -> Option<Self> {
        let standard = |option: &Value| {
            option.as_object().is_some_and(|fields| fields.len() == 2)
                && option["id"] == "serviceTier"
                && option["value"] == "default"
        };
        let (item, standard_service_tier) = match options.as_array()?.as_slice() {
            [item] => (item, false),
            [first, second] if standard(first) => (second, true),
            [first, second] if standard(second) => (first, true),
            _ => return None,
        };
        if item["id"] != ReasoningEffort::OPTION_ID || item.as_object()?.len() != 2 {
            return None;
        }
        Some(Self {
            effort: ReasoningEffort::for_model(model, item["value"].as_str()?)?,
            standard_service_tier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sol_ultra_preserves_the_standard_service_tier_only() {
        let options = json!([
            {"id":"reasoningEffort","value":"ultra"},
            {"id":"serviceTier","value":"default"},
        ]);
        assert_eq!(
            ReasoningOptions::parse("gpt-5.6-sol", &options),
            Some(ReasoningOptions {
                effort: ReasoningEffort::Ultra,
                standard_service_tier: true,
            })
        );
        assert!(ReasoningOptions::parse(ReasoningEffort::MODEL_ID, &options).is_none());
        let mut faster = options;
        faster[1]["value"] = json!("priority");
        assert!(ReasoningOptions::parse("gpt-5.6-sol", &faster).is_none());
    }
}
