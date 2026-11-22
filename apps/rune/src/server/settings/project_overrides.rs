use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::Connection;
use serde_json::{Value, json};

use super::{Fields, client, patch, project_scripts, secret_slug, slug, trim};

const KEY: &str = "projectSettingsOverrides";
pub(super) const LEGACY: &[(&str, &str)] = &[
    (
        "projectAgentBrowserAccessOverrides",
        "enableAgentBrowserAccess",
    ),
    ("projectAutoPullOverrides", "defaultAutoPull"),
    ("projectScriptOverrides", "defaultProjectScripts"),
];
const KEYS: &[&str] = &[
    "worktreeCleanup",
    "defaultModelSelection",
    "defaultRuntimeMode",
    "defaultThreadEnvMode",
    "newWorktreesStartFromOrigin",
    "worktreeSubmodules",
    "defaultAutoPull",
    "defaultProjectScripts",
    "enableAgentBrowserAccess",
    "enableAgentDeviceAccess",
    "textGenerationModelSelection",
    "threadTitleInstructions",
    "sourceControlWriterModelSelection",
    "reviewGuideModelSelection",
    "reviewGuideInstructions",
    "sourceControlWritingStyle",
    "removeAgentCreditsOnMerge",
    "branchNamingMode",
    "branchNamePrefix",
    "branchNameInstructions",
    "pullRequestMergeMethod",
    "sidebarAutoSettleOnMerge",
    "sidebarAutoSettleAfterDays",
    "sidebarAutoSettleKeepPullRequests",
    "continueThreadsAfterServerUpdate",
    "responseStreamingMode",
    "speechProjectCustomWords",
    "providerInstances",
];

pub(super) struct ProjectPatch {
    entries: Fields,
    pub(super) references_secret: bool,
    pub(super) unsupported: Option<String>,
}

impl ProjectPatch {
    pub(super) fn decode(value: &Value) -> Result<Self, String> {
        let root = format!("[\"patch\"][\"{KEY}\"]");
        let entries = value
            .as_object()
            .ok_or_else(|| expected("Expected object", &root))?;
        let mut result = Self {
            entries: Fields::new(),
            references_secret: false,
            unsupported: None,
        };
        for (raw_id, entry) in entries {
            let entry_path = at(&root, raw_id);
            let id = trim(raw_id);
            if id.is_empty() {
                return Err(expected("Expected a non-blank string", &entry_path));
            }
            if entry.is_null() {
                result.entries.insert(id.to_owned(), Value::Null);
                continue;
            }
            let fields = entry
                .as_object()
                .ok_or_else(|| expected("Expected object | null", &entry_path))?;
            let mut next = Fields::new();
            for &key in KEYS {
                if let Some(value) = fields.get(key) {
                    let path = at(&entry_path, key);
                    if key == "worktreeSubmodules"
                        && !matches!(value.as_str(), Some("recursive" | "top-level" | "none"))
                    {
                        // The stock optional codec ignores unknown values.
                        continue;
                    }
                    if supported(key) {
                        next.insert(key.to_owned(), decode_value(key, value, &path)?);
                    } else if result.unsupported.is_none() {
                        result.unsupported = Some(format!("{KEY}.{key}"));
                    }
                }
            }
            result.references_secret |= fields
                .iter()
                .any(|(key, value)| !KEYS.contains(&key.as_str()) && references_secret(value));
            result.entries.insert(id.to_owned(), next.into());
        }
        Ok(result)
    }

    pub(super) fn apply(self, stored: &mut Fields) -> Result<(), String> {
        let mut projects = take_entries(stored);
        for (id, patch) in self.entries {
            let Some(patch) = patch.as_object() else {
                projects.shift_remove(&id);
                continue;
            };
            let current = projects.get(&id).and_then(Value::as_object);
            if current.is_some_and(|entry| {
                entry
                    .keys()
                    .any(|key| KEYS.contains(&key.as_str()) && !supported(key))
            }) {
                return Err("The project has an override outside the supported settings.".into());
            }
            let mut next = current
                .into_iter()
                .flatten()
                .filter(|(key, _)| !KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Fields>();
            next.extend(patch.clone());
            if !patch.contains_key("providerInstances")
                && let Some(provider_instances) =
                    current.and_then(|entry| entry.get("providerInstances"))
            {
                next.insert("providerInstances".to_owned(), provider_instances.clone());
            }
            projects.insert(id, next.into());
        }
        projects.retain(|_, entry| entry.as_object().is_some_and(|fields| !fields.is_empty()));
        put_entries(stored, projects);
        Ok(())
    }
}

pub(super) fn copy_unknown(target: &mut Fields, raw: &Fields) {
    target.extend(
        raw.iter()
            .filter(|(key, _)| !KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone())),
    );
}

pub(super) fn valid_legacy_fields(stored: &Fields) -> bool {
    stored
        .get("projectSettingsFolded")
        .is_none_or(Value::is_boolean)
        && LEGACY.iter().all(|(legacy, _)| {
            stored.get(*legacy).is_none_or(|entries| {
                entries.as_object().is_some_and(|entries| {
                    entries.iter().all(|(id, value)| {
                        !trim(id).is_empty()
                            && if *legacy == "projectScriptOverrides" {
                                value.is_null() || project_scripts::decode(value, "").is_ok()
                            } else {
                                value.is_boolean()
                            }
                    })
                })
            })
        })
}

pub(super) fn fold_legacy(stored: &mut Fields, database: &Path) -> rusqlite::Result<()> {
    if !folded(stored) {
        let database = crate::server::store::open_readonly(database)?;
        fold_legacy_on(stored, &database)?;
    }
    Ok(())
}

pub(super) fn reconcile_startup(stored: &mut Fields, database: &Path) -> rusqlite::Result<bool> {
    let mut database = crate::server::store::open_readonly(database)?;
    let projects = database.transaction()?;
    let migrated = !folded(stored) && fold_legacy_on(stored, &projects)?;
    let swept = sweep(stored, &projects)?;
    projects.commit()?;
    if migrated || swept {
        refresh_legacy(stored);
    }
    Ok(migrated || swept)
}

fn folded(stored: &Fields) -> bool {
    stored.get("projectSettingsFolded") == Some(&Value::Bool(true))
}

fn fold_legacy_on(stored: &mut Fields, database: &Connection) -> rusqlite::Result<bool> {
    let mut query = database.prepare(
        "SELECT project_id, default_model_selection_json, default_thread_env_mode, auto_pull, scripts_json
         FROM projection_projects WHERE deleted_at IS NULL",
    )?;
    let mut rows = query.query([])?;
    let mut row = rows.next()?;
    if row.is_none()
        && LEGACY.iter().all(|(key, _)| {
            stored
                .get(*key)
                .and_then(Value::as_object)
                .is_none_or(Fields::is_empty)
        })
    {
        return Ok(false);
    }
    let mut projects = take_entries(stored);
    let mut reset_scripts = HashSet::new();
    for &(legacy, field) in LEGACY {
        if let Some(entries) = stored.get(legacy).and_then(Value::as_object) {
            for (id, value) in entries {
                let id = trim(id);
                if legacy == "projectScriptOverrides" {
                    if value.is_null() {
                        reset_scripts.insert(id.to_owned());
                    } else if let Ok(scripts) = project_scripts::decode(value, "") {
                        set_if_missing(&mut projects, id, field, scripts);
                    }
                } else if value.is_boolean() {
                    set_if_missing(&mut projects, id, field, value.clone());
                }
            }
        }
    }
    while let Some(project) = row {
        let id: String = project.get(0)?;
        let model: Option<String> = project.get(1)?;
        if let Some(model) = model.and_then(|value| serde_json::from_str(&value).ok())
            && let Some(selection) = client::model_selection(&model)
        {
            set_if_missing(&mut projects, &id, "defaultModelSelection", selection);
        }
        let mode: Option<String> = project.get(2)?;
        if let Some(mode) = mode.filter(|mode| matches!(mode.as_str(), "local" | "worktree")) {
            set_if_missing(&mut projects, &id, "defaultThreadEnvMode", mode.into());
        }
        if project.get::<_, i64>(3)? == 1 {
            set_if_missing(&mut projects, &id, "defaultAutoPull", true.into());
        }
        if !reset_scripts.contains(&id)
            && let Ok(scripts) = serde_json::from_str::<Value>(&project.get::<_, String>(4)?)
            && scripts
                .as_array()
                .is_some_and(|scripts| !scripts.is_empty())
            && let Ok(scripts) = project_scripts::decode(&scripts, "")
        {
            set_if_missing(&mut projects, &id, "defaultProjectScripts", scripts);
        }
        row = rows.next()?;
    }
    projects.retain(|_, entry| entry.as_object().is_some_and(|fields| !fields.is_empty()));
    put_entries(stored, projects);
    stored.insert("projectSettingsFolded".to_owned(), true.into());
    Ok(true)
}

fn sweep(stored: &mut Fields, database: &Connection) -> rusqlite::Result<bool> {
    let mut entries = take_entries(stored);
    if entries.is_empty() {
        return Ok(false);
    }
    let mut query =
        database.prepare("SELECT project_id, deleted_at IS NOT NULL FROM projection_projects")?;
    let deleted_by_id = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    let mut changed = false;
    entries.retain(|id, entry| match deleted_by_id.get(id) {
        Some(false) => true,
        Some(true) if entry.get("worktreeCleanup").is_some() => {
            let current = entry.as_object().expect("validated project overrides");
            let mut next = Fields::new();
            copy_unknown(&mut next, current);
            let has_opaque_fields = !next.is_empty();
            next.insert(
                "worktreeCleanup".to_owned(),
                current["worktreeCleanup"].clone(),
            );
            next.insert("providerInstances".to_owned(), Value::Object(Fields::new()));
            let next = Value::Object(next);
            changed |= has_opaque_fields || *entry != next;
            *entry = next;
            true
        }
        _ => {
            changed = true;
            false
        }
    });
    put_entries(stored, entries);
    Ok(changed)
}

fn put_entries(stored: &mut Fields, entries: Fields) {
    if !entries.is_empty() {
        stored.insert(KEY.to_owned(), entries.into());
    }
}

fn take_entries(stored: &mut Fields) -> Fields {
    let Some(Value::Object(entries)) = stored.shift_remove(KEY) else {
        return Fields::new();
    };
    entries
        .into_iter()
        .map(|(id, mut entry)| {
            if entry["responseStreamingMode"] == "token" {
                entry["responseStreamingMode"] = "paragraph".into();
            }
            if let Some(cleanup) = entry
                .get("worktreeCleanup")
                .and_then(client::worktree_cleanup)
            {
                entry["worktreeCleanup"] = cleanup;
            }
            (trim(&id).to_owned(), entry)
        })
        .collect()
}

fn set_if_missing(projects: &mut Fields, id: &str, field: &str, value: Value) {
    projects
        .entry(id.to_owned())
        .or_insert_with(|| Fields::new().into())
        .as_object_mut()
        .expect("validated project settings object")
        .entry(field.to_owned())
        .or_insert(value);
}

pub(super) fn refresh_legacy(stored: &mut Fields) {
    for &(legacy, field) in LEGACY {
        let entries = stored
            .get(KEY)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(id, project)| Some((trim(id).to_owned(), project.get(field)?.clone())))
            .collect::<Fields>();
        if entries.is_empty() {
            stored.shift_remove(legacy);
        } else {
            stored.insert(legacy.to_owned(), entries.into());
        }
    }
}

fn supported(key: &str) -> bool {
    matches!(
        key,
        "defaultModelSelection"
            | "defaultRuntimeMode"
            | "defaultThreadEnvMode"
            | "newWorktreesStartFromOrigin"
            | "worktreeSubmodules"
            | "defaultAutoPull"
            | "defaultProjectScripts"
            | "enableAgentBrowserAccess"
            | "enableAgentDeviceAccess"
            | "textGenerationModelSelection"
            | "sourceControlWriterModelSelection"
            | "reviewGuideModelSelection"
            | "continueThreadsAfterServerUpdate"
            | "responseStreamingMode"
            | "branchNamingMode"
            | "branchNamePrefix"
            | "branchNameInstructions"
            | "providerInstances"
    )
}

fn decode_value(key: &str, value: &Value, path: &str) -> Result<Value, String> {
    match key {
        "providerInstances" => return decode_provider_instances(value, path),
        "defaultProjectScripts" => return project_scripts::decode(value, path),
        "defaultModelSelection"
        | "sourceControlWriterModelSelection"
        | "reviewGuideModelSelection"
        | "textGenerationModelSelection" => {
            return decode_model_selection(value, path, key != "textGenerationModelSelection");
        }
        "enableAgentBrowserAccess" | "enableAgentDeviceAccess" if !value.is_boolean() => {
            return Err(expected("Expected boolean", path));
        }
        "defaultThreadEnvMode" if value.is_null() => {
            return Err(expected("Expected \"local\" | \"worktree\"", path));
        }
        _ => (),
    }
    if let Some(error) = patch::invalid_value(key, value) {
        return Err(expected(error, path));
    }
    if matches!(key, "branchNamePrefix" | "branchNameInstructions") {
        return Ok(super::text(value)
            .expect("decoded branch naming text")
            .into());
    }
    Ok(value.clone())
}

fn decode_provider_instances(value: &Value, path: &str) -> Result<Value, String> {
    let entries = value
        .as_object()
        .ok_or_else(|| expected("Expected object", path))?;
    let mut instances = Fields::new();
    for (raw_id, instance) in entries {
        let entry_path = at(path, raw_id);
        let id = provider_id(&Value::String(raw_id.clone()), &entry_path)?.to_owned();
        let instance = instance
            .as_object()
            .ok_or_else(|| expected("Expected object", &entry_path))?;
        let normalized = match instance.get("enabled") {
            Some(value) => {
                json!({ "enabled": value.as_bool().ok_or_else(|| expected("Expected boolean", &at(&entry_path, "enabled")))? })
            }
            None => json!({}),
        };
        instances.insert(id, normalized);
    }
    Ok(instances.into())
}

fn decode_model_selection(value: &Value, path: &str, nullable: bool) -> Result<Value, String> {
    if nullable && value.is_null() {
        return Ok(Value::Null);
    }
    let fields = value.as_object().ok_or_else(|| {
        expected(
            if nullable {
                "Expected object | null"
            } else {
                "Expected object"
            },
            path,
        )
    })?;
    let model_path = at(path, "model");
    let model = fields
        .get("model")
        .ok_or_else(|| expected("Missing key", &model_path))?;
    let provider = fields
        .get("instanceId")
        .or_else(|| fields.get("provider"))
        .unwrap_or(&Value::Null);
    provider_id(provider, &at(path, "instanceId"))?;
    nonempty_string(model, &model_path)?;
    if let Some(options) = fields.get("options").filter(|value| !value.is_null()) {
        let options_path = at(path, "options");
        if let Some(entries) = options.as_array() {
            for (index, entry) in entries.iter().enumerate() {
                let entry_path = format!("{options_path}[{index}]");
                let entry = entry
                    .as_object()
                    .ok_or_else(|| expected("Expected object", &entry_path))?;
                let id_path = at(&entry_path, "id");
                let id = entry
                    .get("id")
                    .ok_or_else(|| expected("Missing key", &id_path))?;
                nonempty_string(id, &id_path)?;
                let value_path = at(&entry_path, "value");
                let selected = entry
                    .get("value")
                    .ok_or_else(|| expected("Missing key", &value_path))?;
                match selected {
                    Value::Bool(_) => (),
                    Value::String(_) => {
                        nonempty_string(selected, &value_path)?;
                    }
                    _ => return Err(expected("Expected string | boolean", &value_path)),
                }
            }
        } else if !options.is_object() {
            return Err(expected("Expected array | object", &options_path));
        }
    }
    Ok(client::model_selection(value).expect("validated model selection"))
}

fn provider_id<'a>(value: &'a Value, path: &str) -> Result<&'a str, String> {
    let value = nonempty_string(value, path)?;
    if value.encode_utf16().count() > 64 {
        return Err(expected(
            "Expected a value with a length of at most 64",
            path,
        ));
    }
    if !slug(value) {
        return Err(expected(
            "Expected a string matching the RegExp ^[a-zA-Z][a-zA-Z0-9_-]*$",
            path,
        ));
    }
    Ok(value)
}

fn nonempty_string<'a>(value: &'a Value, path: &str) -> Result<&'a str, String> {
    let value = value
        .as_str()
        .ok_or_else(|| expected("Expected string", path))?;
    let value = trim(value);
    if value.is_empty() {
        return Err(expected("Expected a non-blank string", path));
    }
    Ok(value)
}

fn references_secret(value: &Value) -> bool {
    if let Some(record) = value.as_object() {
        if record
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|value| secret_slug(trim(value)))
            && record
                .get("reference")
                .and_then(Value::as_str)
                .is_some_and(|value| {
                    let text = trim(value);
                    !text.is_empty()
                        && text.encode_utf16().count() <= 1024
                        && !text.contains(['\r', '\n'])
                })
        {
            return true;
        }
        return record.values().any(references_secret);
    }
    value
        .as_array()
        .is_some_and(|items| items.iter().any(references_secret))
}

fn at(path: &str, key: &str) -> String {
    format!("{path}[{}]", Value::String(key.to_owned()))
}

fn expected(message: &str, path: &str) -> String {
    format!("{message}\n  at {path}")
}
