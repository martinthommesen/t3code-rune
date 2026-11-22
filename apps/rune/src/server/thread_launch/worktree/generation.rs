use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::server::settings::SettingsFile;
use crate::server::text_generation::structured_codex;
use crate::server::turn_worker::launch::process_config;

use super::super::LaunchInput;
use super::{naming::Naming, scoped_setting};

pub(in crate::server::thread_launch) enum ResultName {
    Generated { branch: String, exact: bool },
    Failed(String),
    Cancelled,
}

pub(in crate::server::thread_launch) async fn name(
    input: &LaunchInput,
    checkout: &Path,
    settings_path: Option<&Path>,
    message: &str,
    cancelled: &mut watch::Receiver<bool>,
) -> ResultName {
    let run = generate(input, checkout, settings_path, message);
    tokio::select! {
        biased;
        _ = cancelled.wait_for(|flag| *flag) => ResultName::Cancelled,
        result = run => match result {
            Ok((branch, exact)) => ResultName::Generated { branch, exact },
            Err(detail) => ResultName::Failed(format!("Text generation failed in generateBranchName: {detail}")),
        }
    }
}

async fn generate(
    input: &LaunchInput,
    checkout: &Path,
    settings_path: Option<&Path>,
    message: &str,
) -> Result<(String, bool), String> {
    let settings_path = settings_path.ok_or("Worktree branch naming settings are unavailable.")?;
    let raw = super::super::workspace::read_settings(Some(settings_path))?;
    let project = &input.project_id.0;
    let naming = Naming::from_settings(&raw, project);
    let settings = SettingsFile::new(settings_path.to_owned())
        .read()
        .map_err(|error| error.to_string())?;
    let public = settings.for_client();
    let writer = scoped_setting(&public, project, "sourceControlWriterModelSelection");
    let fallback = scoped_setting(&public, project, "textGenerationModelSelection");
    let selected = [writer, fallback]
        .into_iter()
        .find_map(|selection| {
            let instance_id = selection["instanceId"].as_str()?;
            let model = selection["model"].as_str()?;
            let instance = settings.codex_instance(Some(project), instance_id).ok()?;
            Some((selection, model, instance))
        })
        .ok_or("No enabled Codex text-generation model is available.")?;
    let (selection, model, instance) = selected;
    let launch =
        process_config(instance, checkout.to_owned()).map_err(|error| error.to_string())?;
    let prompt = naming.prompt(message);
    let schema = json!({
        "type":"object", "properties":{"branch":{"type":"string"}},
        "required":["branch"], "additionalProperties":false,
    });
    let result = structured_codex::run(
        &launch,
        structured_codex::Request {
            prompt: &prompt,
            schema: &schema,
            model,
            reasoning_effort: option(selection, "reasoningEffort").unwrap_or("low"),
            service_tier: option(selection, "serviceTier"),
        },
        Duration::from_secs(20),
    )
    .await
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            "Timed out after 20 seconds.".to_owned()
        } else {
            error.to_string()
        }
    })?;
    let branch = result["branch"]
        .as_str()
        .ok_or("Codex returned invalid structured output.")?;
    Ok((naming.format(branch), naming.exact()))
}

fn option<'a>(selection: &'a Value, id: &str) -> Option<&'a str> {
    selection["options"].as_array()?.iter().find_map(|option| {
        (option["id"] == id)
            .then(|| option["value"].as_str())
            .flatten()
    })
}
