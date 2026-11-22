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
struct Model {
    model: String,
    #[serde(default)]
    is_default: bool,
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
    let models = models
        .into_iter()
        .filter(|model| !model.model.trim().is_empty() && !hidden.contains(&model.model))
        .collect::<Vec<_>>();
    let selected = PREFERRED_MODELS
        .iter()
        .find_map(|preferred| {
            models
                .iter()
                .find(|model| model_family(&model.model) == *preferred)
        })
        .or_else(|| models.iter().find(|model| model.is_default))
        .or_else(|| models.first());
    Ok(selected.map_or_else(|| PREFERRED_MODELS[0].into(), |model| model.model.clone()))
}

async fn read_pages(server: &CodexAppServer) -> Result<Vec<Model>, &'static str> {
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

fn model_family(model: &str) -> &str {
    model
        .strip_prefix("openai.")
        .filter(|model| model.starts_with("gpt-"))
        .unwrap_or(model)
}
