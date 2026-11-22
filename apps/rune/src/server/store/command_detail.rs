use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;

use super::{Store, open_readonly};

const DETAIL_BYTES: usize = 256 * 1024;
const TRUNCATED: &str = "\n… output truncated for transport";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct CommandDetail {
    pub(in crate::server) thread_id: String,
    item_id: String,
    #[serde(rename = "revision")]
    _revision: Option<String>,
}

impl CommandDetail {
    pub(in crate::server) fn parse(value: Value) -> Option<Self> {
        let mut input: Self = serde_json::from_value(value).ok()?;
        let trim = |value: &str| {
            value
                .trim_matches(|character: char| {
                    character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
                })
                .to_owned()
        };
        input.thread_id = trim(&input.thread_id);
        input.item_id = trim(&input.item_id);
        (!input.thread_id.is_empty() && !input.item_id.is_empty()).then_some(input)
    }
}

impl Store {
    pub(in crate::server) fn command_detail(
        &self,
        input: &CommandDetail,
    ) -> rusqlite::Result<Option<Value>> {
        let database = open_readonly(&self.path)?;
        let record = database.query_row(
            "SELECT type, payload_json FROM orchestration_v2_projection_turn_items WHERE thread_id = ?1 AND turn_item_id = ?2",
            params![input.thread_id, input.item_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).optional()?;
        let Some((kind, payload)) = record else {
            return Ok(None);
        };
        if kind != "command_execution" {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let mut item: Value = serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let fields = item.as_object_mut().ok_or(rusqlite::Error::InvalidQuery)?;
        for field in ["input", "output"] {
            if let Some(value) = fields.get_mut(field) {
                let Value::String(text) = value else {
                    return Err(rusqlite::Error::InvalidQuery);
                };
                if text.len() > DETAIL_BYTES {
                    let mut end = DETAIL_BYTES - TRUNCATED.len();
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                    if text.ends_with('\u{fffd}') {
                        text.pop();
                    }
                    text.push_str(TRUNCATED);
                }
            }
        }
        Ok(Some(item))
    }
}
