use serde_json::{Value, json};

use super::providers::{ProviderInstance, driver_default};
use super::{Fields, nonblank, secret_slug, slug, text, trim};

const MODEL_SELECTIONS: &[&str] = &[
    "defaultModelSelection",
    "fallbackModelSelection",
    "speechPostProcessingModelSelection",
    "textGenerationModelSelection",
    "sourceControlWriterModelSelection",
    "reviewGuideModelSelection",
];
const TOGGLES: &[&str] = &[
    "continueThreadsAfterServerUpdate",
    "projectSettingsFolded",
    "enableProviderUpdateChecks",
    "enableAgentBrowserAccess",
    "enableAgentDeviceAccess",
    "enablePreviewTunnel",
    "defaultAutoPull",
    "newWorktreesStartFromOrigin",
    "promptSuggestionsEnabled",
];
const PROJECT_TOGGLES: &[&str] = &[
    "continueThreadsAfterServerUpdate",
    "enableAgentBrowserAccess",
    "enableAgentDeviceAccess",
    "defaultAutoPull",
    "newWorktreesStartFromOrigin",
];
const RUNTIME_MODES: &[&str] = &[
    "approval-required",
    "auto-accept-edits",
    "auto",
    "full-access",
];
const THREAD_ENV_MODES: &[&str] = &["local", "worktree"];
const IDLE_MODES: &[&str] = &["onReturn", "beforeCacheExpires", "off"];
const TEXT_GENERATION_DEFAULTS: &[(&str, &str)] = &[
    ("codex", "gpt-6-luna"),
    ("claudeAgent", "claude-haiku-5-5"),
    ("cursor", "composer-2"),
    ("grok", "grok-build"),
    ("muse", "default"),
    ("pi", "default"),
    ("opencode", "opencode/big-pickle"),
    ("antigravity", "antigravity-default"),
];

pub(super) fn project(
    mut public: Value,
    raw: &Fields,
    instances: &[(String, ProviderInstance)],
) -> Option<Value> {
    let fields = public
        .as_object_mut()
        .expect("settings defaults are an object");
    copy_toggles(fields, raw, TOGGLES);
    copy_model_selections(fields, raw);
    copy_literal(fields, raw, "defaultRuntimeMode", RUNTIME_MODES);
    copy_literal(fields, raw, "defaultThreadEnvMode", THREAD_ENV_MODES);
    copy_streaming_mode(fields, raw);
    for key in [
        "worktreesDirectory",
        "addProjectBaseDirectory",
        "threadTitleInstructions",
    ] {
        if let Some(value) = raw.get(key).and_then(text) {
            fields.insert(key.to_owned(), value.into());
        }
    }
    if let Some(retention) = raw.get("usageHistoryRetentionDays")
        && (retention == "unlimited" || retention.as_u64().is_some_and(|value| value > 0))
    {
        fields.insert("usageHistoryRetentionDays".to_owned(), retention.clone());
    }
    let idle_mode = raw
        .get("idleCompaction")
        .and_then(|value| literal(value, IDLE_MODES))
        .or_else(|| {
            raw.get("compactIdleThreads").and_then(|value| {
                literal(value, IDLE_MODES).or_else(|| {
                    value.as_bool().map(|enabled| {
                        if enabled {
                            "beforeCacheExpires"
                        } else {
                            "onReturn"
                        }
                    })
                })
            })
        })
        .unwrap_or("onReturn");
    fields.insert("idleCompaction".to_owned(), idle_mode.into());
    fields.insert(
        "compactIdleThreads".to_owned(),
        (idle_mode == "beforeCacheExpires").into(),
    );
    if let Some(preferences) = raw
        .get("providerModelPreferences")
        .and_then(Value::as_object)
    {
        let mapped = preferences
            .iter()
            .filter_map(|(id, value)| {
                let id = trim(id);
                if !slug(id) || !value.is_object() {
                    return None;
                }
                Some((
                    id.to_owned(),
                    json!({
                        "hiddenModels": string_list(&value["hiddenModels"]),
                        "modelOrder": string_list(&value["modelOrder"]),
                    }),
                ))
            })
            .collect::<Fields>();
        fields.insert("providerModelPreferences".to_owned(), mapped.into());
    }
    fields.insert(
        "providerInstances".to_owned(),
        instances
            .iter()
            .map(|(key, instance)| (key.clone(), instance.public()))
            .collect::<Fields>()
            .into(),
    );
    project_overrides(fields, raw)?;
    if let Some(secret_providers) = raw.get("secretProviders").and_then(Value::as_object) {
        let mapped = secret_providers
            .iter()
            .filter_map(|(id, provider)| {
                let id = trim(id);
                let kind = nonblank(provider.get("kind")?).filter(|value| secret_slug(value))?;
                let label = nonblank(provider.get("label")?)?;
                if !secret_slug(id) {
                    return None;
                }
                let mut public = json!({"kind":kind,"label":label});
                if kind == "one_password"
                    && let Some(account) = provider
                        .get("config")
                        .and_then(|config| config.get("account"))
                        .and_then(nonblank)
                {
                    public["config"] = json!({"account":account});
                }
                Some((id.to_owned(), public))
            })
            .collect::<Fields>();
        fields.insert("secretProviders".to_owned(), mapped.into());
    }
    apply_text_generation_fallback(fields, instances);
    Some(public)
}

fn project_overrides(public: &mut Fields, raw: &Fields) -> Option<()> {
    let mut projects = Fields::new();
    if let Some(overrides) = raw.get("projectSettingsOverrides") {
        for (id, value) in overrides.as_object()? {
            let value = value.as_object()?;
            let mut fields = Fields::new();
            copy_toggles(&mut fields, value, PROJECT_TOGGLES);
            copy_model_selections(&mut fields, value);
            copy_literal(&mut fields, value, "defaultRuntimeMode", RUNTIME_MODES);
            copy_literal(&mut fields, value, "defaultThreadEnvMode", THREAD_ENV_MODES);
            copy_streaming_mode(&mut fields, value);
            if let Some(instances) = value.get("providerInstances") {
                let mut mapped = Fields::new();
                for (id, instance) in instances.as_object()? {
                    let id = trim(id);
                    if !slug(id) {
                        return None;
                    }
                    let public = match instance.as_object()?.get("enabled") {
                        Some(value) => json!({"enabled":value.as_bool()?}),
                        None => json!({}),
                    };
                    mapped.insert(id.to_owned(), public);
                }
                fields.insert("providerInstances".to_owned(), mapped.into());
            }
            if trim(id).is_empty() {
                return None;
            }
            projects.insert(trim(id).to_owned(), fields.into());
        }
    }
    if public["projectSettingsFolded"] == false {
        let mut folded = false;
        for (legacy, field) in [
            (
                "projectAgentBrowserAccessOverrides",
                "enableAgentBrowserAccess",
            ),
            ("projectAutoPullOverrides", "defaultAutoPull"),
        ] {
            if let Some(overrides) = raw.get(legacy).and_then(Value::as_object) {
                for (id, value) in overrides {
                    if !trim(id).is_empty() && value.is_boolean() {
                        let project = projects
                            .entry(trim(id).to_owned())
                            .or_insert_with(|| Fields::new().into());
                        project
                            .as_object_mut()
                            .expect("project settings object")
                            .entry(field.to_owned())
                            .or_insert_with(|| value.clone());
                        folded = true;
                    }
                }
            }
        }
        if folded {
            public.insert("projectSettingsFolded".to_owned(), true.into());
        }
    }
    for (legacy, field) in [
        (
            "projectAgentBrowserAccessOverrides",
            "enableAgentBrowserAccess",
        ),
        ("projectAutoPullOverrides", "defaultAutoPull"),
    ] {
        let values = projects
            .iter()
            .filter_map(|(id, project)| Some((id.clone(), project.get(field)?.clone())))
            .collect::<Fields>();
        public.insert(legacy.to_owned(), values.into());
    }
    public.insert("projectSettingsOverrides".to_owned(), projects.into());
    Some(())
}

fn apply_text_generation_fallback(public: &mut Fields, instances: &[(String, ProviderInstance)]) {
    let current = &public["textGenerationModelSelection"];
    let instance_id = current["instanceId"]
        .as_str()
        .expect("normalized text generation selection");
    let find = |id: &str| {
        instances
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, instance)| instance)
    };
    let usable = find(instance_id).map_or_else(
        || matches!(instance_id, "codex" | "claudeAgent"),
        |instance| instance.enabled() && instance.driver != "acpRegistry",
    );
    if usable {
        return;
    }
    let candidates = TEXT_GENERATION_DEFAULTS
        .iter()
        .map(|&(id, model)| (id, model))
        .chain(instances.iter().filter_map(|(id, instance)| {
            let model = TEXT_GENERATION_DEFAULTS
                .iter()
                .find(|(driver, _)| *driver == instance.driver)?
                .1;
            Some((id.as_str(), model))
        }));
    for (id, model) in candidates {
        let instance = find(id);
        if !instance.map_or_else(|| driver_default(id), ProviderInstance::enabled) {
            continue;
        }
        let preferred = &public["defaultModelSelection"];
        let scope_model = preferred["model"].as_str();
        let has_custom_default = preferred["instanceId"] == id
            && instance
                .and_then(|instance| instance.config.as_ref())
                .and_then(|config| config["customModels"].as_array())
                .is_some_and(|models| {
                    models.iter().any(|model| {
                        text(model).or_else(|| model.get("slug").and_then(text)) == scope_model
                    })
                });
        let fallback = if has_custom_default {
            preferred.clone()
        } else {
            json!({"instanceId":id,"model":model})
        };
        public.insert("textGenerationModelSelection".to_owned(), fallback);
        break;
    }
}

fn copy_toggles(target: &mut Fields, raw: &Fields, keys: &[&str]) {
    for &key in keys {
        if let Some(value) = raw.get(key).and_then(Value::as_bool) {
            target.insert(key.to_owned(), value.into());
        }
    }
}

fn copy_literal(target: &mut Fields, raw: &Fields, key: &str, values: &[&str]) {
    if let Some(value) = raw.get(key).and_then(|value| literal(value, values)) {
        target.insert(key.to_owned(), value.into());
    }
}

fn literal<'a>(value: &'a Value, allowed: &[&str]) -> Option<&'a str> {
    value.as_str().filter(|value| allowed.contains(value))
}

fn copy_streaming_mode(target: &mut Fields, raw: &Fields) {
    if let Some(value) = raw.get("responseStreamingMode").and_then(Value::as_str) {
        match value {
            "turn" => target.insert("responseStreamingMode".to_owned(), "turn".into()),
            "paragraph" | "token" => {
                target.insert("responseStreamingMode".to_owned(), "paragraph".into())
            }
            _ => None,
        };
    }
}

fn copy_model_selections(target: &mut Fields, raw: &Fields) {
    for &key in MODEL_SELECTIONS {
        if let Some(value) = raw.get(key) {
            if value.is_null() && key != "textGenerationModelSelection" {
                target.insert(key.to_owned(), Value::Null);
            } else if let Some(model) = model_selection(value) {
                target.insert(key.to_owned(), model);
            }
        }
    }
}

fn model_selection(raw: &Value) -> Option<Value> {
    let fields = raw.as_object()?;
    let instance_id = nonblank(
        fields
            .get("instanceId")
            .or_else(|| fields.get("provider"))?,
    )?;
    if !slug(instance_id) {
        return None;
    }
    let mut selection = json!({"instanceId":instance_id,"model":nonblank(fields.get("model")?)?});
    if let Some(options) = fields.get("options").filter(|value| !value.is_null()) {
        let result: Vec<Value> = if let Some(options) = options.as_array() {
            options
                .iter()
                .map(|entry| option(nonblank(entry.get("id")?)?, entry.get("value")?))
                .collect::<Option<_>>()?
        } else {
            let mut entries: Vec<_> = options.as_object()?.iter().collect();
            // Object.entries visits array indices first, before keys in insertion order.
            entries.sort_by_key(|(id, _)| {
                id.parse::<u32>()
                    .ok()
                    .filter(|value| value.to_string() == **id)
                    .unwrap_or(u32::MAX)
            });
            entries
                .into_iter()
                .filter_map(|(id, value)| option(trim(id), value))
                .collect()
        };
        selection["options"] = result.into();
    }
    Some(selection)
}

fn option(id: &str, value: &Value) -> Option<Value> {
    if id.is_empty() {
        return None;
    }
    let value = value
        .as_bool()
        .map(Value::from)
        .or_else(|| nonblank(value).map(Value::from))?;
    Some(json!({"id":id,"value":value}))
}

fn string_list(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}
