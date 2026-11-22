use std::sync::Mutex;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct Open {
    pub(in crate::server) thread_id: String,
    url: Option<String>,
    #[serde(default = "default_viewport")]
    viewport: Value,
    profile_id: Option<String>,
    runtime: Option<String>,
    color_scheme: Option<String>,
    zoom_factor: Option<Number>,
    reveal: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct Snapshot {
    pub(in crate::server) thread_id: String,
    pub(in crate::server) tab_id: String,
    pub(in crate::server) nav_status: Navigation,
    can_go_back: bool,
    can_go_forward: bool,
    viewport: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color_scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zoom_factor: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reveal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headless: Option<bool>,
    updated_at: String,
}

impl Snapshot {
    pub(super) fn matches(&self, url: &str, title: &str, back: bool, forward: bool) -> bool {
        matches!(&self.nav_status, Navigation::Success{url: current, title: name} if current == url && name == title)
            && self.can_go_back == back
            && self.can_go_forward == forward
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "_tag")]
pub(in crate::server) enum Navigation {
    Idle,
    Loading {
        url: String,
        title: String,
    },
    Success {
        url: String,
        title: String,
    },
    LoadFailed {
        url: String,
        title: String,
        code: i32,
        description: String,
    },
}

impl Navigation {
    fn loading(url: String) -> Self {
        Self::Loading {
            url,
            title: String::new(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct List {
    sessions: Vec<Snapshot>,
    server_epoch: String,
    revision: u64,
}

pub(super) struct Registry {
    epoch: String,
    state: Mutex<State>,
    events: broadcast::Sender<Value>,
}

#[derive(Default)]
struct State {
    revision: u64,
    next_tab: u64,
    tabs: Vec<Snapshot>,
}

impl Default for Registry {
    fn default() -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            epoch: Uuid::new_v4().to_string(),
            state: Mutex::default(),
            events,
        }
    }
}

impl Registry {
    pub(super) fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub(super) fn open(&self, input: Open, now: &str) -> Result<Snapshot, Value> {
        let url = input.url.as_deref().map(normalize_url).transpose()?;
        let mut state = self.state.lock().expect("preview tab state");
        state.next_tab += 1;
        let suffix = base36(state.next_tab);
        let tab_id = if input.runtime.as_deref() == Some("server") {
            format!("tab_{suffix}_{}", self.epoch)
        } else {
            format!("tab_{suffix}")
        };
        let snapshot = Snapshot {
            thread_id: input.thread_id,
            tab_id,
            nav_status: url.map_or(Navigation::Idle, Navigation::loading),
            can_go_back: false,
            can_go_forward: false,
            viewport: input.viewport,
            profile_id: input.profile_id,
            runtime: input.runtime,
            color_scheme: input.color_scheme,
            zoom_factor: input.zoom_factor,
            reveal: input.reveal,
            headless: None,
            updated_at: now.to_owned(),
        };
        state.tabs.push(snapshot.clone());
        self.publish(&mut state, "opened", &snapshot, now, Some(&snapshot));
        Ok(snapshot)
    }

    pub(super) fn list(&self, thread_id: &str) -> List {
        let state = self.state.lock().expect("preview tab state");
        List {
            sessions: state
                .tabs
                .iter()
                .filter(|tab| tab.thread_id == thread_id)
                .cloned()
                .collect(),
            server_epoch: self.epoch.clone(),
            revision: state.revision,
        }
    }

    pub(super) fn current(&self, thread_id: &str, tab_id: &str) -> Option<Snapshot> {
        let state = self.state.lock().expect("preview tab state");
        state
            .tabs
            .iter()
            .find(|tab| tab.thread_id == thread_id && tab.tab_id == tab_id)
            .cloned()
    }

    pub(super) fn close(&self, thread_id: &str, tab_id: Option<&str>, now: &str) -> Vec<String> {
        let mut state = self.state.lock().expect("preview tab state");
        let mut closed = Vec::new();
        state.tabs.retain(|tab| {
            if tab.thread_id == thread_id && tab_id.is_none_or(|id| id == tab.tab_id) {
                closed.push(tab.clone());
                false
            } else {
                true
            }
        });
        for tab in &closed {
            self.publish(&mut state, "closed", tab, now, None);
        }
        closed.into_iter().map(|tab| tab.tab_id).collect()
    }

    pub(super) fn mark_headless(&self, thread_id: &str, tab_id: &str, now: &str) -> bool {
        self.update(thread_id, tab_id, now, |snapshot| {
            if snapshot.headless == Some(true) {
                return None;
            }
            snapshot.headless = Some(true);
            Some("resized")
        })
    }

    pub(super) fn navigated(
        &self,
        thread_id: &str,
        tab_id: &str,
        navigation: Navigation,
        back: bool,
        forward: bool,
        now: &str,
    ) -> bool {
        self.update(thread_id, tab_id, now, |snapshot| {
            let kind = if matches!(navigation, Navigation::LoadFailed { .. }) {
                "failed"
            } else {
                "navigated"
            };
            snapshot.nav_status = navigation;
            snapshot.can_go_back = back;
            snapshot.can_go_forward = forward;
            Some(kind)
        })
    }

    fn update(
        &self,
        thread_id: &str,
        tab_id: &str,
        now: &str,
        update: impl FnOnce(&mut Snapshot) -> Option<&'static str>,
    ) -> bool {
        let mut state = self.state.lock().expect("preview tab state");
        let Some(snapshot) = state
            .tabs
            .iter_mut()
            .find(|tab| tab.thread_id == thread_id && tab.tab_id == tab_id)
        else {
            return false;
        };
        let Some(kind) = update(snapshot) else {
            return true;
        };
        snapshot.updated_at = now.to_owned();
        let snapshot = snapshot.clone();
        self.publish(&mut state, kind, &snapshot, now, Some(&snapshot));
        true
    }

    fn publish(
        &self,
        state: &mut State,
        kind: &str,
        tab: &Snapshot,
        now: &str,
        snapshot: Option<&Snapshot>,
    ) {
        state.revision += 1;
        let mut event = json!({
            "type": kind, "threadId": tab.thread_id, "tabId": tab.tab_id,
            "createdAt": now, "serverEpoch": self.epoch, "revision": state.revision,
        });
        if kind == "failed" {
            if let Navigation::LoadFailed {
                url,
                title,
                code,
                description,
            } = &tab.nav_status
            {
                event["url"] = json!(url);
                event["title"] = json!(title);
                event["code"] = json!(code);
                event["description"] = json!(description);
            }
        } else if let Some(snapshot) = snapshot {
            event["snapshot"] = json!(snapshot);
        }
        let _ = self.events.send(event);
    }
}

pub(super) fn normalize_url(raw: &str) -> Result<String, Value> {
    let trimmed = raw.trim();
    let error = |reason: &str, protocol: Option<&str>, cause: &str| {
        let mut failure = json!({"_tag":"PreviewInvalidUrlError","inputLength":raw.encode_utf16().count(),"reason":reason,"cause":cause});
        if let Some(protocol) = protocol {
            failure["protocol"] = json!(format!("{protocol}:"));
        }
        failure
    };
    if trimmed.is_empty() {
        return Err(error("empty", None, "Preview URL is empty."));
    }
    let explicit = trimmed.split_once(':').and_then(|(protocol, _)| {
        let mut chars = protocol.bytes();
        (chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-')))
        .then(|| protocol.to_ascii_lowercase())
    });
    if explicit.as_deref().is_some_and(|value| {
        matches!(
            value,
            "about"
                | "blob"
                | "chrome"
                | "data"
                | "file"
                | "ftp"
                | "javascript"
                | "mailto"
                | "sms"
                | "ssh"
                | "tel"
                | "view-source"
                | "ws"
                | "wss"
        )
    }) {
        return Err(error(
            "unsupported-protocol",
            explicit.as_deref(),
            "Preview URL protocol is unsupported.",
        ));
    }
    let has_scheme = explicit.is_some()
        && trimmed
            .split_once(':')
            .is_some_and(|(_, tail)| tail.starts_with("//"));
    let candidate = if has_scheme {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = Url::parse(&candidate).map_err(|cause| {
        error(
            "parse",
            Some(
                explicit
                    .as_deref()
                    .filter(|_| has_scheme)
                    .unwrap_or("https"),
            ),
            &cause.to_string(),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(error(
            "unsupported-protocol",
            Some(parsed.scheme()),
            "Preview URL protocol is unsupported.",
        ));
    }
    let loopback = parsed.host_str().is_some_and(|name| {
        name.trim_end_matches('.').eq_ignore_ascii_case("localhost")
            || name
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback() || address.is_unspecified())
    });
    if !has_scheme && loopback {
        return Url::parse(&format!("http://{trimmed}"))
            .map(Into::into)
            .map_err(|cause| error("parse", Some("http"), &cause.to_string()));
    }
    Ok(parsed.into())
}

fn default_viewport() -> Value {
    json!({"_tag":"fill"})
}

fn base36(mut index: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut output = Vec::new();
    while index > 0 {
        output.push(DIGITS[(index % 36) as usize]);
        index /= 36;
    }
    output.into_iter().rev().map(char::from).collect()
}
