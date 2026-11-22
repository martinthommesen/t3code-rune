use std::{collections::HashMap, time::Duration};

use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Connection, Event, EventSender, REQUEST_TIMEOUT, RpcExit, Session, SessionStop};

const METADATA_TTL: Duration = Duration::from_secs(60);
const METADATA_LIMIT: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalFileLink {
    pub(crate) attachment: u64,
    pub(crate) path: String,
    pub(crate) target: String,
    pub(crate) project_root: String,
}

pub(super) enum Request {
    Check {
        link: TerminalFileLink,
        id: Uuid,
    },
    Open {
        link: TerminalFileLink,
        preferred: Option<String>,
    },
}

impl Request {
    fn link(&self) -> &TerminalFileLink {
        match self {
            Self::Check { link, .. } | Self::Open { link, .. } => link,
        }
    }
}

#[derive(Default)]
pub(super) struct TerminalLinks {
    pub(super) can_operate: bool,
    pub(super) editors: Vec<String>,
    pending: HashMap<String, Pending>,
    metadata: HashMap<String, (bool, Instant)>,
}

struct Pending {
    began: Instant,
    request: Request,
    editor: Option<String>,
    waiting: Vec<Request>,
}

impl TerminalLinks {
    pub(super) fn new(can_operate: bool) -> Self {
        Self {
            can_operate,
            ..Self::default()
        }
    }

    pub(super) fn matches(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    fn cached(&self, path: &str) -> Option<bool> {
        let (available, recorded) = self.metadata.get(path)?;
        (recorded.elapsed() < METADATA_TTL).then_some(*available)
    }

    fn remember(&mut self, path: String, available: bool) {
        self.metadata.insert(path, (available, Instant::now()));
        if self.metadata.len() > METADATA_LIMIT
            && let Some((oldest, _)) = self.metadata.iter().min_by_key(|(_, (_, time))| *time)
        {
            let oldest = oldest.clone();
            self.metadata.remove(&oldest);
        }
    }
}

impl Connection {
    pub(crate) fn check_terminal_file_link(&self, link: TerminalFileLink, id: Uuid) {
        let _ = self
            .commands
            .send(super::Command::TerminalFileLink(Request::Check {
                link,
                id,
            }));
    }

    pub(crate) fn open_terminal_file_link(
        &self,
        link: TerminalFileLink,
        preferred: Option<String>,
    ) {
        let _ = self
            .commands
            .send(super::Command::TerminalFileLink(Request::Open {
                link,
                preferred,
            }));
    }
}

impl Session {
    pub(super) async fn terminal_file_link(
        &mut self,
        request: Request,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let link = request.link();
        if !self.terminal_links.can_operate
            || !self.filesystem_read
            || !self.terminals.contains_key(&link.attachment)
        {
            if let Request::Check { link, id } = request {
                events.send(Event::TerminalFileLinkChecked {
                    attachment: link.attachment,
                    id,
                    available: false,
                });
            }
            return Ok(());
        }
        if matches!(request, Request::Open { .. })
            && self.terminal_links.pending.values().any(|pending| {
                std::iter::once(&pending.request)
                    .chain(&pending.waiting)
                    .any(|request| {
                        matches!(request, Request::Open { .. }) && request.link() == link
                    })
            })
        {
            return Ok(());
        }
        if let Some(available) = self.terminal_links.cached(&link.path) {
            return self
                .finish_terminal_metadata(request, available, events)
                .await;
        }
        if let Some(pending) = self.terminal_links.pending.values_mut().find(|pending| {
            let active = pending.request.link();
            pending.editor.is_none()
                && active.attachment == link.attachment
                && active.path == link.path
        }) {
            pending.waiting.push(request);
            return Ok(());
        }
        let payload = json!({"paths":[link.path]});
        let id = self.allocate_id();
        self.terminal_links.pending.insert(
            id.clone(),
            Pending {
                began: Instant::now(),
                request,
                editor: None,
                waiting: Vec::new(),
            },
        );
        self.send_request(&id, "filesystem.getMetadata", payload)
            .await
    }

    pub(super) async fn terminal_file_link_exit(
        &mut self,
        request_id: &str,
        exit: RpcExit,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let Some(pending) = self.terminal_links.pending.remove(request_id) else {
            return Ok(());
        };
        let link = pending.request.link();
        if !self.terminals.contains_key(&link.attachment) {
            return Ok(());
        }
        if let Some(editor) = pending.editor {
            events.send(Event::TerminalFileLinkOpened {
                attachment: link.attachment,
                editor: Some(editor),
                result: exit.into_result().map(|_| ()),
            });
            return Ok(());
        }
        let result = exit.into_result();
        let available = result.as_ref().is_ok_and(|value| {
            matches!(
                value["entries"][0]["kind"].as_str(),
                Some("file" | "directory")
            )
        });
        if result.is_ok() {
            self.terminal_links.remember(link.path.clone(), available);
        }
        for request in std::iter::once(pending.request).chain(pending.waiting) {
            self.finish_terminal_metadata(request, available, events)
                .await?;
        }
        Ok(())
    }

    async fn finish_terminal_metadata(
        &mut self,
        request: Request,
        available: bool,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        match request {
            Request::Check { link, id } => {
                events.send(Event::TerminalFileLinkChecked {
                    attachment: link.attachment,
                    id,
                    available,
                });
                Ok(())
            }
            Request::Open { .. } if available => self.launch_terminal_editor(request, events).await,
            Request::Open { .. } => Ok(()),
        }
    }

    async fn launch_terminal_editor(
        &mut self,
        request: Request,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let Request::Open { link, preferred } = &request else {
            unreachable!("editor request is a link activation");
        };
        let editors = &self.terminal_links.editors;
        let selected = preferred
            .as_ref()
            .filter(|preferred| editors.contains(preferred))
            .or_else(|| editors.first());
        let Some(editor) = selected.cloned() else {
            events.send(Event::TerminalFileLinkOpened {
                attachment: link.attachment,
                editor: None,
                result: Err(format!("No available editor can open {}.", link.target)),
            });
            return Ok(());
        };
        let payload = json!({"cwd":link.target,"editor":editor,"projectRoot":link.project_root});
        let id = self.allocate_id();
        self.terminal_links.pending.insert(
            id.clone(),
            Pending {
                began: Instant::now(),
                request,
                editor: Some(editor),
                waiting: Vec::new(),
            },
        );
        self.send_request(&id, "shell.openInEditor", payload).await
    }

    pub(super) async fn expire_terminal_links(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        let expired: Vec<_> = self
            .terminal_links
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending.began.elapsed() >= REQUEST_TIMEOUT
                    || !self
                        .terminals
                        .contains_key(&pending.request.link().attachment)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let pending = self
                .terminal_links
                .pending
                .remove(&id)
                .expect("expired link request");
            let attachment = pending.request.link().attachment;
            if self.terminals.contains_key(&attachment) {
                for request in std::iter::once(pending.request).chain(pending.waiting) {
                    match request {
                        Request::Check { id, .. } => events.send(Event::TerminalFileLinkChecked {
                            attachment,
                            id,
                            available: false,
                        }),
                        Request::Open { .. } => {
                            if let Some(editor) = pending.editor.as_ref() {
                                events.send(Event::TerminalFileLinkOpened {
                                    attachment,
                                    editor: Some(editor.clone()),
                                    result: Err("Could not confirm that the editor opened.".into()),
                                });
                            }
                        }
                    }
                }
            }
            self.interrupt(&id).await?;
        }
        Ok(())
    }
}

pub(super) fn config_editors(config: Option<&Value>) -> Vec<String> {
    config
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
