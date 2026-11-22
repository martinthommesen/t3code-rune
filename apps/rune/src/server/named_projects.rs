use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::broadcast;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::ProjectionChange;
use super::project_commands::ProjectCommands;

const FOLDER_ATTEMPTS: usize = 100;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
const ICON_BACKGROUNDS: [&str; 15] = [
    "#dc2626", "#ea580c", "#d97706", "#16a34a", "#059669", "#0d9488", "#0891b2", "#0284c7",
    "#2563eb", "#4f46e5", "#7c3aed", "#9333ea", "#c026d3", "#db2777", "#e11d48",
];

#[derive(Deserialize)]
pub(super) struct CreateNamedProject {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NamedProject {
    project_id: String,
    workspace_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum NamedProjectError {
    Name,
    Folder,
    GitMissing,
    Save,
}

impl NamedProjectError {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Name => "Enter a project name between 1 and 200 characters.",
            Self::Folder => "Failed to create the project folder.",
            Self::GitMissing => {
                "Git isn't installed on this machine. Install Git to create a project."
            }
            Self::Save => "Failed to create the project.",
        }
    }
}

pub(super) fn root_for(state_dir: &Path) -> PathBuf {
    let base = if state_dir.file_name() == Some(OsStr::new("userdata")) {
        state_dir.parent().unwrap_or(state_dir)
    } else {
        state_dir
    };
    base.join("projects")
}

pub(super) async fn create(
    projects: ProjectCommands,
    root: PathBuf,
    input: CreateNamedProject,
    changes: broadcast::Sender<ProjectionChange>,
) -> Result<NamedProject, NamedProjectError> {
    let name = trim(&input.name).to_owned();
    if name.is_empty() || name.encode_utf16().count() > 200 {
        return Err(NamedProjectError::Name);
    }
    let folder_name = folder_name(&name);
    let folder = tokio::task::spawn_blocking(move || Folder::claim(&root, &folder_name))
        .await
        .map_err(|_| NamedProjectError::Folder)?
        .map_err(|_| NamedProjectError::Folder)?;
    let commit_error = scaffold(&folder.path, &name).await?;
    tokio::task::spawn_blocking(move || {
        let id = Uuid::new_v4().to_string();
        let workspace_root = folder.path.to_str().ok_or(NamedProjectError::Save)?;
        let outcome = projects.dispatch(json!({
            "type": "project.create",
            "commandId": format!("named-project:{id}"),
            "projectId": id,
            "title": name,
            "workspaceRoot": workspace_root,
            "defaultModelSelection": null,
        }));
        if outcome.is_err() {
            if projects.has_workspace(&folder.path).unwrap_or(true) {
                folder.preserve();
            }
            return Err(NamedProjectError::Save);
        }
        let workspace_root = folder.preserve();
        let _ = changes.send(ProjectionChange::Project);
        Ok(NamedProject {
            project_id: id,
            workspace_root,
            commit_error,
        })
    })
    .await
    .map_err(|_| NamedProjectError::Save)?
}

struct Folder {
    path: PathBuf,
    keep: bool,
}

impl Folder {
    fn preserve(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }

    fn claim(root: &Path, name: &str) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        for attempt in 1..=FOLDER_ATTEMPTS {
            let path = if attempt == 1 {
                root.join(name)
            } else {
                root.join(format!("{name}-{attempt}"))
            };
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, keep: false }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Every project folder name is taken",
        ))
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

async fn scaffold(cwd: &Path, name: &str) -> Result<Option<String>, NamedProjectError> {
    let branch = git(cwd, &["config", "--get", "init.defaultBranch"], GIT_TIMEOUT)
        .await
        .ok()
        .filter(|result| result.status.success())
        .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_owned())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "main".into());
    required_git(cwd, &["init", &format!("--initial-branch={branch}")]).await?;
    let files_path = cwd.to_path_buf();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        fs::write(files_path.join("README.md"), readme(&name))?;
        fs::create_dir(files_path.join("assets"))?;
        fs::write(files_path.join("assets/icon.svg"), icon(&name))
    })
    .await
    .map_err(|_| NamedProjectError::Folder)?
    .map_err(|_| NamedProjectError::Folder)?;
    required_git(
        cwd,
        &[
            "-c",
            "core.safecrlf=false",
            "add",
            "--force",
            "--",
            "README.md",
            "assets/icon.svg",
        ],
    )
    .await?;
    let commit_error = match git(
        cwd,
        &["commit", "--message", "Initial commit"],
        COMMIT_TIMEOUT,
    )
    .await
    {
        Ok(result) if result.status.success() => None,
        Ok(result) => Some(commit_failure(&String::from_utf8_lossy(&result.stderr))),
        Err(error) => Some(error.to_string()),
    };
    Ok(commit_error)
}

async fn required_git(cwd: &Path, args: &[&str]) -> Result<(), NamedProjectError> {
    match git(cwd, args, GIT_TIMEOUT).await {
        Ok(result) if result.status.success() => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(NamedProjectError::GitMissing),
        _ => Err(NamedProjectError::Folder),
    }
}

async fn git(cwd: &Path, args: &[&str], timeout: Duration) -> io::Result<Output> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0");
    for name in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_PREFIX",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(name);
    }
    tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Git timed out."))?
}

fn trim(text: &str) -> &str {
    text.trim_matches(|character| {
        matches!(character,
        '\t'..='\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' |
        '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

pub(super) fn fold_latin_lowercase(name: &str) -> String {
    let mut latin = String::with_capacity(name.len());
    for character in name.chars() {
        match character {
            'Ŀ' | 'ŀ' | 'Ł' | 'ł' => latin.push('l'),
            'ŉ' | 'Ŋ' | 'ŋ' => latin.push('n'),
            'Đ' | 'đ' | 'Ð' | 'ð' => latin.push('d'),
            'ß' | 'ẞ' => latin.push_str("ss"),
            'Ø' | 'ø' => latin.push('o'),
            'Æ' | 'æ' => latin.push_str("ae"),
            'Œ' | 'œ' => latin.push_str("oe"),
            'Þ' | 'þ' => latin.push_str("th"),
            'Ħ' | 'ħ' => latin.push('h'),
            'ı' => latin.push('i'),
            'ĸ' => latin.push('q'),
            'Ŧ' | 'ŧ' => latin.push('t'),
            _ => latin.push(character),
        }
    }
    latin
        .nfkd()
        .filter(|character| !matches!(*character, '\u{0300}'..='\u{036f}'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn folder_name(name: &str) -> String {
    let mut slug = String::new();
    for character in fold_latin_lowercase(name).chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() == 64 {
            break;
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return "project".into();
    }
    if matches!(slug.as_str(), "con" | "prn" | "aux" | "nul")
        || matches!(
            slug.as_bytes(),
            [b'c', b'o', b'm', b'1'..=b'9'] | [b'l', b'p', b't', b'1'..=b'9']
        )
    {
        slug.push_str("-project");
    }
    slug
}

fn readme(name: &str) -> String {
    format!(
        "<img src=\"assets/icon.svg\" width=\"64\" height=\"64\" alt=\"\">\n\n# {name}\n\nCreated in [T3 Code](https://t3.codes).\n"
    )
}

fn icon(name: &str) -> String {
    let mut initials = name
        .split(|character: char| !character.is_alphanumeric())
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect::<String>();
    if initials.is_empty() {
        initials.push(trim(name).chars().next().unwrap_or('?'));
    }
    let size = if initials.encode_utf16().count() > 1 {
        26
    } else {
        32
    };
    let escaped = initials
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let color = name.chars().fold(0_u32, |hash, character| {
        hash.wrapping_mul(31).wrapping_add(character as u32)
    });
    let background = ICON_BACKGROUNDS[color as usize % ICON_BACKGROUNDS.len()];
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\">\n  <rect width=\"64\" height=\"64\" rx=\"14\" fill=\"{background}\"/>\n  <text x=\"32\" y=\"32\" dy=\"0.35em\" text-anchor=\"middle\" font-family=\"ui-sans-serif, system-ui, -apple-system, sans-serif\" font-size=\"{size}\" font-weight=\"600\" fill=\"#ffffff\">{escaped}</text>\n</svg>\n"
    )
}

fn commit_failure(stderr: &str) -> String {
    let folded = stderr.to_ascii_lowercase();
    if [
        "identity unknown",
        "tell me who you are",
        "no name was given",
        "no email was given",
    ]
    .iter()
    .any(|text| folded.contains(text))
    {
        return "Git has no name or email on this machine. Set user.name and user.email, then commit.".into();
    }
    stderr
        .lines()
        .rev()
        .map(trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Git could not make the first commit.")
        .into()
}
