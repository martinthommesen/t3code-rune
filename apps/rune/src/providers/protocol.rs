use serde_json::{Map, Value, json};
use tokio::io::AsyncBufRead;

use super::ndjson::{self, ReadError};
use super::{CodexError, RequestId, RpcError};

pub(super) use super::ndjson::MAX_FRAME_BYTES;

pub(super) enum Frame {
    Request {
        id: RequestId,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
    Response {
        id: RequestId,
        result: Result<Value, RpcError>,
    },
}

impl Frame {
    fn parse(bytes: &[u8]) -> Result<Self, CodexError> {
        let mut fields: Map<String, Value> =
            serde_json::from_slice(bytes).map_err(|_| CodexError::InvalidFrame)?;
        let id = fields.remove("id").map(decode_id).transpose()?;
        if let Some(method) = fields.remove("method") {
            let Value::String(method) = method else {
                return Err(CodexError::InvalidFrame);
            };
            let params = fields.remove("params");
            return Ok(match id {
                Some(id) => Self::Request { id, method, params },
                None => Self::Notification { method, params },
            });
        }
        let id = id.ok_or(CodexError::InvalidFrame)?;
        let result = match fields.remove("error") {
            Some(error) => {
                Err(serde_json::from_value(error).map_err(|_| CodexError::InvalidFrame)?)
            }
            None => Ok(fields.remove("result").unwrap_or(Value::Null)),
        };
        Ok(Self::Response { id, result })
    }
}

pub(super) fn decode_id(value: Value) -> Result<RequestId, CodexError> {
    match value {
        Value::Number(number) => Ok(RequestId::Number(number)),
        Value::String(text) => Ok(RequestId::Text(text)),
        _ => Err(CodexError::InvalidFrame),
    }
}

pub(super) fn client_request_id(id: RequestId) -> Option<u64> {
    match id {
        RequestId::Number(number) => number.as_u64().or_else(|| {
            let numeric = number.as_f64()?;
            (numeric.fract() == 0.0 && (0.0..((1_u64 << 53) as f64)).contains(&numeric))
                .then_some(numeric as u64)
        }),
        RequestId::Text(text) => {
            let id = text.parse::<u64>().ok()?;
            (text == id.to_string()).then_some(id)
        }
    }
}

pub(super) fn method_frame(id: Option<u64>, method: &str, params: Option<Value>) -> Value {
    let mut frame = Map::new();
    if let Some(id) = id {
        frame.insert("id".into(), id.into());
    }
    frame.insert("method".into(), method.into());
    if let Some(params) = params {
        frame.insert("params".into(), params);
    }
    Value::Object(frame)
}

pub(super) fn response_frame(id: RequestId, result: Result<Value, RpcError>) -> Value {
    match result {
        Ok(result) => json!({"id": id, "result": result}),
        Err(error) => json!({"id": id, "error": error}),
    }
}

pub(super) async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    buffer: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<Option<Frame>, CodexError> {
    let found =
        ndjson::read_line(reader, buffer, max_bytes)
            .await
            .map_err(|error| match error {
                ReadError::Io(error) => CodexError::Io(error.into()),
                ReadError::TooLarge => CodexError::FrameTooLarge,
            })?;
    if !found {
        return Ok(None);
    }
    let frame = Frame::parse(buffer);
    buffer.clear();
    frame.map(Some)
}
