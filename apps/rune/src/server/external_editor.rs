use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex_lite::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{process::Command, time::timeout};

use super::filesystem_metadata::expand_home;

#[derive(Clone, Copy)]
enum Style {
    Goto,
    Direct,
    LineColumn,
}

struct Editor {
    id: &'static str,
    commands: &'static [&'static str],
    arguments: &'static [&'static str],
    style: Style,
}

impl Editor {
    const fn new(id: &'static str, commands: &'static [&'static str], style: Style) -> Self {
        Self {
            id,
            commands,
            arguments: &[],
            style,
        }
    }

    const fn with_args(mut self, arguments: &'static [&'static str]) -> Self {
        self.arguments = arguments;
        self
    }

    fn executable(&self, path: &OsStr) -> Option<PathBuf> {
        self.commands
            .iter()
            .find_map(|command| executable(path, command))
    }
}

const EDITORS: &[Editor] = &[
    Editor::new("cursor", &["cursor"], Style::Goto).with_args(&["--classic"]),
    Editor::new("trae", &["trae"], Style::Goto),
    Editor::new("kiro", &["kiro"], Style::Goto).with_args(&["ide"]),
    Editor::new("vscode", &["code"], Style::Goto),
    Editor::new("vscode-insiders", &["code-insiders"], Style::Goto),
    Editor::new("vscodium", &["codium"], Style::Goto),
    Editor::new("zed", &["zed", "zeditor"], Style::Direct),
    Editor::new("antigravity", &["antigravity-ide", "agy-ide"], Style::Goto),
    Editor::new("idea", &["idea"], Style::LineColumn),
    Editor::new("aqua", &["aqua"], Style::LineColumn),
    Editor::new("clion", &["clion"], Style::LineColumn),
    Editor::new("datagrip", &["datagrip"], Style::LineColumn),
    Editor::new("goland", &["goland"], Style::LineColumn),
    Editor::new("phpstorm", &["phpstorm"], Style::LineColumn),
    Editor::new("pycharm", &["pycharm"], Style::LineColumn),
    Editor::new("rider", &["rider"], Style::LineColumn),
    Editor::new("rubymine", &["rubymine"], Style::LineColumn),
    Editor::new("rustrover", &["rustrover"], Style::LineColumn),
    Editor::new("webstorm", &["webstorm"], Style::LineColumn),
];

const FILE_MANAGER: &str = "file-manager";
static POSITIONED_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.*?):([0-9]+)(?::([0-9]+))?$").expect("valid terminal position pattern")
});

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Input {
    cwd: String,
    editor: String,
    project_root: Option<String>,
    position: Option<Position>,
    reveal: Option<bool>,
}

#[derive(Deserialize)]
struct Position {
    line: Option<u64>,
    column: Option<u64>,
}

impl Input {
    pub(super) fn decode(payload: Value) -> Result<Self, String> {
        let mut input: Self = serde_json::from_value(payload).map_err(|error| error.to_string())?;
        input.cwd = input.cwd.trim().to_owned();
        if input.cwd.is_empty() {
            return Err("Expected a nonempty path\n  at [\"cwd\"]".into());
        }
        input.project_root = input.project_root.map(|root| root.trim().to_owned());
        if input.project_root.as_deref() == Some("") {
            return Err("Expected a nonempty path\n  at [\"projectRoot\"]".into());
        }
        if let Some(position) = &input.position
            && (position.line == Some(0) || position.column == Some(0))
        {
            return Err("Expected a positive line and column\n  at [\"position\"]".into());
        }
        Ok(input)
    }
}

pub(super) async fn available_editors() -> Vec<&'static str> {
    let path = env::var_os("PATH").unwrap_or_default();
    let mut choices: Vec<_> = EDITORS
        .iter()
        .filter(|editor| editor.executable(&path).is_some())
        .map(|editor| editor.id)
        .collect();
    if file_manager(&path).await.is_some() {
        choices.push(FILE_MANAGER);
    }
    choices
}

pub(super) async fn open(input: Input) -> Result<(), Value> {
    let path = env::var_os("PATH").unwrap_or_default();
    let target = expand_home(&input.cwd).to_string_lossy().into_owned();
    let project = input.project_root.as_deref().map(expand_home);
    let root = project.as_deref().and_then(Path::to_str);
    let (command, args) = if input.editor == FILE_MANAGER {
        let command = file_manager(&path)
            .await
            .ok_or_else(|| unsupported(&input.editor))?;
        if input.reveal == Some(true) {
            return Err(unsupported(&input.editor));
        }
        (command, vec![target])
    } else {
        let editor = EDITORS
            .iter()
            .find(|item| item.id == input.editor)
            .ok_or_else(
                || json!({"_tag":"ExternalLauncherUnknownEditorError","editor":input.editor}),
            )?;
        let command = editor.executable(&path).ok_or_else(|| {
            json!({"_tag":"ExternalLauncherCommandNotFoundError","editor":editor.id,"command":editor.commands[0]})
        })?;
        let args = editor_args(editor, &target, root, input.position.as_ref());
        (command, args)
    };
    let mut child = Command::new(&command);
    child
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    child.process_group(0);
    let mut child = child.spawn().map_err(|error| {
        json!({"_tag":"ExternalLauncherEditorSpawnError","editor":input.editor,
            "target":input.cwd,"command":command,"args":args,"cause":error.to_string()})
    })?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

fn editor_args(
    editor: &Editor,
    target: &str,
    root: Option<&str>,
    position: Option<&Position>,
) -> Vec<String> {
    let mut args: Vec<String> = editor
        .arguments
        .iter()
        .map(|value| (*value).to_owned())
        .collect();
    let positioned = match position {
        Some(position) => position.line.map(|line| {
            (
                target,
                line.to_string(),
                position.column.map(|column| column.to_string()),
            )
        }),
        None => POSITIONED_PATH.captures(target).and_then(|parts| {
            let file = parts.get(1)?.as_str();
            (!file.is_empty()).then(|| {
                (
                    file,
                    parts[2].to_owned(),
                    parts.get(3).map(|column| column.as_str().to_owned()),
                )
            })
        }),
    };
    if matches!(editor.style, Style::Goto) && Path::new(target).is_dir() {
        args.extend(["--new-window".into(), target.into()]);
    } else if let Some((file, line, column)) = positioned {
        match editor.style {
            Style::Direct | Style::Goto => {
                let positioned_target = match column {
                    Some(column) => format!("{file}:{line}:{column}"),
                    None => format!("{file}:{line}"),
                };
                if position.is_some()
                    && matches!(editor.style, Style::Goto)
                    && target.contains(':')
                    && target.split(':').any(|part| part.parse::<u64>().is_ok())
                {
                    args.push(target.to_owned());
                } else {
                    if matches!(editor.style, Style::Goto) {
                        args.push("--goto".into());
                    }
                    args.push(positioned_target);
                }
            }
            Style::LineColumn => {
                args.extend(root.map(str::to_owned));
                args.extend(["--line".into(), line]);
                if let Some(column) = column {
                    let zero_indexed = column.parse::<u64>().unwrap_or_default().saturating_sub(1);
                    args.extend(["--column".into(), zero_indexed.to_string()]);
                }
                args.push(file.to_owned());
            }
        }
    } else {
        args.push(target.to_owned());
    }
    args
}

fn executable(path: &OsStr, command: &str) -> Option<PathBuf> {
    let command = if cfg!(windows) {
        format!("{command}.exe")
    } else {
        command.to_owned()
    };
    env::split_paths(path)
        .map(|directory| directory.join(&command))
        .find(|candidate| {
            let Ok(metadata) = candidate.metadata() else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            true
        })
}

async fn file_manager(path: &OsStr) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return executable(path, "open");
    }
    if cfg!(windows) {
        return executable(path, "explorer");
    }
    if !["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    {
        return None;
    }
    let opener = executable(path, "xdg-open")?;
    let mime = executable(path, "xdg-mime")?;
    let output = timeout(
        Duration::from_secs(2),
        Command::new(mime)
            .args(["query", "default", "inode/directory"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    (output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty())
        .then_some(opener)
}

fn unsupported(editor: &str) -> Value {
    json!({"_tag":"ExternalLauncherUnsupportedEditorError","editor":editor})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_raw_position_for_goto_and_forwards_workspace_to_line_column_editors() {
        let file = "/workspace/docs/demo.txt:03:05";
        let root = Some("/workspace");
        let args = |id| {
            editor_args(
                EDITORS.iter().find(|editor| editor.id == id).unwrap(),
                file,
                root,
                None,
            )
        };
        assert_eq!(args("vscode"), ["--goto", file]);
        assert_eq!(args("cursor"), ["--classic", "--goto", file]);
        assert_eq!(args("zed"), [file]);
        assert_eq!(
            args("idea"),
            [
                "/workspace",
                "--line",
                "03",
                "--column",
                "4",
                "/workspace/docs/demo.txt"
            ]
        );
    }
}
