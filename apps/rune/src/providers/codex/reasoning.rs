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
}

impl ReasoningEffort {
    pub const MODEL_ID: &str = "gpt-5.4";
    pub const OPTION_ID: &str = "reasoningEffort";

    pub fn for_model(model: &str, value: &str) -> Option<Self> {
        if model != Self::MODEL_ID {
            return None;
        }
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::ExtraHigh),
            _ => None,
        }
    }

    pub fn from_options(model: &str, options: &Value) -> Option<Self> {
        let [item] = options.as_array()?.as_slice() else {
            return None;
        };
        if item["id"] != Self::OPTION_ID || item.as_object()?.len() != 2 {
            return None;
        }
        Self::for_model(model, item["value"].as_str()?)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExtraHigh => "xhigh",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::ExtraHigh => "Extra High",
        }
    }
}
