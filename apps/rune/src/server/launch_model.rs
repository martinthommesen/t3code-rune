use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use t3_rune::providers::{CodexAppServer, CodexLaunch};

use super::project_commands::ProjectCommands;
use super::settings::SettingsFile;
use super::turn_worker::launch::process_config;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const PREFERRED_MODELS: [&str; 3] = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra"];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Selection {
    instance_id: &'static str,
    model: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    data: Vec<Model>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Model {
    pub(super) model: String,
    #[serde(default)]
    pub(super) is_default: bool,
    pub(super) display_name: Option<String>,
    pub(super) default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub(super) supported_reasoning_efforts: Vec<ModelEffort>,
    pub(super) service_tiers: Option<Vec<ModelServiceTier>>,
    pub(super) additional_speed_tiers: Option<Vec<String>>,
    pub(super) default_service_tier: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ModelServiceTier {
    pub(super) id: String,
    pub(super) name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelEffort {
    pub(super) reasoning_effort: String,
}

pub(super) async fn discover(
    projects: ProjectCommands,
    settings: SettingsFile,
    project_id: String,
) -> Result<Selection, &'static str> {
    let (process, hidden) = tokio::task::spawn_blocking(move || {
        let cwd = projects
            .active_workspace(&project_id)
            .ok()
            .flatten()
            .ok_or("The project is no longer available for new threads.")?;
        let settings = settings
            .read()
            .map_err(|_| "The environment's settings could not be read.")?;
        let instance = settings
            .codex_instance(Some(&project_id), "codex")
            .map_err(|_| "Codex is unavailable for this project.")?;
        let process =
            process_config(instance, cwd).map_err(|_| "Codex has unsupported launch settings.")?;
        let public = settings.for_client();
        let hidden = public["providerModelPreferences"]["codex"]["hiddenModels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|model| model.as_str().map(str::to_owned))
            .collect();
        Ok::<_, &'static str>((process, hidden))
    })
    .await
    .map_err(|_| "The environment's settings could not be read.")??;

    let model = tokio::time::timeout(DISCOVERY_TIMEOUT, default_model(process, hidden))
        .await
        .map_err(|_| "Codex did not return its model list in time.")??;
    Ok(Selection {
        instance_id: "codex",
        model,
    })
}

async fn default_model(
    process: CodexLaunch,
    hidden: HashSet<String>,
) -> Result<String, &'static str> {
    let server = CodexAppServer::launch(process)
        .await
        .map_err(|_| "Could not start Codex to look up available models.")?;
    let models = read_pages(&server).await;
    let _ = server.shutdown().await;
    let models = models?;
    let candidates = models
        .iter()
        .filter(|model| !model.model.trim().is_empty() && !hidden.contains(&model.model))
        .map(|model| (model.model.as_str(), model.is_default));
    Ok(preferred_codex_model(candidates)
        .unwrap_or(PREFERRED_MODELS[0])
        .to_owned())
}

pub(super) async fn read_pages(server: &CodexAppServer) -> Result<Vec<Model>, &'static str> {
    let client = server.client();
    let mut models = Vec::new();
    let mut cursors = HashSet::new();
    let mut params = json!({});
    loop {
        let page = client
            .request("model/list", Some(params), DISCOVERY_TIMEOUT)
            .await
            .map_err(|_| "Codex could not list its available models.")?;
        let page: Page =
            serde_json::from_value(page).map_err(|_| "Codex returned an invalid model list.")?;
        models.extend(page.data);
        let Some(cursor) = page.next_cursor.filter(|cursor| !cursor.is_empty()) else {
            return Ok(models);
        };
        if !cursors.insert(cursor.clone()) {
            return Err("Codex repeated a model list page.");
        }
        params = json!({"cursor":cursor});
    }
}

pub(crate) fn preferred_codex_model<'a>(
    models: impl Iterator<Item = (&'a str, bool)> + Clone,
) -> Option<&'a str> {
    PREFERRED_MODELS
        .into_iter()
        .find_map(|preferred| {
            models
                .clone()
                .find(|(slug, _)| model_family(slug) == preferred)
                .map(|(slug, _)| slug)
        })
        .or_else(|| {
            models
                .clone()
                .find(|(_, is_default)| *is_default)
                .map(|(slug, _)| slug)
        })
        .or_else(|| models.map(|(slug, _)| slug).next())
}

fn model_family(model: &str) -> &str {
    model
        .strip_prefix("openai.")
        .filter(|model| model.starts_with("gpt-"))
        .unwrap_or(model)
}
