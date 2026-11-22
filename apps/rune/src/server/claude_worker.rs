use std::collections::HashMap;
use std::error;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use t3_rune::providers::claude::{
    AssistantBlock, ContextUsage, Launch, Message, PermissionMode, ProcessLedger, Session, Usage,
};

use super::settings::{ClaudeInstance, EnvironmentValue, SettingsError, SettingsFile};
use super::turn_commands::claude::Claim;
use super::turn_commands::title_effect::ClaimedTitle;
use super::turn_commands::{TurnCommandError, TurnCommands};
use super::turn_worker::TurnQueue;

const KEEP_IDLE_FOR: Duration = Duration::from_secs(30 * 60);

#[derive(Debug)]
pub(super) enum Error {
    Settings(SettingsError),
    Storage(TurnCommandError),
    Provider(t3_rune::providers::claude::Error),
    Authentication(u16),
    Unavailable(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settings(error) => error.fmt(formatter),
            Self::Storage(error) => error.fmt(formatter),
            Self::Provider(_) => formatter.write_str("Claude Code could not complete the turn."),
            Self::Authentication(_) => formatter.write_str("Claude could not authenticate. For subscription login, run `claude auth login` on this environment's machine, then start a new thread. For API-key authentication, check this instance's configured credentials."),
            Self::Unavailable(message) => formatter.write_str(message),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Settings(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::Provider(error) => Some(error),
            Self::Unavailable(_) | Self::Authentication(_) => None,
        }
    }
}

impl Error {
    pub(super) fn code(&self) -> Option<&'static str> {
        match self {
            Self::Authentication(401) => Some("api_error_401"),
            Self::Authentication(403) => Some("api_error_403"),
            _ => None,
        }
    }
}

impl From<TurnCommandError> for Error {
    fn from(error: TurnCommandError) -> Self {
        Self::Storage(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(TurnCommandError::from(error))
    }
}

impl From<t3_rune::providers::claude::Error> for Error {
    fn from(error: t3_rune::providers::claude::Error) -> Self {
        Self::Provider(error)
    }
}

#[derive(Default)]
pub(super) struct Runtimes {
    idle: HashMap<String, Idle>,
}

struct Idle {
    process: Option<Process>,
    context_usage: Option<Value>,
    reported_windows: HashMap<String, u64>,
    idle_since: Instant,
}

struct Process {
    key: ProcessKey,
    session: Session,
    usage: ProcessLedger,
}

impl Process {
    async fn apply_title(&mut self, claim: &ClaimedTitle) -> Result<(), Error> {
        if claim.native_claude_id == Some(self.session.session_id())
            && self.key.title != claim.title
            && self.session.rename_session(&claim.title).await?
        {
            self.key.title.clone_from(&claim.title);
        }
        Ok(())
    }

    async fn apply_pending_title(
        &mut self,
        commands: &TurnCommands,
        claim: &Claim,
    ) -> Result<(), Error> {
        if let Some(title) =
            commands.claim_title_effect(Some(&claim.thread_id), &claim.worker_id)?
        {
            let result = self.apply_title(&title).await;
            commands.finish_title_effect(&title, &claim.worker_id)?;
            result?;
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
struct ProcessKey {
    executable: PathBuf,
    workspace: PathBuf,
    environment: HashMap<OsString, OsString>,
    session_id: uuid::Uuid,
    model: String,
    title: String,
}

impl From<&Launch> for ProcessKey {
    fn from(launch: &Launch) -> Self {
        Self {
            executable: launch.executable.clone(),
            workspace: launch.working_directory.clone(),
            environment: launch.environment.clone(),
            session_id: launch.target.id(),
            model: launch.model.clone(),
            title: launch.title.clone().unwrap_or_default(),
        }
    }
}

impl Runtimes {
    pub(super) async fn apply_title(&mut self, claim: &ClaimedTitle) {
        if let Some(idle) = self.idle.get_mut(&claim.thread_id)
            && let Some(process) = &mut idle.process
            && process.apply_title(claim).await.is_err()
        {
            idle.process = None;
        }
    }

    pub(super) async fn execute(
        &mut self,
        commands: &TurnCommands,
        settings: &SettingsFile,
        claim: &Claim,
        queue: &TurnQueue,
        on_commit: impl Fn(i64),
    ) -> Result<i64, Error> {
        let launch = process_config(settings, claim)?;
        let key = ProcessKey::from(&launch);
        let _ = commands
            .capture_baseline_in(&claim.workspace, &claim.checkpoint_scope_id, claim.ordinal)
            .await;
        let previous = self.idle.remove(&claim.thread_id);
        let (process, previous_context, mut reported_windows) = previous.map_or_else(
            || (None, None, HashMap::new()),
            |idle| (idle.process, idle.context_usage, idle.reported_windows),
        );
        let mut process = match process {
            Some(process) if process.key == key => process,
            process => {
                if let Some(process) = process {
                    process.session.shutdown().await?;
                }
                Process {
                    key,
                    session: Session::open(launch).await?,
                    usage: ProcessLedger::default(),
                }
            }
        };
        on_commit(owned(commands.bind_claude_turn(claim, previous_context)?)?);
        process.session.offer_text(&claim.text, None).await?;
        on_commit(owned(commands.record_claude_offer(claim)?)?);
        process.apply_pending_title(commands, claim).await?;

        let mut latest_assistant = None;
        let mut context_usage = None;
        let sequence = loop {
            let message = tokio::select! {
                message = process.session.next_message() => message?,
                () = queue.notified() => {
                    process.apply_pending_title(commands, claim).await?;
                    continue;
                }
            };
            match message {
                Message::Initialized(init) if init.session_id != claim.target.id() => {
                    return Err(Error::Unavailable(
                        "Claude Code opened a different session.",
                    ));
                }
                Message::Assistant(assistant) if assistant.parent_tool_use_id.is_none() => {
                    if assistant.error.is_some() {
                        continue;
                    }
                    let mut text = String::new();
                    for block in assistant.message.content {
                        match block {
                            AssistantBlock::Text { text: part } => text.push_str(&part),
                            AssistantBlock::Thinking { .. } => {}
                            _ => {
                                return Err(Error::Unavailable(
                                    "This Claude Code turn needs an unsupported tool handler.",
                                ));
                            }
                        }
                    }
                    if text.is_empty() {
                        continue;
                    }
                    let native_item_id = assistant.uuid.to_string();
                    let assistant_model = assistant.message.model;
                    let usage = assistant.message.usage;
                    let context = assistant_context(usage, &assistant_model, &reported_windows);
                    context_usage = context.map(provider_context).or(context_usage);
                    let update = commands.record_claude_text(
                        claim,
                        &native_item_id,
                        &text,
                        &assistant_model,
                        context.map(serde_json::to_value).transpose()?,
                    )?;
                    if let Some(sequence) = update {
                        on_commit(sequence);
                    }
                    latest_assistant = Some((native_item_id, assistant_model, usage));
                }
                Message::Result(result) if !result.succeeded() => {
                    return Err(match result.api_error_status {
                        Some(status @ (401 | 403)) => Error::Authentication(status),
                        _ => Error::Unavailable("Claude Code could not complete the turn."),
                    });
                }
                Message::Result(result) => {
                    let Some((id, model, last_usage)) = latest_assistant else {
                        return Err(Error::Unavailable(
                            "Claude Code returned no assistant message.",
                        ));
                    };
                    let mut changed_windows = false;
                    if let Some(models) = &result.model_usage {
                        for (model, windows) in models {
                            if let Some(window) =
                                windows.context_window.filter(|window| *window > 0)
                            {
                                changed_windows |=
                                    reported_windows.insert(model.clone(), window) != Some(window);
                            }
                        }
                    }
                    if changed_windows
                        && let Some(context) =
                            assistant_context(last_usage, &model, &reported_windows)
                    {
                        context_usage = Some(provider_context(context));
                        on_commit(owned(commands.record_claude_context(
                            claim,
                            &model,
                            serde_json::to_value(context)?,
                        )?)?);
                    }
                    let usage = serde_json::to_value(process.usage.settle(&result, &model))?;
                    let sequence =
                        owned(commands.complete_claude_turn(claim, &id, &model, usage)?)?;
                    on_commit(sequence);
                    break sequence;
                }
                Message::RejectedControl { .. } | Message::Assistant(_) => {
                    return Err(Error::Unavailable(
                        "This Claude Code turn needs an unsupported tool handler.",
                    ));
                }
                Message::Other(_) | Message::Initialized(_) => {}
            }
        };
        self.idle.insert(
            claim.thread_id.clone(),
            Idle {
                process: Some(process),
                context_usage,
                reported_windows,
                idle_since: Instant::now(),
            },
        );
        Ok(sequence)
    }

    pub(super) async fn release_idle(&mut self) {
        for idle in self.idle.values_mut() {
            if idle.idle_since.elapsed() >= KEEP_IDLE_FOR
                && let Some(process) = idle.process.take()
            {
                let _ = process.session.shutdown().await;
            }
        }
    }
}

fn assistant_context(
    usage: Option<Usage>,
    model: &str,
    windows: &HashMap<String, u64>,
) -> Option<ContextUsage> {
    let window = windows
        .get(model)
        .copied()
        .or((model == "claude-sonnet-4-6").then_some(200_000));
    ContextUsage::from_assistant(usage?, window)
}

fn provider_context(context: ContextUsage) -> Value {
    json!({"usedTokens":context.used_tokens,"maxTokens":context.max_tokens})
}

fn owned(committed: Option<i64>) -> Result<i64, Error> {
    committed.ok_or(Error::Unavailable(
        "The Claude Code run no longer owns its start.",
    ))
}

fn process_config(settings: &SettingsFile, claim: &Claim) -> Result<Launch, Error> {
    let snapshot = settings.read().map_err(Error::Settings)?;
    let config = claim.config(&snapshot)?;
    let (_, environment) = configured_process(config, &claim.workspace)?;
    Ok(Launch {
        executable: environment.executable,
        working_directory: claim.workspace.clone(),
        environment: environment.variables,
        target: claim.target,
        model: claim.model.clone(),
        title: Some(claim.title.clone()),
        append_system_prompt: RUNTIME_INSTRUCTIONS.to_owned(),
        permission: PermissionMode::FullAccess,
    })
}

pub(super) struct ProcessSettings {
    pub(super) executable: PathBuf,
    pub(super) variables: HashMap<OsString, OsString>,
}

pub(super) fn configured_process(
    config: ClaudeInstance,
    cwd: &std::path::Path,
) -> Result<(Option<PathBuf>, ProcessSettings), Error> {
    let mut environment = HashMap::new();
    for entry in config.environment {
        let EnvironmentValue::Literal(value) = entry.value else {
            return Err(Error::Unavailable(
                "Claude Code requires a saved or referenced environment variable.",
            ));
        };
        environment.insert(OsString::from(entry.name), OsString::from(value));
    }
    let configured_home = if !config.home_path.is_empty() {
        let home = expand_home(&config.home_path)?;
        let absolute = std::path::absolute(home).map_err(|_| {
            Error::Unavailable("The Claude Code configuration directory is invalid.")
        })?;
        environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            absolute.clone().into_os_string(),
        );
        Some(absolute)
    } else {
        None
    };
    let home = configured_home.or_else(|| {
        environment
            .get(std::ffi::OsStr::new("CLAUDE_CONFIG_DIR"))
            .cloned()
            .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR"))
            .filter(|value| !value.is_empty())
            .map(|path| cwd.join(path))
    });
    Ok((
        home,
        ProcessSettings {
            executable: expand_home(&config.binary_path)?,
            variables: environment,
        },
    ))
}

fn expand_home(value: &str) -> Result<PathBuf, Error> {
    let tail = match value {
        "~" => "",
        value if value.starts_with("~/") => &value[2..],
        value => return Ok(PathBuf::from(value)),
    };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok_or(
        Error::Unavailable("The current account's home directory could not be resolved."),
    )?;
    Ok(PathBuf::from(home).join(tail))
}

const RUNTIME_INSTRUCTIONS: &str = include_str!("claude_runtime_instructions.txt");

#[cfg(all(test, unix))]
mod tests;
