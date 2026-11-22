use std::io;

use serde_json::Value;

use super::{
    ClientSettings, preference_object, read_document, resolve_file_target, write_document,
};

const LAST_EDITOR: &str = "lastEditor";

impl ClientSettings {
    pub(crate) fn preferred_editor(&self) -> Option<String> {
        let path = self.path.as_deref()?;
        let mut document = read_document(path).ok()?;
        preference_object(&mut document)
            .ok()?
            .get(LAST_EDITOR)?
            .as_str()
            .map(str::to_owned)
    }

    pub(crate) fn remember_editor(&self, editor: String) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let target = resolve_file_target(path)?;
        let mut document = read_document(&target)?;
        let preferences = preference_object(&mut document)?;
        if preferences.get(LAST_EDITOR).and_then(Value::as_str) != Some(&editor) {
            preferences.insert(LAST_EDITOR.into(), Value::String(editor));
            write_document(&target, &document)?;
        }
        Ok(())
    }
}
