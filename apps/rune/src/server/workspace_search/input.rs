use serde::Serialize;
use serde_json::{Value, json};

use super::super::thread_commands::trim_js;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    File,
    Directory,
}

#[derive(Clone)]
pub(in crate::server) struct Input {
    pub(super) cwd: String,
    pub(super) query: String,
    pub(super) limit: usize,
    pub(super) kind: Option<Kind>,
    pub(super) images: bool,
}

impl Input {
    pub(in crate::server) fn decode(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("Expected object")?;
        let required = |field: &str| {
            object
                .get(field)
                .ok_or_else(|| invalid(field, "Missing key"))
        };
        let string = |field: &str| {
            required(field)?
                .as_str()
                .map(trim_js)
                .ok_or_else(|| invalid(field, "Expected string"))
        };
        let cwd = string("cwd")?;
        if cwd.is_empty() {
            return Err(invalid("cwd", "Expected a non-blank string"));
        }
        let query = string("query")?;
        if query.encode_utf16().count() > 256 {
            return Err(invalid(
                "query",
                "Expected a value with a length of at most 256",
            ));
        }
        let limit = required("limit")?
            .as_f64()
            .ok_or_else(|| invalid("limit", "Expected number"))?;
        if limit.fract() != 0.0 {
            return Err(invalid("limit", "Expected an integer"));
        }
        if limit < 1.0 {
            return Err(invalid(
                "limit",
                "Expected a value greater than or equal to 1",
            ));
        }
        if limit > 200.0 {
            return Err(invalid(
                "limit",
                "Expected a value less than or equal to 200",
            ));
        }
        let kind = match object.get("kind") {
            None | Some(Value::Null) => None,
            Some(Value::String(kind)) if kind == "file" => Some(Kind::File),
            Some(Value::String(kind)) if kind == "directory" => Some(Kind::Directory),
            _ => return Err(invalid("kind", r#"Expected "file" | "directory""#)),
        };
        let images = match object.get("imageOnly") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(images)) => *images,
            _ => return Err(invalid("imageOnly", "Expected boolean | null")),
        };
        Ok(Self {
            cwd: cwd.into(),
            query: query.into(),
            limit: limit as usize,
            kind,
            images,
        })
    }

    pub(super) fn failure(
        &self,
        root: &str,
        problem: &str,
        cause_name: &str,
        message: String,
    ) -> Value {
        json!({
            "_tag": "ProjectSearchEntriesError",
            "cwd": self.cwd,
            "queryLength": self.query.encode_utf16().count(),
            "limit": self.limit,
            "failure": problem,
            "normalizedCwd": root,
            "message": format!("Failed to search workspace entries in '{}'.", self.cwd),
            "cause": {"name": cause_name, "message": message},
        })
    }
}

fn invalid(field: &str, message: &str) -> String {
    format!("{message}\n  at [\"{field}\"]")
}
