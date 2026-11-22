use std::io;

use serde_json::Value;

use super::{
    ClientSettings, preference_object, read_document, resolve_file_target, write_document,
};

const KEY: &str = "browserLinkTarget";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub(crate) enum BrowserLinkTarget {
    #[default]
    System,
    App,
}

impl BrowserLinkTarget {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "Your default browser",
            Self::App => "T3 Code",
        }
    }
}

impl ClientSettings {
    pub(crate) fn browser_link_target(&self) -> BrowserLinkTarget {
        self.browser_link_target
    }

    pub(crate) fn remember_browser_link_target(
        &mut self,
        target: BrowserLinkTarget,
    ) -> io::Result<()> {
        if let Some(path) = &self.path {
            let path = resolve_file_target(path)?;
            let mut document = read_document(&path)?;
            let value = match target {
                BrowserLinkTarget::App => "app",
                BrowserLinkTarget::System => "system",
            };
            let settings = preference_object(&mut document)?;
            settings.insert(KEY.to_owned(), Value::String(value.to_owned()));
            write_document(&path, &document)?;
        }
        self.browser_link_target = target;
        Ok(())
    }
}

pub(super) fn read(document: Option<&mut serde_json::Map<String, Value>>) -> BrowserLinkTarget {
    let value = document
        .and_then(|document| preference_object(document).ok())
        .and_then(|settings| settings.get(KEY))
        .and_then(Value::as_str);
    if value == Some("app") {
        BrowserLinkTarget::App
    } else {
        BrowserLinkTarget::System
    }
}
