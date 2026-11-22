use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::Error;

#[derive(Debug)]
pub enum Message {
    Initialized(Initialization),
    Assistant(AssistantMessage),
    Result(Box<TurnResult>),
    RejectedControl { subtype: String },
    Other(Value),
}

#[derive(Debug, Deserialize)]
pub struct Initialization {
    pub session_id: Uuid,
    pub model: String,
    #[serde(rename = "claude_code_version")]
    pub cli_version: Option<String>,
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    pub uuid: Uuid,
    pub session_id: Uuid,
    pub parent_tool_use_id: Option<String>,
    pub error: Option<String>,
    pub message: AssistantSnapshot,
}

#[derive(Debug, Deserialize)]
pub struct AssistantSnapshot {
    pub id: String,
    pub model: String,
    pub content: Vec<AssistantBlock>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct TurnResult {
    pub uuid: Uuid,
    pub session_id: Uuid,
    pub subtype: String,
    pub is_error: bool,
    pub result: Option<String>,
    pub stop_reason: Option<String>,
    pub api_error_status: Option<u16>,
    pub usage: Option<Usage>,
    #[serde(rename = "modelUsage")]
    pub model_usage: Option<BTreeMap<String, ModelUsage>>,
}

impl TurnResult {
    pub fn succeeded(&self) -> bool {
        self.subtype == "success" && !self.is_error
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_creation: Option<CacheCreation>,
    pub output_tokens_details: Option<OutputDetails>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct CacheCreation {
    pub ephemeral_1h_input_tokens: Option<u64>,
    pub ephemeral_5m_input_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct OutputDetails {
    pub thinking_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

pub(super) enum Incoming {
    Request {
        id: String,
        subtype: String,
        tool_use_id: Option<String>,
    },
    Response {
        id: String,
        success: bool,
    },
    Message(Message),
}

impl Incoming {
    pub(super) fn decode(line: &[u8]) -> Result<Self, Error> {
        let value: Value = serde_json::from_slice(line).map_err(|_| Error::InvalidFrame)?;
        let tag = value["type"].as_str().ok_or(Error::InvalidFrame)?;
        let message = match tag {
            "control_request" => {
                let request = &value["request"];
                return Ok(Self::Request {
                    id: required_string(&value["request_id"])?.to_owned(),
                    subtype: required_string(&request["subtype"])?.to_owned(),
                    tool_use_id: request["tool_use_id"].as_str().map(str::to_owned),
                });
            }
            "control_response" => {
                let response = &value["response"];
                return Ok(Self::Response {
                    id: required_string(&response["request_id"])?.to_owned(),
                    success: match required_string(&response["subtype"])? {
                        "success" => true,
                        "error" => false,
                        _ => return Err(Error::InvalidFrame),
                    },
                });
            }
            "system" if value["subtype"] == "init" => Message::Initialized(decode(value)?),
            "assistant" => Message::Assistant(decode(value)?),
            "result" => Message::Result(decode(value)?),
            _ => Message::Other(value),
        };
        Ok(Self::Message(message))
    }
}

fn required_string(value: &Value) -> Result<&str, Error> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or(Error::InvalidFrame)
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| Error::InvalidFrame)
}
