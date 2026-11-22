use std::collections::{BTreeMap, BTreeSet};
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
const THREAD_DETAILS_CLOSED: &str = "threadDetailsInlineClosed";
const ONBOARDING_COMPLETED: &str = "onboardingCompletedAt";
const SIDEBAR_OPEN: &str = "chat_thread_sidebar_open";
const MAX_SYMLINK_HOPS: usize = 40;

type ClosedThreadDetails = BTreeMap<String, BTreeSet<String>>;

pub(crate) struct ClientSettings {
    path: Option<PathBuf>,
    confirm_terminal_close: bool,
    closed_thread_details: ClosedThreadDetails,
    appearance: AppearancePreference,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            path: None,
            confirm_terminal_close: true,
            closed_thread_details: ClosedThreadDetails::new(),
            appearance: AppearancePreference::default(),
        }
    }
}

impl ClientSettings {
    pub(crate) fn at_state_dir(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let document = read_document(&path).ok();
        let confirm_terminal_close = document
            .as_ref()
            .and_then(|document| terminal_confirmation(document).ok())
            .unwrap_or(true);
        let closed_thread_details = document
            .as_ref()
            .and_then(|document| closed_thread_details(document).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            confirm_terminal_close,
            closed_thread_details,
            appearance: AppearancePreference::at_state_dir(state_dir),
        }
    }

    pub(crate) fn sidebar_open(&self) -> bool {
        self.path
            .as_deref()
            .and_then(|path| read_document(path).ok())
            .and_then(|document| {
                let settings = match document.get("settings") {
                    Some(value) => value.as_object()?,
                    None => &document,
                };
                settings.get(SIDEBAR_OPEN).and_then(Value::as_bool)
            })
            .unwrap_or(true)
    }

    pub(crate) fn set_sidebar_open(&self, open: bool) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let target = resolve_file_target(path)?;
        let mut document = read_document(&target)?;
        let preferences = preference_object(&mut document)?;
        if open {
            preferences.shift_remove(SIDEBAR_OPEN);
        } else {
            preferences.insert(SIDEBAR_OPEN.into(), Value::Bool(false));
        }
        write_document(&target, &document)
    }

    pub(crate) fn onboarding_completed(&self) -> io::Result<bool> {
        self.path.as_deref().map_or(Ok(true), |path| {
            onboarding_completion(&read_document(path)?)
        })
    }

    pub(crate) fn complete_onboarding(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let target = resolve_file_target(path)?;
        let mut document = read_document(&target)?;
        if onboarding_completion(&document)? {
            return Ok(());
        }
        let format = time::format_description::parse_borrowed::<2>(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
        )
        .expect("static onboarding timestamp format");
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&format)
            .map_err(io::Error::other)?;
        preference_object(&mut document)?
            .insert(ONBOARDING_COMPLETED.to_owned(), Value::String(timestamp));
        write_document(&target, &document)
    }

    pub(crate) fn confirm_terminal_close(&self) -> bool {
        self.confirm_terminal_close
    }

    pub(crate) fn thread_details_inline_open(&self, environment: &str, thread: &str) -> bool {
        !self
            .closed_thread_details
            .get(environment)
            .is_some_and(|closed| closed.contains(thread))
    }

    pub(crate) fn set_thread_details_inline_open(
        &mut self,
        environment: &str,
        thread: &str,
        open: bool,
    ) -> io::Result<()> {
        let update = |closed: &mut ClosedThreadDetails| {
            if open {
                if let Some(threads) = closed.get_mut(environment) {
                    threads.remove(thread);
                    if threads.is_empty() {
                        closed.remove(environment);
                    }
                }
            } else {
                closed
                    .entry(environment.to_owned())
                    .or_default()
                    .insert(thread.to_owned());
            }
        };

        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let mut closed = closed_thread_details(&document)?;
            update(&mut closed);
            let preferences = preference_object(&mut document)?;
            if closed.is_empty() {
                preferences.remove(THREAD_DETAILS_CLOSED);
            } else {
                preferences.insert(
                    THREAD_DETAILS_CLOSED.to_owned(),
                    serde_json::to_value(&closed)?,
                );
            }
            write_document(&target, &document)?;
            self.closed_thread_details = closed;
        } else {
            update(&mut self.closed_thread_details);
        }
        Ok(())
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

fn onboarding_completion(document: &Map<String, Value>) -> io::Result<bool> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    match preferences.get(ONBOARDING_COMPLETED) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(_)) => Ok(true),
        Some(_) => Err(invalid_document()),
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

fn closed_thread_details(document: &Map<String, Value>) -> io::Result<ClosedThreadDetails> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    preferences
        .get(THREAD_DETAILS_CLOSED)
        .map(|value| serde_json::from_value(value.clone()).map_err(io::Error::from))
        .transpose()
        .map(Option::unwrap_or_default)
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
