use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use t3_rune::providers::CodexLaunch;

use super::super::settings::{CodexInstance, EnvironmentValue, SettingsFile};
use super::super::turn_commands::start_effect::ClaimedTurnStart;
use super::WorkerError;
use super::streaming::StreamingMode;

const LAUNCH_OVERRIDE: &str = "T3CODE_CODEX_LAUNCH_ARGS";
const CODEX_HOME: &str = "CODEX_HOME";

pub(super) struct Launch {
    pub(super) process: CodexLaunch,
    pub(super) streaming: StreamingMode,
}

impl Launch {
    pub(super) fn resolve(
        settings: &SettingsFile,
        claim: &ClaimedTurnStart,
    ) -> Result<Self, WorkerError> {
        let snapshot = settings.read().map_err(WorkerError::Settings)?;
        let instance = snapshot
            .codex_instance(Some(&claim.project_id), "codex")
            .map_err(WorkerError::ProviderSettings)?;
        if instance
            .custom_models
            .iter()
            .any(|model| model == &claim.turn.model || model["slug"] == claim.turn.model)
        {
            return Err(WorkerError::Unavailable(
                "This Codex model requires custom model configuration.",
            ));
        }
        let process = process_config(instance, claim.thread.cwd.clone())?;
        let public = snapshot.for_client();
        let streaming_mode =
            public["projectSettingsOverrides"][&claim.project_id]["responseStreamingMode"]
                .as_str()
                .or_else(|| public["responseStreamingMode"].as_str());
        let streaming = if streaming_mode == Some("turn") {
            StreamingMode::Turn
        } else {
            StreamingMode::Paragraph
        };
        Ok(Self { process, streaming })
    }
}

pub(in crate::server) fn process_config(
    instance: CodexInstance,
    working_directory: PathBuf,
) -> Result<CodexLaunch, WorkerError> {
    if !instance.shadow_home_path.is_empty() {
        return Err(WorkerError::Unavailable(
            "Codex shadow homes are unavailable for this worker.",
        ));
    }
    let mut environment = HashMap::new();
    for variable in instance.environment {
        let EnvironmentValue::Literal(value) = variable.value else {
            return Err(WorkerError::Unavailable(
                "The Codex instance requires a saved or referenced environment variable.",
            ));
        };
        environment.insert(OsString::from(variable.name), OsString::from(value));
    }
    if !instance.home_path.is_empty() {
        let path = expand_home(&instance.home_path)?;
        environment.insert(CODEX_HOME.into(), path.into_os_string());
    }
    let override_args = environment
        .get(OsStr::new(LAUNCH_OVERRIDE))
        .cloned()
        .or_else(|| std::env::var_os(LAUNCH_OVERRIDE));
    let override_args = override_args.as_ref().and_then(|value| value.to_str());
    let arguments = tokenize(
        override_args
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&instance.launch_args),
    );
    Ok(CodexLaunch {
        executable: expand_home(&instance.binary_path)?,
        working_directory,
        arguments,
        environment,
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

fn expand_home(value: &str) -> Result<PathBuf, WorkerError> {
    let suffix = match value {
        "~" => "",
        path if path.starts_with("~/") => &path[2..],
        path => return Ok(PathBuf::from(path)),
    };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok_or(
        WorkerError::Unavailable("The current account's home directory could not be resolved."),
    )?;
    Ok(PathBuf::from(home).join(suffix))
}

fn tokenize(value: &str) -> Vec<OsString> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut quoted = false;
    let mut input = value.chars().peekable();
    while let Some(character) = input.next() {
        match quote {
            Some(delimiter) if character == delimiter => {
                quote = None;
                quoted = true;
            }
            Some('"')
                if character == '\\'
                    && input
                        .peek()
                        .is_some_and(|next| matches!(next, '"' | '\\' | '$' | '`')) =>
            {
                current.push(input.next().expect("a following character"));
            }
            Some(_) => current.push(character),
            None if matches!(character, '\'' | '"') => {
                quote = Some(character);
                quoted = true;
            }
            None if character.is_ascii_whitespace() || character == '\u{000b}' => {
                if !current.is_empty() || quoted {
                    tokens.push(OsString::from(std::mem::take(&mut current)));
                    quoted = false;
                }
            }
            None if character == '\\'
                && input
                    .peek()
                    .is_some_and(|next| next.is_whitespace() || matches!(next, '\'' | '"')) =>
            {
                current.push(input.next().expect("a following character"));
            }
            None => current.push(character),
        }
    }
    if !current.is_empty() || quoted {
        tokens.push(OsString::from(current));
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn tokenizes_launch_arguments_without_shell_expansion() {
        assert_eq!(
            tokenize(r#"--config 'name=a b' "" --config "url=a\$b" --config path=a\ b"#),
            [
                "--config", "name=a b", "", "--config", "url=a$b", "--config", "path=a b"
            ],
        );
    }
}
