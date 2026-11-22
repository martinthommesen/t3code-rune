use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::server::parse_settings_json;

mod appearance;
mod browser;
pub(crate) use browser::BrowserLinkTarget;
mod chromium;
mod editor;
mod project_order;
mod reasoning;
mod terminal;

pub(crate) use appearance::AppearanceMode;
use appearance::AppearancePreference;
pub(crate) use project_order::{LEGACY_KEY, ProjectOrder};
pub(crate) use terminal::{SavedTerminalGroup, SavedTerminalLayout, SplitDirection};

const FILE_NAME: &str = "client-settings.json";
const TERMINAL_LAYOUTS: &str = "terminalLayouts";
const TERMINAL_HEIGHTS: &str = "terminalDrawerHeights";
const TERMINAL_CONFIRMATION: &str = "confirmTerminalClose";
const THREAD_DELETE_CONFIRMATION: &str = "confirmThreadDelete";
const THREAD_ARCHIVE_CONFIRMATION: &str = "confirmThreadArchive";
const THREAD_DETAILS_CLOSED: &str = "threadDetailsInlineClosed";
const LAST_PROJECT_ACTIONS: &str = "lastInvokedScriptByProject";
const ONBOARDING_COMPLETED: &str = "onboardingCompletedAt";
const SIDEBAR_OPEN: &str = "chat_thread_sidebar_open";
const MAX_SYMLINK_HOPS: usize = 40;

type TerminalHeights = BTreeMap<String, BTreeMap<String, u32>>;

type ClosedThreadDetails = BTreeMap<String, BTreeSet<String>>;

pub(crate) struct ClientSettings {
    path: Option<PathBuf>,
    confirm_terminal_close: bool,
    confirm_thread_delete: bool,
    confirm_thread_archive: bool,
    terminal_heights: TerminalHeights,
    terminal_layouts: terminal::Layouts,
    closed_thread_details: ClosedThreadDetails,
    last_project_actions: BTreeMap<String, String>,
    appearance: AppearancePreference,
    browser_link_target: BrowserLinkTarget,
    reasoning: reasoning::Preferences,
    project_order: project_order::Preferences,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            path: None,
            confirm_terminal_close: true,
            confirm_thread_delete: true,
            confirm_thread_archive: false,
            terminal_heights: TerminalHeights::new(),
            terminal_layouts: terminal::Layouts::new(),
            closed_thread_details: ClosedThreadDetails::new(),
            last_project_actions: BTreeMap::new(),
            appearance: AppearancePreference::default(),
            browser_link_target: BrowserLinkTarget::default(),
            reasoning: reasoning::Preferences::default(),
            project_order: project_order::Preferences::default(),
        }
    }
}

impl ClientSettings {
    pub(crate) fn at_state_dir(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let mut document = read_document(&path).ok();
        let browser_link_target = browser::read(document.as_mut());
        let confirm_terminal_close = document
            .as_ref()
            .and_then(|document| confirmation(document, TERMINAL_CONFIRMATION, true).ok())
            .unwrap_or(true);
        let confirm_thread_delete = document
            .as_ref()
            .and_then(|document| confirmation(document, THREAD_DELETE_CONFIRMATION, true).ok())
            .unwrap_or(true);
        let confirm_thread_archive = document
            .as_ref()
            .and_then(|document| confirmation(document, THREAD_ARCHIVE_CONFIRMATION, false).ok())
            .unwrap_or(false);
        let terminal_heights = document
            .as_ref()
            .and_then(|document| terminal_heights(document).ok())
            .unwrap_or_default();
        let terminal_layouts = document
            .as_ref()
            .and_then(|document| terminal_layouts(document).ok())
            .unwrap_or_default();
        let closed_thread_details = document
            .as_ref()
            .and_then(|document| closed_thread_details(document).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            confirm_terminal_close,
            confirm_thread_delete,
            confirm_thread_archive,
            terminal_heights,
            terminal_layouts,
            closed_thread_details,
            last_project_actions: document
                .as_ref()
                .and_then(|document| last_project_actions(document).ok())
                .unwrap_or_default(),
            appearance: AppearancePreference::at_state_dir(state_dir),
            browser_link_target,
            project_order: document
                .as_ref()
                .map(project_order::Preferences::read)
                .unwrap_or_default(),
            reasoning: document
                .as_ref()
                .and_then(|value| reasoning::Preferences::read(value).ok())
                .unwrap_or_default(),
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

    pub(crate) fn last_project_action(&self, project_id: &str) -> Option<&str> {
        self.last_project_actions
            .get(project_id)
            .map(String::as_str)
    }

    pub(crate) fn remember_project_action(
        &mut self,
        project_id: &str,
        action_id: &str,
    ) -> io::Result<()> {
        if self.last_project_action(project_id) == Some(action_id) {
            return Ok(());
        }
        let update = |actions: &mut BTreeMap<String, String>| {
            actions.insert(project_id.to_owned(), action_id.to_owned());
        };
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let mut actions = last_project_actions(&document)?;
            update(&mut actions);
            preference_object(&mut document)?.insert(
                LAST_PROJECT_ACTIONS.to_owned(),
                serde_json::to_value(&actions)?,
            );
            write_document(&target, &document)?;
            self.last_project_actions = actions;
        } else {
            update(&mut self.last_project_actions);
        }
        Ok(())
    }

    pub(crate) fn terminal_layout(
        &self,
        environment: &str,
        thread: &str,
    ) -> Option<&SavedTerminalLayout> {
        self.terminal_layouts.get(environment)?.get(thread)
    }

    pub(crate) fn set_terminal_layout(
        &mut self,
        environment: &str,
        thread: &str,
        layout: SavedTerminalLayout,
    ) -> io::Result<()> {
        if self.terminal_layout(environment, thread) == Some(&layout) {
            return Ok(());
        }
        let update = |layouts: &mut terminal::Layouts| {
            layouts
                .entry(environment.to_owned())
                .or_default()
                .insert(thread.to_owned(), layout);
        };
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let mut layouts = terminal_layouts(&document)?;
            update(&mut layouts);
            preference_object(&mut document)?
                .insert(TERMINAL_LAYOUTS.into(), serde_json::to_value(&layouts)?);
            write_document(&target, &document)?;
            self.terminal_layouts = layouts;
        } else {
            update(&mut self.terminal_layouts);
        }
        Ok(())
    }

    pub(crate) fn terminal_height(&self, environment: &str, thread: &str) -> Option<u32> {
        self.terminal_heights.get(environment)?.get(thread).copied()
    }

    pub(crate) fn set_terminal_height(
        &mut self,
        environment: &str,
        thread: &str,
        height: u32,
    ) -> io::Result<()> {
        let add_height = |heights: &mut TerminalHeights| {
            heights
                .entry(environment.to_owned())
                .or_default()
                .insert(thread.to_owned(), height);
        };
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let mut heights = terminal_heights(&document)?;
            add_height(&mut heights);
            preference_object(&mut document)?
                .insert(TERMINAL_HEIGHTS.into(), serde_json::to_value(&heights)?);
            write_document(&target, &document)?;
            self.terminal_heights = heights;
        } else {
            add_height(&mut self.terminal_heights);
        }
        Ok(())
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

    pub(crate) fn confirm_thread_archive(&self) -> bool {
        self.confirm_thread_archive
    }

    pub(crate) fn set_confirm_thread_archive(&mut self, confirm: bool) -> io::Result<()> {
        self.write_confirmation(THREAD_ARCHIVE_CONFIRMATION, confirm)?;
        self.confirm_thread_archive = confirm;
        Ok(())
    }

    pub(crate) fn confirm_thread_delete(&self) -> bool {
        self.confirm_thread_delete
    }

    pub(crate) fn set_confirm_thread_delete(&mut self, confirm: bool) -> io::Result<()> {
        self.write_confirmation(THREAD_DELETE_CONFIRMATION, confirm)?;
        self.confirm_thread_delete = confirm;
        Ok(())
    }

    pub(crate) fn set_confirm_terminal_close(&mut self, confirm: bool) -> io::Result<()> {
        self.write_confirmation(TERMINAL_CONFIRMATION, confirm)?;
        self.confirm_terminal_close = confirm;
        Ok(())
    }

    fn write_confirmation(&self, key: &str, confirm: bool) -> io::Result<()> {
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            confirmation(&document, key, false)?;
            preference_object(&mut document)?.insert(key.to_owned(), Value::Bool(confirm));
            write_document(&target, &document)?;
        }
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

fn terminal_layouts(document: &Map<String, Value>) -> io::Result<terminal::Layouts> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    preferences
        .get(TERMINAL_LAYOUTS)
        .map(|layout| serde_json::from_value(layout.clone()).map_err(io::Error::from))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn terminal_heights(document: &Map<String, Value>) -> io::Result<TerminalHeights> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    preferences
        .get(TERMINAL_HEIGHTS)
        .map(|height| serde_json::from_value(height.clone()).map_err(io::Error::from))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn confirmation(document: &Map<String, Value>, key: &str, default: bool) -> io::Result<bool> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    match preferences.get(key) {
        Some(value) => value.as_bool().ok_or_else(invalid_document),
        None => Ok(default),
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

fn last_project_actions(document: &Map<String, Value>) -> io::Result<BTreeMap<String, String>> {
    let preferences = match document.get("settings") {
        Some(value) => value.as_object().ok_or_else(invalid_document)?,
        None => document,
    };
    preferences
        .get(LAST_PROJECT_ACTIONS)
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
