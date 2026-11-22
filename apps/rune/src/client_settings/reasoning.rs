use std::collections::BTreeMap;
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use t3_rune::providers::codex::ReasoningEffort;

use super::{
    ClientSettings, invalid_document, preference_object, read_document, resolve_file_target,
    write_document,
};

const KEY: &str = "codexReasoningEffort";

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Preferences {
    sticky: Option<ReasoningEffort>,
    drafts: BTreeMap<String, BTreeMap<String, ReasoningEffort>>,
}

impl Preferences {
    pub(super) fn read(document: &Map<String, Value>) -> io::Result<Self> {
        let preferences = match document.get("settings") {
            Some(value) => value.as_object().ok_or_else(invalid_document)?,
            None => document,
        };
        preferences
            .get(KEY)
            .map(|value| Self::deserialize(value).map_err(io::Error::from))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn set(&mut self, environment: &str, project: &str, effort: Option<ReasoningEffort>) {
        if let Some(effort) = effort {
            self.sticky = Some(effort);
            self.drafts
                .entry(environment.to_owned())
                .or_default()
                .insert(project.to_owned(), effort);
        } else if let Some(drafts) = self.drafts.get_mut(environment) {
            drafts.remove(project);
            if drafts.is_empty() {
                self.drafts.remove(environment);
            }
        }
    }
}

impl ClientSettings {
    pub(crate) fn sticky_codex_reasoning(&self) -> Option<ReasoningEffort> {
        self.reasoning.sticky
    }

    pub(crate) fn draft_codex_reasoning(
        &self,
        environment: &str,
        project: &str,
    ) -> Option<ReasoningEffort> {
        self.reasoning
            .drafts
            .get(environment)?
            .get(project)
            .copied()
    }

    pub(crate) fn remember_codex_reasoning(
        &mut self,
        environment: &str,
        project: &str,
        effort: Option<ReasoningEffort>,
    ) -> io::Result<()> {
        if self.draft_codex_reasoning(environment, project) == effort
            && (effort.is_none() || self.reasoning.sticky == effort)
        {
            return Ok(());
        }
        if let Some(path) = &self.path {
            let target = resolve_file_target(path)?;
            let mut document = read_document(&target)?;
            let mut reasoning = Preferences::read(&document)?;
            reasoning.set(environment, project, effort);
            preference_object(&mut document)?.insert(KEY.into(), serde_json::to_value(&reasoning)?);
            write_document(&target, &document)?;
            self.reasoning = reasoning;
        } else {
            self.reasoning.set(environment, project, effort);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    #[test]
    fn restores_a_draft_effort_without_replacing_other_client_settings() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("client-settings.json");
        fs::write(
            &file,
            json!({"settings":{"theme":"dark"},"version":12}).to_string(),
        )
        .unwrap();
        let mut settings = ClientSettings::at_state_dir(directory.path());
        settings
            .remember_codex_reasoning("env", "project", Some(ReasoningEffort::High))
            .unwrap();
        let mut restored = ClientSettings::at_state_dir(directory.path());
        assert_eq!(
            restored.draft_codex_reasoning("env", "project"),
            Some(ReasoningEffort::High)
        );
        assert_eq!(restored.draft_codex_reasoning("other", "project"), None);
        restored
            .remember_codex_reasoning("env", "project", None)
            .unwrap();
        let reloaded = ClientSettings::at_state_dir(directory.path());
        assert_eq!(reloaded.draft_codex_reasoning("env", "project"), None);
        assert_eq!(
            reloaded.sticky_codex_reasoning(),
            Some(ReasoningEffort::High)
        );
        let stored: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(stored["settings"]["theme"], "dark");
        assert_eq!(stored["version"], 12);
    }
}
