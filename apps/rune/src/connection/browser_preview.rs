mod media;

use std::collections::HashMap;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{
    Connection, Environment, Event, EventSender, REQUEST_TIMEOUT, RpcExit, Session, SessionStop,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTab {
    pub(crate) thread_id: String,
    pub(crate) tab_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserSnapshot {
    #[serde(flatten)]
    pub(crate) key: BrowserTab,
    pub(crate) nav_status: Navigation,
    pub(crate) can_go_back: bool,
    pub(crate) can_go_forward: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "_tag")]
pub(crate) enum Navigation {
    Idle,
    Loading {
        url: String,
    },
    Success {
        url: String,
        title: String,
    },
    LoadFailed {
        url: String,
        title: String,
        description: String,
    },
}

impl Navigation {
    pub(crate) fn url(&self) -> &str {
        match self {
            Self::Idle => "",
            Self::Loading { url } | Self::Success { url, .. } | Self::LoadFailed { url, .. } => url,
        }
    }

    pub(crate) fn title(&self) -> &str {
        match self {
            Self::Success { title, .. } | Self::LoadFailed { title, .. } if !title.is_empty() => {
                title
            }
            _ => self.url(),
        }
    }
}

pub(crate) struct BrowserFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: std::sync::Arc<[u8]>,
    pub(crate) accepted: oneshot::Sender<()>,
}

pub(super) enum Command {
    Open {
        thread: String,
        url: String,
    },
    Close(BrowserTab),
    Watch {
        tab: BrowserTab,
        width: u32,
        height: u32,
    },
    Hide,
    Input {
        tab: BrowserTab,
        payload: Value,
    },
}

pub(super) struct Client {
    available: bool,
    base: Url,
    token: String,
    stream: Option<String>,
    pending: HashMap<String, (String, String, Instant)>,
    media: Option<media::Viewer>,
}

impl Client {
    pub(super) fn new(environment: &Environment) -> Self {
        let allowed = environment.token.scope.as_deref().is_some_and(|scope| {
            scope
                .split_whitespace()
                .any(|name| name == "preview:operate")
        });
        Self {
            available: environment.descriptor.capabilities.server_browser && allowed,
            base: environment.url.clone(),
            token: environment.token.access_token.clone(),
            stream: None,
            pending: HashMap::new(),
            media: None,
        }
    }

    pub(super) fn is_stream(&self, id: &str) -> bool {
        self.stream.as_deref() == Some(id)
    }

    pub(super) fn is_opening(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    pub(super) async fn next(&mut self) -> Option<Event> {
        let Some(viewer) = &mut self.media else {
            return std::future::pending().await;
        };
        let result = viewer.next().await;
        if result.is_none() {
            self.media = None;
        }
        result
    }
}

impl Connection {
    pub(crate) fn open_browser_preview(&self, thread: String, url: String) -> bool {
        self.browser_command(Command::Open { thread, url })
    }

    pub(crate) fn close_browser_preview(&self, tab: BrowserTab) {
        self.browser_command(Command::Close(tab));
    }

    pub(crate) fn watch_browser_preview(&self, tab: BrowserTab, width: u32, height: u32) {
        self.browser_command(Command::Watch { tab, width, height });
    }

    pub(crate) fn hide_browser_preview(&self) {
        self.browser_command(Command::Hide);
    }

    pub(crate) fn browser_input(&self, tab: BrowserTab, payload: Value) {
        self.browser_command(Command::Input { tab, payload });
    }

    fn browser_command(&self, command: Command) -> bool {
        self.commands
            .send(super::Command::BrowserPreview(command))
            .is_ok()
    }
}

impl Session {
    pub(super) async fn browser_preview_command(
        &mut self,
        command: Command,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        match command {
            Command::Open { thread, url } => {
                if !self.browser_preview.available {
                    events.send(Event::BrowserPreviewOpened {
                        thread,
                        url,
                        snapshot: None,
                    });
                    return Ok(());
                }
                if self.browser_preview.stream.is_none() {
                    let stream = self.allocate_id();
                    self.send_request(&stream, "subscribePreviewEvents", json!({}))
                        .await?;
                    self.browser_preview.stream = Some(stream);
                }
                let id = self.allocate_id();
                let request = json!({"threadId":thread,"url":url,"viewport":{"_tag":"fill"},"profileId":"default","runtime":"server","colorScheme":"system","zoomFactor":1});
                self.browser_preview
                    .pending
                    .insert(id.clone(), (thread, url, Instant::now()));
                self.send_request(&id, "preview.open", request).await?;
            }
            Command::Close(tab) => {
                if self
                    .browser_preview
                    .media
                    .as_ref()
                    .is_some_and(|viewer| viewer.tab() == &tab)
                {
                    self.browser_preview.media = None;
                }
                let id = self.allocate_id();
                self.send_request(&id, "preview.close", json!(tab)).await?;
            }
            Command::Watch { tab, width, height } if self.browser_preview.available => {
                if let Some(viewer) = self
                    .browser_preview
                    .media
                    .as_ref()
                    .filter(|viewer| viewer.tab() == &tab)
                {
                    viewer.send(json!({"type":"resize","width":width,"height":height}));
                } else {
                    self.browser_preview.media = Some(media::Viewer::start(
                        self.browser_preview.base.clone(),
                        &self.browser_preview.token,
                        tab,
                        width,
                        height,
                    ));
                }
            }
            Command::Hide => self.browser_preview.media = None,
            Command::Input { tab, payload } => {
                if let Some(viewer) = self
                    .browser_preview
                    .media
                    .as_ref()
                    .filter(|viewer| viewer.tab() == &tab)
                {
                    viewer.send(payload);
                }
            }
            Command::Watch { .. } => (),
        }
        Ok(())
    }

    pub(super) fn apply_preview_events(
        &mut self,
        values: Vec<Value>,
        events: &EventSender<impl Fn()>,
    ) {
        for event in values {
            match event["type"].as_str() {
                Some("opened" | "navigated" | "resized") => {
                    if let Ok(snapshot) =
                        serde_json::from_value::<BrowserSnapshot>(event["snapshot"].clone())
                    {
                        events.send(Event::BrowserPreviewUpdated(snapshot));
                    }
                }
                Some("closed") => {
                    if let Ok(tab) = serde_json::from_value::<BrowserTab>(event) {
                        events.send(Event::BrowserPreviewClosed(tab));
                    }
                }
                Some("failed") => {
                    if let Ok(tab) = serde_json::from_value::<BrowserTab>(event.clone()) {
                        let (url, title, description) =
                            (&event["url"], &event["title"], &event["description"]);
                        if let (Some(url), Some(title), Some(description)) =
                            (url.as_str(), title.as_str(), description.as_str())
                        {
                            events.send(Event::BrowserPreviewUpdated(BrowserSnapshot {
                                key: tab,
                                nav_status: Navigation::LoadFailed {
                                    url: url.into(),
                                    title: title.into(),
                                    description: description.into(),
                                },
                                can_go_back: false,
                                can_go_forward: false,
                            }));
                        }
                    }
                }
                _ => (),
            }
        }
    }

    pub(super) fn preview_exit(
        &mut self,
        id: &str,
        exit: RpcExit,
        events: &EventSender<impl Fn()>,
    ) {
        if self.browser_preview.is_stream(id) {
            self.browser_preview.stream = None;
            return;
        }
        if let Some((thread, url, _)) = self.browser_preview.pending.remove(id) {
            let snapshot = exit
                .into_result()
                .ok()
                .and_then(|value| serde_json::from_value(value).ok())
                .filter(|snapshot: &BrowserSnapshot| snapshot.key.thread_id == thread);
            events.send(Event::BrowserPreviewOpened {
                thread,
                url,
                snapshot,
            });
        }
    }

    pub(super) async fn expire_preview(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let expired = self
            .browser_preview
            .pending
            .iter()
            .filter(|(_, (_, _, at))| at.elapsed() >= REQUEST_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            if let Some((thread, url, _)) = self.browser_preview.pending.remove(&id) {
                self.interrupt(&id).await?;
                events.send(Event::BrowserPreviewOpened {
                    thread,
                    url,
                    snapshot: None,
                });
            }
        }
        Ok(())
    }
}
