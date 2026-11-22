use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use t3_rune::providers::codex::ReasoningEffort;
use t3_rune::providers::{CodexAppServer, CodexError};
use time::OffsetDateTime;

use crate::server::launch_model::{Model, read_pages};
use crate::server::settings::{CodexInstanceError, SettingsSnapshot};
use crate::server::thread_commands::trim_js;
use crate::server::turn_commands::{codex_default_effort_override, timestamp};
use crate::server::turn_worker::launch::process_config;

mod claude;

const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    account: Option<Identity>,
    requires_openai_auth: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Identity {
    ApiKey,
    AmazonBedrock,
    Chatgpt {
        email: Option<String>,
        #[serde(rename = "planType")]
        plan_type: String,
    },
}

pub(super) async fn initial(settings: &SettingsSnapshot, cwd: &Path) -> Vec<Value> {
    let (mut providers, claude) =
        tokio::join!(initial_codex(settings, cwd), claude::status(settings, cwd));
    providers.extend(claude);
    providers
}

async fn initial_codex(settings: &SettingsSnapshot, cwd: &Path) -> Vec<Value> {
    let public = settings.for_client();
    let instances = public["providerInstances"].as_object();
    let mut providers = Vec::new();
    if instances.is_none_or(|configured| !configured.contains_key("codex")) {
        providers.push(codex(settings, cwd, "codex", &Value::Null).await);
    }
    for (id, config) in instances.into_iter().flatten() {
        if config["driver"] == "codex" {
            providers.push(codex(settings, cwd, id, config).await);
        }
    }
    providers
}

async fn codex(settings: &SettingsSnapshot, cwd: &Path, id: &str, config: &Value) -> Value {
    let mut result = json!({
        "instanceId": id,
        "driver": "codex",
        "displayName": config["displayName"].as_str().unwrap_or("Codex"),
        "showInteractionModeToggle": true,
        "reportsContextWindow": true,
        "enabled": true,
        "installed": false,
        "version": null,
        "status": "error",
        "auth": {"status": "unknown"},
        "checkedAt": timestamp(OffsetDateTime::now_utc()),
        "models": [],
        "slashCommands": [],
        "skills": [],
    });
    if let Some(accent) = config["accentColor"].as_str() {
        result["accentColor"] = accent.into();
    }
    let instance = match settings.codex_instance(None, id) {
        Ok(instance) => instance,
        Err(CodexInstanceError::Disabled) => {
            result["enabled"] = false.into();
            result["status"] = "disabled".into();
            result["message"] = "Codex is disabled in T3 Code settings.".into();
            return result;
        }
        Err(error) => {
            result["message"] = error.to_string().into();
            return result;
        }
    };
    let executable = instance.binary_path.clone();
    let configured_home = instance.home_path.clone();
    let process = match process_config(instance, cwd.to_owned()) {
        Ok(process) => process,
        Err(error) => {
            result["message"] = error.to_string().into();
            return result;
        }
    };
    let probe = async {
        let server = match CodexAppServer::launch(process).await {
            Ok(server) => server,
            Err(CodexError::Spawn(_)) => {
                result["message"] = start_failure(&executable).into();
                return;
            }
            Err(_) => {
                result["installed"] = true.into();
                result["message"] = "Codex did not return its app-server initialization.".into();
                return;
            }
        };
        result["installed"] = true.into();
        if let Some(version) = server
            .initialization
            .user_agent
            .split_once('/')
            .and_then(|(_, suffix)| suffix.split_whitespace().next())
        {
            result["version"] = version.into();
        }
        result["continuation"] = json!({
            "groupKey": format!("codex:home:{}", server.initialization.codex_home.display()),
        });
        match server
            .client()
            .request("account/read", Some(json!({})), ACCOUNT_TIMEOUT)
            .await
        {
            Ok(raw) => match serde_json::from_value::<Account>(raw) {
                Ok(account) => {
                    apply_account(&mut result, &server, account, !configured_home.is_empty()).await
                }
                Err(_) => result["message"] = "Codex returned an invalid account response.".into(),
            },
            Err(_) => result["message"] = "Codex did not return its account status.".into(),
        }
        let _ = server.shutdown().await;
    };
    if tokio::time::timeout(PROBE_TIMEOUT, probe).await.is_err()
        && result["status"] == "error"
        && result.get("message").is_none()
    {
        result["message"] = "Timed out while checking Codex app-server provider status.".into();
    }
    result
}

async fn apply_account(
    result: &mut Value,
    server: &CodexAppServer,
    account: Account,
    custom_home: bool,
) {
    if let Some(identity) = account.account {
        result["auth"] = auth(identity);
    } else if account.requires_openai_auth {
        result["auth"]["status"] = "unauthenticated".into();
        let command = if custom_home {
            let home = server.initialization.codex_home.to_string_lossy();
            let quoted = serde_json::to_string(&home).expect("serializable path");
            format!("Run `codex login` with CODEX_HOME set to {quoted}")
        } else {
            "Run `codex login`".to_owned()
        };
        result["message"] =
            format!("Codex CLI is not authenticated. {command} and try again.").into();
        return;
    }
    result["status"] = "warning".into();
    result["message"] = "Codex did not return its model catalog in time.".into();
    if let Ok(models) = read_pages(server).await {
        result["status"] = "ready".into();
        if let Some(fields) = result.as_object_mut() {
            fields.shift_remove("message");
        }
        result["models"] = models
            .into_iter()
            .filter_map(wire_model)
            .collect::<Vec<_>>()
            .into();
    }
}

fn auth(identity: Identity) -> Value {
    match identity {
        Identity::ApiKey => {
            json!({"status": "authenticated", "type": "apiKey", "label": "OpenAI API Key"})
        }
        Identity::AmazonBedrock => {
            json!({"status": "authenticated", "type": "amazonBedrock", "label": "Amazon Bedrock"})
        }
        Identity::Chatgpt { email, plan_type } => {
            let mut auth = json!({"status": "authenticated", "type": "chatgpt"});
            if let Some(email) = email
                .as_deref()
                .map(trim_js)
                .filter(|email| !email.is_empty())
            {
                auth["email"] = email.into();
            }
            let plan = match plan_type.as_str() {
                "free" => " Free",
                "go" => " Go",
                "plus" => " Plus",
                "pro" => " Pro 200",
                "prolite" => " Pro 100",
                "promax" => " Pro 500",
                "self_serve_business_prolite" => " Business Premium",
                "team" | "self_serve_business_usage_based" | "business" => " Business",
                "ent26"
                | "enterprise_cbp_automation"
                | "enterprise_cbp_usage_based"
                | "enterprise" => " Enterprise",
                "edu" | "edu_plus" | "edu_pro" => " Edu",
                "unknown" => "",
                _ => return auth,
            };
            auth["label"] = format!("ChatGPT{plan} Subscription").into();
            auth
        }
    }
}

fn start_failure(executable: &str) -> String {
    let detail = if executable == "codex" {
        "Installing ChatGPT or Codex desktop may not add codex to PATH."
    } else {
        "Make sure the configured executable exists and can be run."
    };
    format!(
        "Could not start Codex CLI (`{executable}`). Check Settings → Providers → Codex → Binary path on the server. {detail}"
    )
}

fn wire_model(model: Model) -> Option<Value> {
    let slug = trim_js(&model.model);
    if slug.is_empty() {
        return None;
    }
    let name = model
        .display_name
        .as_deref()
        .map(trim_js)
        .filter(|name| !name.is_empty())
        .unwrap_or(slug);
    let descriptors: Vec<_> = [
        reasoning_descriptor(slug, &model),
        service_tier_descriptor(&model),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut value = json!({
        "slug": slug,
        "name": model_name(name),
        "isCustom": false,
        "capabilities": if descriptors.is_empty() { Value::Null } else { json!({"optionDescriptors":descriptors}) },
    });
    if model.is_default {
        value["isDefault"] = true.into();
    }
    Some(value)
}

fn reasoning_descriptor(slug: &str, model: &Model) -> Option<Value> {
    let default = codex_default_effort_override(slug)
        .map(ReasoningEffort::as_str)
        .or(model.default_reasoning_effort.as_deref());
    let options: Vec<_> = model
        .supported_reasoning_efforts
        .iter()
        .map(|option| {
            let id = option.reasoning_effort.as_str();
            let label = match id {
                "none" => "None",
                "minimal" => "Minimal",
                "low" => "Low",
                "medium" => "Medium",
                "high" => "High",
                "xhigh" => "Extra High",
                "max" => "Max",
                "ultra" => "Ultra",
                other => other,
            };
            let mut row = json!({"id":id,"label":label});
            if Some(id) == default {
                row["isDefault"] = true.into();
            }
            row
        })
        .collect();
    if options.is_empty() {
        return None;
    }
    let mut reasoning = json!({
        "id":ReasoningEffort::OPTION_ID,"label":"Reasoning","type":"select","options":options
    });
    if let Some(default) =
        default.filter(|default| options.iter().any(|option| option["id"] == *default))
    {
        reasoning["currentValue"] = default.into();
    }
    Some(reasoning)
}

fn service_tier_descriptor(model: &Model) -> Option<Value> {
    let tiers: Vec<_> = if let Some(reported) = model
        .service_tiers
        .as_ref()
        .filter(|tiers| !tiers.is_empty())
    {
        reported
            .iter()
            .map(|tier| (tier.id.as_str(), tier.name.as_str()))
            .collect()
    } else {
        model
            .additional_speed_tiers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|tier| {
                let id = tier.as_str();
                (id, if id == "fast" { "Fast" } else { id })
            })
            .collect()
    };
    if tiers.is_empty() {
        return None;
    }
    let default = model
        .default_service_tier
        .as_deref()
        .filter(|default| tiers.iter().any(|(tier, _)| tier == default))
        .unwrap_or("default");
    let options: Vec<_> = std::iter::once(("default", "Standard"))
        .chain(tiers)
        .map(|(id, label)| {
            let mut option = json!({"id":id,"label":label});
            if id == default {
                option["isDefault"] = true.into();
            }
            option
        })
        .collect();
    Some(
        json!({"id":"serviceTier","label":"Service Tier","type":"select","options":options,"currentValue":default}),
    )
}

fn model_name(name: &str) -> String {
    let (prefix, tail) = if name
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gpt"))
    {
        ("GPT", &name[3..])
    } else {
        ("", name)
    };
    let mut output = prefix.to_owned();
    let mut after_dash = false;
    for character in tail.chars() {
        output.push(if after_dash {
            character.to_ascii_uppercase()
        } else {
            character
        });
        after_dash = character == '-';
    }
    output
}
