use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::server::parse_settings_json;

mod appearance;
mod chromium;

pub(crate) use appearance::AppearanceMode;
use appearance::AppearancePreference;

const FILE_NAME: &str = "client-settings.json";
const TERMINAL_CONFIRMATION: &str = "confirmTerminalClose";
const MAX_SYMLINK_HOPS: usize = 40;

pub(crate) struct ClientSettings {
    path: Option<PathBuf>,
    confirm_terminal_close: bool,
    appearance: AppearancePreference,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            path: None,
            confirm_terminal_close: true,
            appearance: AppearancePreference::default(),
        }
    }
}

impl ClientSettings {
    pub(crate) fn at_state_dir(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let confirm_terminal_close = read_document(&path)
            .and_then(|document| terminal_confirmation(&document))
            .unwrap_or(true);
        Self {
            path: Some(path),
            confirm_terminal_close,
            appearance: AppearancePreference::at_state_dir(state_dir),
        }
    }

    pub(crate) fn confirm_terminal_close(&self) -> bool {
        self.confirm_terminal_close
    }

    pub(crate) fn appearance_mode(&self) -> AppearanceMode {
        self.appearance.mode()
    }

    pub(crate) fn set_appearance_mode(&mut self, mode: AppearanceMode) -> io::Result<()> {
        self.appearance.set(mode)
    }

    pub(crate) fn import_electron_appearance(&mut self, electron_profile: &Path) -> io::Result<()> {
        self.appearance.import(electron_profile)
    }

    pub(crate) fn set_confirm_terminal_close(&mut self, confirm: bool) -> io::Result<()> {
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            terminal_confirmation(&document)?;
            preference_object(&mut document)?
                .insert(TERMINAL_CONFIRMATION.to_owned(), Value::Bool(confirm));
            write_document(&target, &document)?;
        }
        self.confirm_terminal_close = confirm;
        Ok(())
    }
}

pub(crate) fn read_document(path: &Path) -> io::Result<Map<String, Value>> {
    match fs::read_to_string(path) {
        Ok(content) => match parse_settings_json(&content) {
            Some(Value::Object(document)) => Ok(document),
            _ => Err(invalid_document()),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error),
    }
}

fn terminal_confirmation(document: &Map<String, Value>) -> io::Result<bool> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    match preferences.get(TERMINAL_CONFIRMATION) {
        Some(value) => value.as_bool().ok_or_else(invalid_document),
        None => Ok(true),
    }
}

fn preference_object(document: &mut Map<String, Value>) -> io::Result<&mut Map<String, Value>> {
    if document.contains_key("settings") {
        document
            .get_mut("settings")
            .and_then(Value::as_object_mut)
            .ok_or_else(invalid_document)
    } else {
        Ok(document)
    }
}

fn invalid_document() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid desktop client settings document.",
    )
}

pub(crate) fn resolve_file_target(path: &Path) -> io::Result<PathBuf> {
    let mut target = std::path::absolute(path)?;
    for _ in 0..=MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&target)?;
                target = if link.is_absolute() {
                    link
                } else {
                    let directory = target.parent().ok_or_else(invalid_document)?;
                    fs::canonicalize(directory)?.join(link)
                };
            }
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(target),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Too many symbolic links in the client settings path.",
    ))
}

pub(crate) fn write_document(path: &Path, document: &Map<String, Value>) -> io::Result<()> {
    let directory = path.parent().ok_or_else(invalid_document)?;
    fs::create_dir_all(directory)?;
    let mut new_file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut new_file, document)?;
    new_file.write_all(b"\n")?;
    new_file.as_file().sync_all()?;
    new_file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests;
