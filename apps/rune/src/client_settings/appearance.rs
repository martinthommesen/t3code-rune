use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{chromium, read_document, resolve_file_target, write_document};

const FILE_NAME: &str = "theme-preferences.json";
pub(super) const MODE_KEY: &str = "t3code:theme-appearance-mode";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum AppearanceMode {
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    pub(crate) fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "system" => Self::System,
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Default)]
pub(super) struct AppearancePreference {
    path: Option<PathBuf>,
    mode: AppearanceMode,
}

impl AppearancePreference {
    pub(super) fn at_state_dir(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let mode = read_document(&path)
            .ok()
            .and_then(|document| {
                document
                    .get(MODE_KEY)
                    .and_then(Value::as_str)
                    .and_then(AppearanceMode::parse)
            })
            .unwrap_or_default();
        Self {
            path: Some(path),
            mode,
        }
    }

    pub(super) fn mode(&self) -> AppearanceMode {
        self.mode
    }

    pub(super) fn import(&mut self, profile: &Path) -> io::Result<()> {
        if let Some(path) = &self.path
            && let Some(mode) = read_document(path)?
                .get(MODE_KEY)
                .and_then(Value::as_str)
                .and_then(AppearanceMode::parse)
        {
            self.mode = mode;
            return Ok(());
        }
        if let Some(mode) = chromium::read_appearance(profile)? {
            self.set(mode)?;
        }
        Ok(())
    }

    pub(super) fn set(&mut self, mode: AppearanceMode) -> io::Result<()> {
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            document.insert(MODE_KEY.to_owned(), Value::String(mode.as_str().to_owned()));
            write_document(&target, &document)?;
        }
        self.mode = mode;
        Ok(())
    }
}
