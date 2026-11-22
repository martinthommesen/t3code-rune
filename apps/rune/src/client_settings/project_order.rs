use std::path::Path;
use std::{collections::HashSet, io};

use serde_json::{Map, Value};

use super::{
    ClientSettings, chromium, preference_object, read_document, resolve_file_target, write_document,
};
use crate::workspace_path::comparison_path;

const MODE: &str = "sidebarProjectSortOrder";
const ORDER: &str = "projectOrder";
const LEGACY_ORDER: &str = "projectOrderCwds";
pub(super) const UI_STATE_KEY: &str = "t3code:ui-state:v1";
pub(crate) const LEGACY_KEY: &str = "legacy-project-cwd:";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProjectOrder {
    #[default]
    Activity,
    Created,
    Manual,
}

impl ProjectOrder {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Activity => "updated_at",
            Self::Created => "created_at",
            Self::Manual => "manual",
        }
    }

    fn parse(value: &Value) -> Self {
        match value.as_str() {
            Some("created_at") => Self::Created,
            Some("manual") => Self::Manual,
            _ => Self::default(),
        }
    }
}

#[derive(Default)]
pub(super) struct Preferences {
    mode: ProjectOrder,
    order: Option<Vec<String>>,
}

impl Preferences {
    pub(super) fn read(document: &Map<String, Value>) -> Self {
        let preferences = match document.get("settings") {
            Some(Value::Object(settings)) => settings,
            Some(_) => return Self::default(),
            None => document,
        };
        Self {
            mode: preferences
                .get(MODE)
                .map(ProjectOrder::parse)
                .unwrap_or_default(),
            order: order(preferences),
        }
    }
}

fn order(values: &Map<String, Value>) -> Option<Vec<String>> {
    let (keys, legacy) = values
        .get(ORDER)
        .map(|keys| (keys, false))
        .or_else(|| values.get(LEGACY_ORDER).map(|keys| (keys, true)))?;
    let mut seen = HashSet::new();
    Some(
        keys.as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|key| !key.is_empty() && seen.insert(*key))
            .map(|key| {
                if legacy {
                    format!("{LEGACY_KEY}{}", comparison_path(key))
                } else {
                    key.to_owned()
                }
            })
            .collect(),
    )
}

impl ClientSettings {
    pub(crate) fn project_order(&self) -> ProjectOrder {
        self.project_order.mode
    }

    pub(crate) fn manual_project_order(&self) -> &[String] {
        self.project_order.order.as_deref().unwrap_or_default()
    }

    pub(crate) fn set_project_order(&mut self, mode: ProjectOrder) -> io::Result<()> {
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let preferences = preference_object(&mut document)?;
            preferences.insert(MODE.to_owned(), Value::String(mode.as_str().to_owned()));
            write_document(&target, &document)?;
        }
        self.project_order.mode = mode;
        Ok(())
    }

    pub(crate) fn import_electron_project_order(&mut self, profile: &Path) -> io::Result<()> {
        if self.project_order.order.is_some() {
            return Ok(());
        }
        let stored = match chromium::read_project_order(profile) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        };
        let Some(keys) = stored
            .as_deref()
            .and_then(|value| serde_json::from_str::<Map<String, Value>>(value).ok())
            .as_ref()
            .and_then(order)
            .filter(|keys| !keys.is_empty())
        else {
            return Ok(());
        };
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let preferences = preference_object(&mut document)?;
            if let Some(existing) = order(preferences) {
                self.project_order.order = Some(existing);
                return Ok(());
            }
            preferences.insert(ORDER.to_owned(), serde_json::to_value(&keys)?);
            write_document(&target, &document)?;
        }
        self.project_order.order = Some(keys);
        Ok(())
    }
}
