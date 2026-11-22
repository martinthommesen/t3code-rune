use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

mod watch;

use crate::server::config_updates::Subscribers;

use super::{MAX_RULES, decode_fields, expression, shortcut, stock};

#[derive(Default)]
pub(in crate::server) struct Edits {
    previous: Mutex<Option<Value>>,
    updates: Subscribers,
}

pub(in crate::server) enum Edit {
    Upsert {
        rule: RawRule,
        replace: Option<RawRule>,
    },
    Remove(RawRule),
}

pub(in crate::server) struct RawRule {
    key: String,
    command: String,
    when: Option<String>,
}

impl RawRule {
    fn decode(value: &Value) -> Result<Self, String> {
        let (key, command, when) = decode_fields(value)?;
        Ok(Self {
            key: key.to_owned(),
            command: command.to_owned(),
            when: when.map(str::to_owned),
        })
    }

    fn value(&self) -> Value {
        let mut rule = json!({"key": self.key, "command": self.command});
        if let Some(when) = &self.when {
            rule["when"] = json!(when);
        }
        rule
    }

    fn matches(&self, other: &Self) -> bool {
        self.command == other.command
            && self.key_identity() == other.key_identity()
            && self.when_identity() == other.when_identity()
    }

    fn key_identity(&self) -> Value {
        shortcut(&self.key).unwrap_or_else(|| json!(&self.key))
    }

    fn when_identity(&self) -> Option<Value> {
        self.when
            .as_deref()
            .map(|when| expression(when).unwrap_or_else(|| json!(when)))
    }

    fn resolves(&self) -> bool {
        shortcut(&self.key).is_some()
            && self
                .when
                .as_deref()
                .is_none_or(|when| expression(when).is_some())
    }
}

impl Edit {
    pub(in crate::server) fn decode(value: &Value, remove: bool) -> Result<Self, String> {
        fn rule(value: &Value, context: Option<&str>) -> Result<RawRule, String> {
            let filtered = if value.get("when") == Some(&Value::Null) {
                let mut cleaned = value.clone();
                cleaned
                    .as_object_mut()
                    .expect("optional when field requires an object")
                    .remove("when");
                Some(cleaned)
            } else {
                None
            };
            RawRule::decode(filtered.as_ref().unwrap_or(value)).map_err(|error| {
                let error = error.strip_prefix("SchemaError: ").unwrap_or(&error);
                let error = error.replace("Expected string | undefined", "Expected string | null");
                if let Some(context) = context {
                    return error.replace("\n  at [", &format!("\n  at [\"{context}\"]["));
                }
                error
            })
        }
        let primary = rule(value, None)?;
        if remove {
            return Ok(Self::Remove(primary));
        }
        let replace = match value.get("replace") {
            None | Some(Value::Null) => None,
            Some(value) if value.is_object() => Some(rule(value, Some("replace"))?),
            _ => return Err("Expected object | null\n  at [\"replace\"]".into()),
        };
        Ok(Self::Upsert {
            rule: primary,
            replace,
        })
    }
}

struct Entry {
    raw: Value,
    rule: Option<RawRule>,
}

impl Entry {
    fn new(raw: Value) -> Self {
        let rule = RawRule::decode(&raw).ok().filter(RawRule::resolves);
        let raw = rule.as_ref().map_or(raw, RawRule::value);
        Self { raw, rule }
    }
}

impl Edits {
    pub(in crate::server) fn watch(self: &Arc<Self>, path: PathBuf) -> JoinHandle<()> {
        watch::start(Arc::clone(self), path)
    }

    pub(in crate::server) fn initial(&self, path: &Path) -> Result<(Value, Value), Value> {
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let (bindings, issues) = super::resolved(path, true)?;
        previous.get_or_insert_with(|| configuration(&bindings, &issues));
        Ok((bindings, issues))
    }

    fn observe(&self, path: &Path) {
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok((bindings, issues)) = super::resolved(path, false) else {
            return;
        };
        let state = configuration(&bindings, &issues);
        if previous.as_ref() != Some(&state) {
            self.updates.publish(&state);
            *previous = Some(state);
        }
    }

    pub(in crate::server) fn subscribe(&self) -> UnboundedReceiver<Arc<Value>> {
        self.updates.subscribe()
    }

    pub(in crate::server) fn update(&self, path: &Path, edit: Edit) -> Result<Value, Value> {
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entries = writable_entries(path)?;
        let entries = match edit {
            Edit::Upsert { rule, replace } => upsert(path, entries, rule, replace)?,
            Edit::Remove(target) => {
                entries.retain(|entry| {
                    entry
                        .rule
                        .as_ref()
                        .is_none_or(|rule| !rule.matches(&target))
                });
                entries.into_iter().map(|entry| entry.raw).collect()
            }
        };
        let mut data = serde_json::to_vec_pretty(&entries).expect("JSON keybindings serialize");
        data.push(b'\n');
        crate::server::settings::write_document(path, &data)
            .map_err(|_| failure(path, "failed to write keybindings config", None))?;
        let (keybindings, issues) = super::resolved(path, false)?;
        let result = configuration(&keybindings, &issues);
        *previous = Some(result.clone());
        self.updates.publish(&result);
        Ok(result)
    }
}

fn configuration(bindings: &Value, issues: &Value) -> Value {
    json!({"keybindings": bindings, "issues": issues})
}

fn writable_entries(path: &Path) -> Result<Vec<Entry>, Value> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(stock()
                .defaults
                .iter()
                .map(|entry| Entry::new(entry.raw.clone()))
                .collect());
        }
        Err(_) => return Err(failure(path, "failed to read keybindings config", None)),
    };
    let entries = match crate::server::parse_settings_json(&raw) {
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(failure(path, "expected JSON array", Some("Expected array"))),
        None => {
            return Err(failure(
                path,
                "expected JSON array",
                Some("Expected a valid JSON string"),
            ));
        }
    };
    Ok(entries.into_iter().map(Entry::new).collect())
}

fn upsert(
    path: &Path,
    mut entries: Vec<Entry>,
    rule: RawRule,
    replace: Option<RawRule>,
) -> Result<Vec<Value>, Value> {
    let defaults = stock()
        .defaults
        .iter()
        .filter_map(|entry| RawRule::decode(&entry.raw).ok());
    let preserve_defaults = replace.as_ref().is_none_or(|target| {
        target.command == rule.command && defaults.clone().any(|default| default.matches(target))
    }) && !entries.iter().any(|entry| {
        entry
            .rule
            .as_ref()
            .is_some_and(|existing| existing.command == rule.command)
    }) && rule.resolves();
    let mut updated: Vec<_> = defaults
        .filter(|candidate| {
            preserve_defaults
                && candidate.command == rule.command
                && !candidate.matches(&rule)
                && replace
                    .as_ref()
                    .is_none_or(|target| !candidate.matches(target))
        })
        .map(|entry| entry.value())
        .collect();
    entries.retain(|entry| {
        entry.rule.as_ref().is_none_or(|candidate| {
            !candidate.matches(&rule)
                && replace
                    .as_ref()
                    .is_none_or(|target| !candidate.matches(target))
        })
    });
    let max_retained = MAX_RULES - 1;
    if !updated.is_empty() && entries.len() + updated.len() > max_retained {
        return Err(failure(
            path,
            "Not enough room to save this shortcut and its inherited shortcuts.",
            None,
        ));
    }
    let start = entries.len().saturating_sub(max_retained);
    updated.extend(entries.into_iter().skip(start).map(|entry| entry.raw));
    updated.push(rule.value());
    Ok(updated)
}

fn failure(path: &Path, detail: &str, cause: Option<&str>) -> Value {
    let mut result =
        json!({"_tag":"KeybindingsConfigParseError", "configPath":path, "detail":detail});
    if let Some(message) = cause {
        result["cause"] = json!({"name": "SchemaError", "message":message});
    }
    result
}
