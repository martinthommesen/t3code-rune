use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use t3_rune::providers::CodexLaunch;

const MCP_LIST_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const DISABLED_FEATURES: [&str; 3] = ["apps", "connectors", "plugins"];
const NO_PLUGINS: [&str; 6] = [
    "--config",
    "features.connectors=false",
    "--config",
    "features.plugins=false",
    "--config",
    "features.apps=false",
];

pub(in crate::server) struct Request<'a> {
    pub(in crate::server) prompt: &'a str,
    pub(in crate::server) schema: &'a Value,
    pub(in crate::server) model: &'a str,
    pub(in crate::server) reasoning_effort: &'a str,
    pub(in crate::server) service_tier: Option<&'a str>,
}

pub(in crate::server) async fn run(
    launch: &CodexLaunch,
    request: Request<'_>,
    deadline: Duration,
) -> io::Result<Value> {
    timeout(deadline, execute(launch, request))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Codex CLI request timed out."))?
}

async fn execute(launch: &CodexLaunch, request: Request<'_>) -> io::Result<Value> {
    let directory = tempfile::Builder::new().prefix("t3code-codex-").tempdir()?;
    let schema_path = directory.path().join("schema.json");
    let output_path = directory.path().join("last-message.json");
    std::fs::write(&schema_path, serde_json::to_vec(request.schema)?)?;
    std::fs::File::create(&output_path)?;
    let servers = enabled_mcp_servers(launch).await;
    let mut command = command(launch);
    command.arg("exec");
    let servers_off = mcp_disable_table(&servers);
    if let Some(table) = &servers_off {
        command.args(["--config", table]);
    }
    let launch_args = filter_launch_args(&launch.arguments, true);
    let resets_servers = launch_args.iter().any(|arg| {
        arg.match_indices("mcp_servers").any(|(offset, name)| {
            let word = arg[..offset]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_');
            !word && arg[offset + name.len()..].trim_start().starts_with('=')
        })
    });
    command.args(launch_args).args(NO_PLUGINS);
    if resets_servers && let Some(table) = &servers_off {
        command.args(["--config", table]);
    }
    for server in servers.iter().filter(|name| !name.contains(['.', '='])) {
        command.args(["--config", &format!("mcp_servers.{server}.enabled=false")]);
    }
    command.args([
        "--ephemeral",
        "--skip-git-repo-check",
        "-s",
        "read-only",
        "--model",
        request.model,
        "--config",
        &format!("model_reasoning_effort={}", quote(request.reasoning_effort)),
        "--config",
        "features.codex_hooks=false",
        "--config",
        "features.hooks=false",
    ]);
    if let Some(tier) = request.service_tier {
        command.args(["--config", &format!("service_tier={}", quote(tier))]);
    }
    let mut child = command
        .arg("--output-schema")
        .arg(&schema_path)
        .arg("--output-last-message")
        .arg(&output_path)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("Codex stdin is piped");
    let send_prompt = async move {
        stdin.write_all(request.prompt.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<_, io::Error>(())
    };
    let ((), status) = tokio::try_join!(send_prompt, child.wait())?;
    if !status.success() {
        return Err(io::Error::other(
            "Codex CLI could not generate structured output.",
        ));
    }
    read_result(&output_path)
}

fn command(launch: &CodexLaunch) -> Command {
    let mut command = Command::new(&launch.executable);
    command
        .current_dir(&launch.working_directory)
        .env("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "T3 Code")
        .envs(&launch.environment)
        .kill_on_drop(true);
    command
}

fn read_result(path: &Path) -> io::Result<Value> {
    if std::fs::metadata(path)?.len() > OUTPUT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex returned too much structured output.",
        ));
    }
    let file = std::fs::File::open(path)?;
    use std::io::Read as _;
    let mut bytes = Vec::new();
    file.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex returned too much structured output.",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Deserialize)]
struct McpServer {
    name: String,
    enabled: bool,
}

async fn enabled_mcp_servers(launch: &CodexLaunch) -> Vec<String> {
    timeout(MCP_LIST_TIMEOUT, async {
        let mut child = command(launch)
            .args(filter_launch_args(&launch.arguments, false))
            .args(NO_PLUGINS)
            .args(["mcp", "list", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?.take(OUTPUT_LIMIT + 1);
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.ok()?;
        if bytes.len() as u64 > OUTPUT_LIMIT || !child.wait().await.ok()?.success() {
            return None;
        }
        let servers: Vec<McpServer> = serde_json::from_slice(&bytes).ok()?;
        Some(
            servers
                .into_iter()
                .filter(|server| server.enabled)
                .map(|server| server.name)
                .collect(),
        )
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

fn filter_launch_args(arguments: &[OsString], exec: bool) -> Vec<String> {
    let mut input = arguments.iter().filter_map(|arg| arg.to_str()).peekable();
    let mut output = Vec::new();
    while let Some(raw) = input.next() {
        let normalized = inline_short_argument(raw);
        let (name, inline) = normalized
            .split_once('=')
            .map_or((normalized.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        if exec && inline.is_none() && matches!(name, "--strict-config" | "--oss") {
            output.push(normalized);
            continue;
        }
        let value_flag = matches!(
            name,
            "--config" | "-c" | "--enable" | "--disable" | "--profile" | "-p"
        ) || (exec && name == "--local-provider");
        if !value_flag {
            continue;
        }
        let value = inline.or_else(|| input.next_if(|value| !value.starts_with('-')));
        let Some(value) = value else { continue };
        let disabled = DISABLED_FEATURES.contains(&value)
            || (exec && matches!(value, "hooks" | "codex_hooks"));
        if name == "--enable" && disabled {
            continue;
        }
        if inline.is_some() {
            output.push(normalized);
        } else {
            let value = value.to_owned();
            output.extend([normalized, value]);
        }
    }
    output
}

fn inline_short_argument(raw: &str) -> String {
    for prefix in ["-c", "-p"] {
        if let Some(suffix) = raw
            .strip_prefix(prefix)
            .filter(|value| !value.is_empty() && !value.starts_with('='))
        {
            return format!("{prefix}={suffix}");
        }
    }
    raw.to_owned()
}

fn mcp_disable_table(servers: &[String]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let values = servers
        .iter()
        .map(|name| format!("{}={{enabled=false}}", quote(name)))
        .collect::<Vec<_>>();
    Some(format!("mcp_servers={{{}}}", values.join(",")))
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("encoding strings as JSON cannot fail")
}
