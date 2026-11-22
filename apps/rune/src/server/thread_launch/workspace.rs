use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use uuid::Uuid;

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_GIT_OUTPUT: u64 = 16 * 1024;

pub(super) fn is_scratch_project(
    database: &Connection,
    project_root: &Path,
    scratch_root: &Path,
) -> rusqlite::Result<bool> {
    let target = scratch_root.canonicalize().ok();
    if let Some(target) = target.filter(|target| target != scratch_root) {
        let exists = database.query_row("SELECT EXISTS(SELECT 1 FROM projection_projects WHERE workspace_root=?1 AND deleted_at IS NULL)",
            [target.to_str()], |row| row.get::<_,bool>(0))?;
        if exists {
            return Ok(project_root == target);
        }
    }
    Ok(project_root == scratch_root)
}

pub(super) async fn current_branch(cwd: &Path) -> Option<String> {
    if git_read(cwd, &["rev-parse", "--is-inside-work-tree"])
        .await?
        .trim()
        != "true"
    {
        return None;
    }
    let output = git_read(cwd, &["symbolic-ref", "--quiet", "HEAD"]).await?;
    let reference = super::input::trim_js(&output);
    let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference);
    (!branch.is_empty()).then(|| branch.to_owned())
}

async fn git_read(cwd: &Path, arguments: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .args(["-c", "safe.bareRepository=explicit"])
        .args(arguments)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(MAX_GIT_OUTPUT + 1);
    let mut bytes = Vec::new();
    let (status, size) = tokio::time::timeout(GIT_TIMEOUT, async {
        tokio::try_join!(child.wait(), stdout.read_to_end(&mut bytes))
    })
    .await
    .ok()?
    .ok()?;
    if !status.success() || size as u64 > MAX_GIT_OUTPUT {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) fn claim_scratch_folder(
    root: &Path,
    thread_id: &str,
    text: &str,
) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let date = OffsetDateTime::now_utc().date().to_string();
    let words = words_in_folder(text);
    let id: String = thread_id
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let short = &id[id.len().saturating_sub(8)..];
    let short = [date.as_str(), words.as_str(), short]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let complete = [date.as_str(), words.as_str(), id.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let mut attempt = 1;
    loop {
        let candidate = match attempt {
            1 => root.join(&short),
            2 => root.join(&complete),
            _ => root.join(format!("{complete}-{}", &Uuid::new_v4().to_string()[..8])),
        };
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

fn words_in_folder(text: &str) -> String {
    let words = super::super::named_projects::fold_latin_lowercase(text);
    let mut slug = words
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join("-");
    slug.truncate(48);
    slug.trim_end_matches('-').to_owned()
}

pub(super) fn check_setup(
    database: &Connection,
    settings_path: Option<&Path>,
    project_id: &str,
    scripts_supported: bool,
) -> Result<(), String> {
    if configured_setup(database, settings_path, project_id)?.is_some() && !scripts_supported {
        return Err(
            "Worktree launches with automatic setup scripts require the setup-script runner."
                .into(),
        );
    }
    Ok(())
}

pub(super) fn configured_setup(
    database: &Connection,
    settings_path: Option<&Path>,
    project_id: &str,
) -> Result<Option<super::setup_script::Script>, String> {
    let (project_scripts, tracked_folders) = database
        .query_row(
            "SELECT scripts_json, COALESCE(json_array_length(tracked_folders_json), 0)
         FROM projection_projects WHERE project_id=?1 AND deleted_at IS NULL",
            [project_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .map_err(|_| "Project setup scripts could not be read.")?;
    if tracked_folders > 0 {
        return Err(
            "A worktree launch with tracked folders requires task-folder preparation.".into(),
        );
    }
    let raw = read_settings(settings_path)?;
    let defaults = &raw["defaultProjectScripts"];
    let modern = &raw["projectSettingsOverrides"][project_id];
    let legacy = &raw["projectScriptOverrides"];
    let projected;
    let scripts = if let Some(project_override) = modern.get("defaultProjectScripts") {
        project_override
    } else if raw["projectSettingsFolded"] == true {
        defaults
    } else if let Some(project_override) = legacy.get(project_id) {
        if project_override.is_null() {
            defaults
        } else {
            project_override
        }
    } else {
        projected = serde_json::from_str::<Value>(&project_scripts)
            .map_err(|_| "Project setup scripts could not be read.")?;
        if projected
            .as_array()
            .is_some_and(|scripts| !scripts.is_empty())
        {
            &projected
        } else {
            defaults
        }
    };
    scripts
        .as_array()
        .and_then(|scripts| {
            scripts
                .iter()
                .find(|script| script["runOnWorktreeCreate"] == true)
        })
        .map(|script| {
            serde_json::from_value(script.clone())
                .map_err(|_| "Project setup script was not valid.".into())
        })
        .transpose()
}

pub(super) fn read_settings(path: Option<&Path>) -> Result<Value, &'static str> {
    let Some(path) = path else {
        return Ok(Value::Null);
    };
    match fs::read_to_string(path) {
        Ok(contents) => super::super::parse_settings_json(&contents)
            .ok_or("Worktree setup settings could not be read."),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Null),
        Err(_) => Err("Worktree setup settings could not be read."),
    }
}

pub(super) fn configured_worktrees_root(
    settings_path: Option<&Path>,
    default: &Path,
) -> Result<PathBuf, &'static str> {
    let settings = read_settings(settings_path)?;
    let setting = settings["worktreesDirectory"]
        .as_str()
        .map(super::input::trim_js)
        .unwrap_or_default();
    let path = if setting.is_empty() {
        default.to_owned()
    } else if setting == "~"
        || setting.starts_with("~/")
        || (cfg!(windows) && setting.starts_with("~\\"))
    {
        let home = std::env::home_dir().ok_or("The home directory is unavailable.")?;
        home.join(setting.get(2..).unwrap_or_default())
    } else {
        PathBuf::from(setting)
    };
    if !path.is_absolute() {
        return Err("The configured worktree folder must be an absolute folder on this machine.");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if component == std::path::Component::ParentDir {
            normalized.pop();
        } else {
            normalized.push(component.as_os_str());
        }
    }
    if normalized.parent().is_none() {
        return Err("The configured worktree folder must not be a filesystem root.");
    }
    Ok(normalized)
}
