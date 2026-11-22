use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;

use super::{Error, launch, process::Process};

pub struct ProbeLaunch {
    pub executable: PathBuf,
    pub working_directory: PathBuf,
    pub environment: HashMap<OsString, OsString>,
    pub managed_mcp: bool,
}

#[derive(Debug, Deserialize)]
pub struct Probe {
    pub account: Option<ProbeAccount>,
    #[serde(default)]
    pub models: Vec<ProbeModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeAccount {
    pub email: Option<String>,
    pub subscription_type: Option<String>,
    pub token_source: Option<String>,
    pub api_key_source: Option<String>,
    pub api_provider: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeModel {
    pub value: String,
    pub display_name: String,
}

impl ProbeLaunch {
    pub async fn version(&self) -> Result<String, Error> {
        let mut command =
            launch::command(&self.executable, &self.working_directory, &self.environment);
        let mut child = command
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(Error::Spawn)?;
        let stdout = child.stdout.take().ok_or(Error::InvalidFrame)?;
        let mut output = String::new();
        stdout
            .take(4096)
            .read_to_string(&mut output)
            .await
            .map_err(Error::Io)?;
        let status = child.wait().await.map_err(Error::Io)?;
        if !status.success() {
            return Err(Error::Exited(status.code()));
        }
        output
            .split_whitespace()
            .find(|word| is_version(word))
            .map(str::to_owned)
            .ok_or(Error::InvalidFrame)
    }

    pub async fn inspect(&self) -> Result<Probe, Error> {
        let mut command =
            launch::command(&self.executable, &self.working_directory, &self.environment);
        command.args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--setting-sources=user,project,local",
        ]);
        if !self.managed_mcp {
            command.arg("--strict-mcp-config");
        }
        command
            .args([
                "--no-session-persistence",
                "--settings",
                r#"{"disableAllHooks":true}"#,
            ])
            .env_remove("FORCE_CODE_TERMINAL")
            .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
            .env("CLAUDE_CODE_AUTO_CONNECT_IDE", "0")
            .env("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL", "1");
        let mut process = Process::spawn(command)?;
        let result = process
            .initialize(
                json!({"subtype":"initialize","systemPrompt":[""]}),
                &mut Default::default(),
            )
            .await?;
        process.shutdown().await?;
        serde_json::from_value(result).map_err(|_| Error::InvalidFrame)
    }
}

fn is_version(word: &str) -> bool {
    let core = word.split(['-', '+']).next().unwrap_or_default();
    let mut numbers = core.split('.');
    (0..3).all(|_| {
        numbers
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }) && numbers.next().is_none()
}
