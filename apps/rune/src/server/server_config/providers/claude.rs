use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use t3_rune::providers::claude::{Error, ProbeAccount, ProbeLaunch, ProbeModel};
use time::OffsetDateTime;
use tokio::time::timeout;

use crate::server::claude_worker::configured_process;
use crate::server::settings::SettingsSnapshot;
use crate::server::thread_commands::trim_js;
use crate::server::turn_commands::{supports_plain_claude_config, timestamp};

const INSTANCE: &str = "claudeAgent";
const SONNET: &str = "claude-sonnet-4-6";

pub(super) async fn status(settings: &SettingsSnapshot, cwd: &Path) -> Option<Value> {
    let public = settings.for_client();
    let configured = public["providerInstances"].get(INSTANCE);
    if configured.is_some_and(|config| config["driver"] != INSTANCE) {
        return None;
    }
    let mut provider = json!({
        "instanceId": INSTANCE, "driver": INSTANCE,
        "displayName": configured.and_then(|config| config["displayName"].as_str()).unwrap_or("Claude"),
        "nativeSavedThreadTurns": true, "nativeRootLaunch": true,
        "showInteractionModeToggle": false, "reportsContextWindow": true,
        "supportedRuntimeModes":["full-access"], "supportsTextGeneration":false, "supportsConversationRollback":false,
        "enabled": true, "installed": false, "version": null, "status": "error",
        "auth":{"status":"unknown"},
        "checkedAt": timestamp(OffsetDateTime::now_utc()), "models": [], "slashCommands": [], "skills": [],
    });
    if let Some(accent) = configured.and_then(|config| config["accentColor"].as_str()) {
        provider["accentColor"] = accent.into();
    }
    if configured.is_some_and(|config| config["enabled"] == false) {
        provider["enabled"] = false.into();
        provider["status"] = "disabled".into();
        provider["message"] = "Claude is disabled in T3 Code settings.".into();
        return Some(provider);
    }
    let Some(config) = settings.claude_instance(None, INSTANCE) else {
        return configured.map(|_| {
            provider["availability"] = "unavailable".into();
            provider["message"] = "Could not read the Claude Code provider configuration.".into();
            provider
        });
    };
    if !supports_plain_claude_config(&config, SONNET) {
        provider["availability"] = "unavailable".into();
        provider["message"] =
            "This Claude Code configuration is unavailable for saved-thread text turns.".into();
        return Some(provider);
    }
    let (home, process) = match configured_process(config, cwd) {
        Ok(process) => process,
        Err(error) => {
            provider["availability"] = "unavailable".into();
            provider["message"] = error.to_string().into();
            return Some(provider);
        }
    };
    let managed = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode/managed-mcp.json"
    } else if cfg!(windows) {
        "C:\\Program Files\\ClaudeCode\\managed-mcp.json"
    } else {
        "/etc/claude-code/managed-mcp.json"
    };
    let probe = ProbeLaunch {
        executable: process.executable,
        working_directory: cwd.to_owned(),
        environment: process.variables,
        managed_mcp: Path::new(managed).is_file(),
    };

    let version = match timeout(Duration::from_secs(5), probe.version()).await {
        Ok(Ok(version)) => version,
        Ok(Err(Error::Spawn(error))) if error.kind() == std::io::ErrorKind::NotFound => {
            provider["message"] = "Claude Agent CLI (`claude`) was not found on PATH.".into();
            return Some(provider);
        }
        _ => {
            provider["installed"] = true.into();
            provider["message"] = "Claude Agent CLI is installed but failed to run.".into();
            return Some(provider);
        }
    };
    provider["installed"] = true.into();
    provider["version"] = version.into();
    if let Some(home) = home {
        provider["continuation"] = json!({"groupKey":format!("claude:home:{}",home.display())});
    }
    let Ok(Ok(snapshot)) = timeout(Duration::from_secs(15), probe.inspect()).await else {
        provider["status"] = "warning".into();
        provider["message"] =
            "Could not verify Claude authentication status from initialization result.".into();
        return Some(provider);
    };
    let auth = snapshot
        .account
        .as_ref()
        .map_or_else(|| json!({"status":"unknown"}), account);
    if auth["status"] != "authenticated" {
        provider["message"] = if auth["status"] == "unauthenticated" {
            "Claude Code is not authenticated. Run `claude auth login` and try again."
        } else {
            "Could not verify Claude authentication status from initialization result."
        }
        .into();
        provider["auth"] = auth;
        return Some(provider);
    }
    provider["auth"] = auth;
    if snapshot.models.iter().any(is_sonnet) {
        provider["status"] = "ready".into();
        provider["availability"] = "available".into();
        provider["models"] = json!([{
            "slug":SONNET, "name":"Claude Sonnet 4.6", "shortName":"Sonnet 4.6",
            "isCustom":false, "isLegacy":true, "supportsTextGeneration":false, "capabilities":null,
        }]);
    } else {
        provider["status"] = "warning".into();
        provider["availability"] = "unavailable".into();
        provider["message"] = "Claude Code did not report Sonnet 4.6.".into();
    }
    Some(provider)
}

fn is_sonnet(model: &ProbeModel) -> bool {
    let name = trim_js(&model.display_name);
    matches!(trim_js(&model.value), SONNET | "sonnet-4.6")
        || (trim_js(&model.value) == "sonnet" && matches!(name, "Sonnet 4.6" | "Claude Sonnet 4.6"))
}

fn account(account: &ProbeAccount) -> Value {
    let email = account
        .email
        .as_deref()
        .map(trim_js)
        .filter(|text| !text.is_empty());
    let subscription = account
        .subscription_type
        .as_deref()
        .map(trim_js)
        .filter(|text| !text.is_empty());
    let token = account
        .token_source
        .as_deref()
        .map(trim_js)
        .filter(|text| !text.is_empty());
    let key = account
        .api_key_source
        .as_deref()
        .map(trim_js)
        .is_some_and(|source| !source.is_empty() && source != "none");
    if account
        .api_provider
        .as_deref()
        .is_some_and(|provider| provider != "firstParty")
    {
        return json!({"status":"unknown"});
    }
    if email.is_none()
        && subscription.is_none()
        && !key
        && token.is_none_or(|token| token == "none")
    {
        return json!({"status":if token == Some("none") {"unauthenticated"} else {"unknown"}});
    }
    let mut auth = json!({"status":"authenticated"});
    if let Some(email) = email {
        auth["email"] = email.into();
    }
    let method = token
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "");
    if key
        || matches!(
            method.as_str(),
            "apikey" | "anthropicapikey" | "anthropicauthtoken"
        )
    {
        auth["type"] = "apiKey".into();
        auth["label"] = "Claude API Key".into();
    } else if let Some(subscription) = subscription {
        let name = subscription
            .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
            .filter(|word| !word.is_empty())
            .map(|word| {
                let mut chars = word.chars();
                format!(
                    "{}{}",
                    chars.next().expect("nonempty word").to_uppercase(),
                    chars.as_str().to_lowercase()
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        auth["type"] = subscription.into();
        let label = if name.starts_with("Claude ") {
            name
        } else {
            format!("Claude {name}")
        };
        auth["label"] = if label.ends_with(" Subscription") {
            label
        } else {
            format!("{label} Subscription")
        }
        .into();
    }
    auth
}
