mod error;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, MAIN_SEPARATOR_STR, Path, PathBuf};

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

pub(super) use self::error::BrowseError;
use super::locale::path_collator;

const MAX_PATH_LENGTH: usize = 512;

#[derive(Debug)]
pub(super) struct BrowseInput {
    partial_path: String,
    cwd: Option<Option<String>>,
}

impl BrowseInput {
    fn project_cwd(&self) -> Option<&str> {
        self.cwd.as_ref()?.as_deref()
    }
}

impl<'de> Deserialize<'de> for BrowseInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let input = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("Expected object"))?;
        let partial_path = input
            .get("partialPath")
            .ok_or_else(|| serde::de::Error::custom("Missing key\n  at [\"partialPath\"]"))?;
        let partial_path = partial_path
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("Expected string\n  at [\"partialPath\"]"))?;
        let partial_path = validate_request_path("partialPath", partial_path)?;
        let cwd = match input.get("cwd") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(path)) => Some(Some(validate_request_path("cwd", path)?)),
            _ => {
                return Err(serde::de::Error::custom(
                    "Expected string | null\n  at [\"cwd\"]",
                ));
            }
        };
        Ok(Self { partial_path, cwd })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowseResult {
    #[serde(serialize_with = "serialize_trimmed")]
    parent_path: String,
    #[serde(serialize_with = "serialize_entries")]
    entries: Vec<BrowseEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseEntry {
    #[serde(skip)]
    filename: OsString,
    #[serde(serialize_with = "serialize_trimmed")]
    name: String,
    #[serde(serialize_with = "serialize_trimmed")]
    full_path: String,
}

pub(super) fn browse(input: BrowseInput) -> Result<BrowseResult, BrowseError> {
    let partial = &input.partial_path;
    if !cfg!(windows) && is_windows_absolute(partial) {
        let platform = match env::consts::OS {
            "macos" => "darwin",
            platform => platform,
        };
        return Err(BrowseError::windows_path(&input, platform));
    }
    let project = if is_explicit_relative(partial) {
        Some(
            input
                .project_cwd()
                .ok_or_else(|| BrowseError::project_required(&input))?,
        )
    } else {
        None
    };
    let target = resolve_target(partial, project).map_err(|error| {
        BrowseError::read_directory(&input, project.unwrap_or(partial).to_owned(), error)
    })?;
    let directory_mode = partial.ends_with(['/', '\\']) || partial == "~";
    let (parent, prefix) = if directory_mode {
        (target.as_path(), String::new())
    } else {
        let parent = target.parent().unwrap_or(&target);
        let prefix = target
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        (parent, prefix)
    };
    let parent_path = parent.to_string_lossy().into_owned();
    let mut entries = match read_directories(parent, &prefix, directory_mode) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Vec::new(),
        Err(error) => return Err(BrowseError::read_directory(&input, parent_path, error)),
    };
    let collator = path_collator().ok_or_else(|| {
        BrowseError::read_directory(
            &input,
            parent_path.clone(),
            io::Error::other("could not initialize path collation"),
        )
    })?;
    entries.sort_by(|left, right| {
        collator.compare(&left.name, &right.name).then_with(|| {
            left.filename
                .as_encoded_bytes()
                .cmp(right.filename.as_encoded_bytes())
        })
    });
    Ok(BrowseResult {
        parent_path,
        entries,
    })
}

fn is_windows_absolute(path: &str) -> bool {
    path.starts_with("\\\\")
        || matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()
            && (path.len() == 2 || matches!(path.as_bytes()[2], b'/' | b'\\')))
}

fn is_explicit_relative(path: &str) -> bool {
    matches!(path, "." | "..")
        || ["./", "../", ".\\", "..\\"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

pub(super) fn resolve_target(partial: &str, project: Option<&str>) -> io::Result<PathBuf> {
    match project {
        Some(project) => {
            let base = absolute_path(expand_home(project)?)?;
            absolute_path(base.join(partial))
        }
        None => absolute_path(expand_home(partial)?),
    }
}

fn expand_home(path: &str) -> io::Result<PathBuf> {
    let tail = match path {
        "~" => "",
        path => match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            Some(tail) => tail,
            None => return Ok(PathBuf::from(path)),
        },
    };
    let home_env = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let mut home = env::var_os(home_env)
        .map(PathBuf::from)
        .or_else(env::home_dir)
        .ok_or_else(|| io::Error::other("could not determine the home directory"))?
        .into_os_string();
    if !tail.is_empty() {
        if !home.is_empty() {
            home.push(MAIN_SEPARATOR_STR);
        }
        home.push(tail);
    }
    Ok(PathBuf::from(home))
}

pub(super) fn absolute_path(path: PathBuf) -> io::Result<PathBuf> {
    let source = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &path
    };
    let absolute = std::path::absolute(source)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn read_directories(
    parent: &Path,
    prefix: &str,
    directory_mode: bool,
) -> io::Result<Vec<BrowseEntry>> {
    let show_hidden = directory_mode || prefix.starts_with('.');
    let lower_prefix = prefix.to_lowercase();
    let mut directories = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let filename = entry.file_name();
        let name = filename.to_string_lossy().into_owned();
        if name.to_lowercase().starts_with(&lower_prefix) && (show_hidden || !name.starts_with('.'))
        {
            let full_path = parent.join(&name).to_string_lossy().into_owned();
            directories.push(BrowseEntry {
                filename,
                name,
                full_path,
            });
        }
    }
    Ok(directories)
}

fn validate_request_path<E: serde::de::Error>(field: &str, raw: &str) -> Result<String, E> {
    let path = trim_js(raw);
    if path.is_empty() {
        return Err(E::custom(format!(
            "Expected a non-blank string\n  at [\"{field}\"]"
        )));
    }
    if path.encode_utf16().count() > MAX_PATH_LENGTH {
        return Err(E::custom(format!(
            "Expected a value with a length of at most {MAX_PATH_LENGTH}\n  at [\"{field}\"]"
        )));
    }
    Ok(path.to_owned())
}

fn serialize_entries<S: Serializer>(
    entries: &[BrowseEntry],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut sequence = serializer.serialize_seq(Some(entries.len()))?;
    for (index, entry) in entries.iter().enumerate() {
        if trim_js(&entry.name).is_empty() {
            return Err(serde::ser::Error::custom(format!(
                "Expected a non-blank string\n  at [\"entries\"][{index}][\"name\"]"
            )));
        }
        sequence.serialize_element(entry)?;
    }
    sequence.end()
}

fn serialize_trimmed<S: Serializer>(text: &str, serializer: S) -> Result<S::Ok, S::Error> {
    let trimmed = trim_js(text);
    if trimmed.is_empty() {
        return Err(serde::ser::Error::custom("Expected a non-blank string"));
    }
    serializer.serialize_str(trimmed)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
    })
}

#[cfg(test)]
mod tests;
