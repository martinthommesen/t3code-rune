use std::fmt;

use serde_json::{Value, json};

use super::{Fields, nonblank, secret_slug, slug, text};

pub(in crate::server) struct CodexInstance {
    pub(in crate::server) binary_path: String,
    pub(in crate::server) home_path: String,
    pub(in crate::server) shadow_home_path: String,
    pub(in crate::server) launch_args: String,
    pub(in crate::server) custom_models: Vec<Value>,
    pub(in crate::server) environment: Vec<EnvironmentVariable>,
}

pub(in crate::server) struct ClaudeInstance {
    pub(in crate::server) binary_path: String,
    pub(in crate::server) home_path: String,
    pub(in crate::server) launch_args: String,
    pub(in crate::server) auto_compact_window: Option<u32>,
    pub(in crate::server) custom_models: Vec<Value>,
    pub(in crate::server) environment: Vec<EnvironmentVariable>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::server) enum CodexInstanceError {
    Unavailable,
    Disabled,
    InvalidSettings,
    InvalidConfig,
}

impl fmt::Display for CodexInstanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "The selected Codex instance is unavailable.",
            Self::Disabled => "The selected Codex instance is disabled for this project.",
            Self::InvalidSettings => "The environment's settings could not be decoded.",
            Self::InvalidConfig => "The selected Codex instance has invalid settings.",
        })
    }
}

impl std::error::Error for CodexInstanceError {}

#[derive(Clone)]
pub(in crate::server) struct EnvironmentVariable {
    pub(in crate::server) name: String,
    pub(in crate::server) value: EnvironmentValue,
    sensitive: bool,
}

#[derive(Clone)]
pub(in crate::server) enum EnvironmentValue {
    Literal(String),
    Saved,
    Reference { provider: String, reference: String },
}

pub(super) struct ProviderInstance {
    pub(super) driver: String,
    display_name: Option<String>,
    accent_color: Option<String>,
    enabled: Option<bool>,
    pub(super) config: Option<Value>,
    environment: Option<Vec<EnvironmentVariable>>,
}

impl ProviderInstance {
    fn parse(value: &Value) -> Option<Self> {
        let fields = value.as_object()?;
        let driver = nonblank(fields.get("driver")?).filter(|value| slug(value))?;
        let display_name = optional_text(fields.get("displayName"))?;
        let accent_color = optional_text(fields.get("accentColor"))?;
        let enabled = optional_bool(fields.get("enabled"))?;
        let environment = match fields.get("environment") {
            Some(value) => Some(
                value
                    .as_array()?
                    .iter()
                    .map(EnvironmentVariable::parse)
                    .collect::<Option<Vec<_>>>()?,
            ),
            None => None,
        };
        let mut instance = Self {
            driver: driver.to_owned(),
            display_name,
            accent_color,
            enabled,
            config: fields.get("config").cloned(),
            environment,
        };
        if let Some(config) = instance.config.as_mut().and_then(Value::as_object_mut)
            && let Some(config_enabled) = config.get("enabled").and_then(Value::as_bool)
        {
            instance.enabled = Some(instance.enabled.unwrap_or(true) && config_enabled);
            config.shift_remove("enabled");
        }
        Some(instance)
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled.unwrap_or_else(|| driver_default(&self.driver))
    }

    pub(super) fn public(&self) -> Value {
        let mut fields = Fields::new();
        fields.insert("driver".to_owned(), self.driver.clone().into());
        if let Some(value) = &self.display_name {
            fields.insert("displayName".to_owned(), value.clone().into());
        }
        if let Some(value) = &self.accent_color {
            fields.insert("accentColor".to_owned(), value.clone().into());
        }
        if let Some(enabled) = self.enabled {
            fields.insert("enabled".to_owned(), enabled.into());
        }
        if let Some(environment) = &self.environment {
            fields.insert(
                "environment".to_owned(),
                environment
                    .iter()
                    .map(EnvironmentVariable::public)
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        if let Some(config) = self.config.as_ref().and_then(Value::as_object) {
            let kept = config
                .iter()
                .filter_map(|(key, value)| {
                    if self.driver != "codex" {
                        return None;
                    }
                    let public = match key.as_str() {
                        "setupMode" | "binaryPath" | "homePath" | "shadowHomePath"
                        | "launchArgs"
                            if value.is_string() =>
                        {
                            value.clone()
                        }
                        "customModels" => value
                            .as_array()?
                            .iter()
                            .filter_map(public_custom_model)
                            .collect::<Vec<_>>()
                            .into(),
                        _ => return None,
                    };
                    Some((key.clone(), public))
                })
                .collect::<Fields>();
            fields.insert("config".to_owned(), kept.into());
        }
        fields.into()
    }
}

impl EnvironmentVariable {
    fn parse(input: &Value) -> Option<Self> {
        let fields = input.as_object()?;
        let name = nonblank(fields.get("name")?)
            .filter(|name| env_name(name))?
            .to_owned();
        let value = match fields.get("value") {
            Some(value) => value.as_str()?,
            None => "",
        };
        let sensitive = optional_bool(fields.get("sensitive"))?.unwrap_or(false);
        let saved = optional_bool(fields.get("valueRedacted"))?.unwrap_or(false);
        if let Some(secret) = fields.get("secret") {
            let provider =
                nonblank(secret.get("provider")?).filter(|provider| secret_slug(provider))?;
            let reference = nonblank(secret.get("reference")?).filter(|reference| {
                !reference.contains(['\n', '\r']) && reference.encode_utf16().count() <= 1024
            })?;
            return Some(Self {
                name,
                value: EnvironmentValue::Reference {
                    provider: provider.to_owned(),
                    reference: reference.to_owned(),
                },
                sensitive: true,
            });
        }
        let value = if sensitive && saved {
            EnvironmentValue::Saved
        } else {
            EnvironmentValue::Literal(value.to_owned())
        };
        Some(Self {
            name,
            value,
            sensitive,
        })
    }

    fn public(&self) -> Value {
        let mut field = json!({"name":self.name, "value":"", "sensitive":self.sensitive});
        match &self.value {
            EnvironmentValue::Literal(value) if !self.sensitive => {
                field["value"] = value.clone().into()
            }
            EnvironmentValue::Literal(value) if !value.is_empty() => {
                field["valueRedacted"] = true.into()
            }
            EnvironmentValue::Saved => field["valueRedacted"] = true.into(),
            EnvironmentValue::Reference {
                provider,
                reference,
            } => {
                field["secret"] = json!({"provider":provider,"reference":reference});
            }
            EnvironmentValue::Literal(_) => {}
        }
        field
    }
}

fn env_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn optional_bool(value: Option<&Value>) -> Option<Option<bool>> {
    match value {
        Some(value) => value.as_bool().map(Some),
        None => Some(None),
    }
}

fn optional_text(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        Some(value) => nonblank(value).map(|value| Some(value.to_owned())),
        None => Some(None),
    }
}

pub(super) fn driver_default(driver: &str) -> bool {
    !matches!(
        driver,
        "cursor" | "grok" | "fx" | "kiro" | "muse" | "pi" | "opencode" | "antigravity"
    )
}

pub(super) fn resolve_instances(raw: &Fields) -> Result<Vec<(String, ProviderInstance)>, ()> {
    let mut instances = Vec::new();
    if let Some(explicit) = raw.get("providerInstances") {
        for (id, entry) in explicit.as_object().ok_or(())? {
            let id = super::trim(id);
            if !slug(id) {
                return Err(());
            }
            let instance = ProviderInstance::parse(entry).ok_or(())?;
            if let Some((_, previous)) = instances.iter_mut().find(|(key, _)| key == id) {
                *previous = instance;
            } else {
                instances.push((id.to_owned(), instance));
            }
        }
    }
    if let Some(legacy) = raw.get("providers") {
        for (driver, blob) in legacy.as_object().ok_or(())? {
            let Some(raw_config) = blob.as_object() else {
                continue;
            };
            if !slug(driver) || instances.iter().any(|(id, _)| id == driver) {
                continue;
            }
            let mut config = raw_config.clone();
            let enabled = config
                .shift_remove("enabled")
                .and_then(|flag| flag.as_bool());
            if config.is_empty()
                && enabled.unwrap_or_else(|| driver_default(driver)) == driver_default(driver)
            {
                continue;
            }
            instances.push((
                driver.clone(),
                ProviderInstance {
                    driver: driver.clone(),
                    display_name: None,
                    accent_color: None,
                    enabled,
                    config: Some(Value::Object(config)),
                    environment: None,
                },
            ));
        }
    }
    Ok(instances)
}

pub(super) fn codex_config(
    instance: Option<&ProviderInstance>,
) -> Result<CodexInstance, CodexInstanceError> {
    let invalid = || CodexInstanceError::InvalidConfig;
    let empty = Fields::new();
    let fields = match instance.and_then(|instance| instance.config.as_ref()) {
        None | Some(Value::Null) => &empty,
        Some(value) => value.as_object().ok_or_else(invalid)?,
    };
    if optional_bool(fields.get("enabled")).is_none()
        || fields
            .get("setupMode")
            .is_some_and(|value| !matches!(value.as_str(), Some("managed" | "existing")))
    {
        return Err(invalid());
    }
    let get_string = |key, fallback| match fields.get(key) {
        Some(value) => text(value).map(str::to_owned).ok_or_else(invalid),
        None => Ok(String::from(fallback)),
    };
    let binary_path = get_string("binaryPath", "codex")?;
    Ok(CodexInstance {
        binary_path: if binary_path.is_empty() {
            "codex".to_owned()
        } else {
            binary_path
        },
        home_path: get_string("homePath", "")?,
        shadow_home_path: get_string("shadowHomePath", "")?,
        launch_args: get_string("launchArgs", "")?,
        custom_models: custom_models(fields).ok_or_else(invalid)?,
        environment: instance
            .and_then(|instance| instance.environment.clone())
            .unwrap_or_default(),
    })
}

pub(super) fn claude_config(instance: Option<&ProviderInstance>) -> Option<ClaudeInstance> {
    let empty = Fields::new();
    let fields = match instance.and_then(|instance| instance.config.as_ref()) {
        None | Some(Value::Null) => &empty,
        Some(value) => value.as_object()?,
    };
    optional_bool(fields.get("enabled"))?;
    if let Some(value) = fields.get("idleCompaction")
        && !matches!(
            value.as_str(),
            Some("default" | "onReturn" | "beforeCacheExpires" | "off")
        )
    {
        return None;
    }
    let string = |key| fields.get(key).map_or(Some(""), Value::as_str);
    let binary_path = super::trim(string("binaryPath")?);
    let auto_compact_window = match super::trim(string("autoCompactWindow")?) {
        "" => None,
        window if !window.starts_with('0') && window.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(
                window
                    .parse()
                    .ok()
                    .filter(|tokens| (100_000..=1_000_000).contains(tokens))?,
            )
        }
        _ => return None,
    };
    Some(ClaudeInstance {
        binary_path: if binary_path.is_empty() {
            "claude"
        } else {
            binary_path
        }
        .to_owned(),
        home_path: super::trim(string("homePath")?).to_owned(),
        launch_args: string("launchArgs")?.to_owned(),
        auto_compact_window,
        custom_models: custom_models(fields)?,
        environment: instance
            .and_then(|instance| instance.environment.clone())
            .unwrap_or_default(),
    })
}

fn custom_models(fields: &Fields) -> Option<Vec<Value>> {
    fields.get("customModels").map_or_else(
        || Some(Vec::new()),
        |value| {
            value
                .as_array()?
                .iter()
                .map(normalize_custom_model)
                .collect()
        },
    )
}

fn public_custom_model(model: &Value) -> Option<Value> {
    if model.is_string() {
        return Some(model.clone());
    }
    let fields = model.as_object()?;
    Some(
        fields
            .iter()
            .filter(|(key, value)| value.is_string() && matches!(key.as_str(), "slug" | "name"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Fields>()
            .into(),
    )
}

fn normalize_custom_model(model: &Value) -> Option<Value> {
    if model.is_string() {
        return Some(model.clone());
    }
    let mut normalized = json!({"slug":nonblank(model.get("slug")?)?});
    if let Some(name) = optional_text(model.get("name"))? {
        normalized["name"] = name.into();
    }
    Some(normalized)
}
