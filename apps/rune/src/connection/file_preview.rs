use std::fs::File;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    Event, EventSender, Instant, REQUEST_TIMEOUT, RpcCause, RpcExit, Session, SessionStop,
};

const MAX_PREVIEW_BYTES: usize = 1024 * 1024;

pub(crate) struct ReadRequest {
    pub(crate) id: Uuid,
    pub(crate) thread_id: String,
    pub(crate) cwd: String,
    pub(crate) relative_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Contents {
    pub(crate) contents: String,
    pub(crate) byte_length: u64,
    pub(crate) truncated: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct FailureFields {
    #[serde(rename = "_tag")]
    tag: Option<String>,
    failure: Option<String>,
    cwd: Option<String>,
    resolved_path: Option<String>,
    operation_path: Option<String>,
    operation: Option<String>,
}

pub(crate) struct ReadFailure {
    pub(crate) message: String,
    pub(crate) attempted_path: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) binary: bool,
}

impl ReadFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            attempted_path: None,
            cwd: None,
            binary: false,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AssetPurpose {
    Inspect,
    Download,
    ShowImage,
    SaveImage,
}

pub(crate) enum AssetOutcome {
    Available,
    Prepared(File),
    Image(Arc<[u8]>),
    Failed(String),
}

enum Operation {
    Read,
    Asset(AssetPurpose),
}

pub(super) struct PendingRead {
    request_id: String,
    id: Uuid,
    started: Instant,
    operation: Operation,
}

impl Session {
    pub(super) async fn read_file_preview(
        &mut self,
        input: ReadRequest,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if !self.can_read_preview(&input.thread_id) {
            events.send(Event::FilePreviewLoaded {
                id: input.id,
                result: Err(ReadFailure::new("This connection cannot read host files.")),
            });
            return Ok(());
        }
        self.request_preview(
            input.id,
            Operation::Read,
            "projects.readFile",
            json!({"cwd": input.cwd, "relativePath": input.relative_path}),
        )
        .await
    }

    pub(super) async fn prepare_file_asset(
        &mut self,
        input: ReadRequest,
        purpose: AssetPurpose,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if !self.can_read_preview(&input.thread_id) {
            events.send(Event::FilePreviewAsset {
                id: input.id,
                result: AssetOutcome::Failed("This connection cannot read host files.".into()),
            });
            return Ok(());
        }
        let resource = match purpose {
            AssetPurpose::ShowImage | AssetPurpose::SaveImage => json!({
                "_tag": "workspace-file",
                "threadId": input.thread_id,
                "path": input.relative_path,
            }),
            AssetPurpose::Inspect | AssetPurpose::Download => json!({
                "_tag": "media-file",
                "threadId": input.thread_id,
                "path": input.relative_path,
                "disposition": "attachment",
            }),
        };
        self.request_preview(
            input.id,
            Operation::Asset(purpose),
            "assets.createUrl",
            json!({"resource": resource}),
        )
        .await
    }

    fn can_read_preview(&self, thread_id: &str) -> bool {
        self.filesystem_read
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| thread.thread_id == thread_id)
    }

    async fn request_preview(
        &mut self,
        id: Uuid,
        operation: Operation,
        method: &'static str,
        payload: Value,
    ) -> Result<(), SessionStop> {
        self.cancel_file_preview(None).await?;
        let request_id = self.allocate_id();
        self.pending_file_preview = Some(PendingRead {
            request_id: request_id.clone(),
            id,
            started: Instant::now(),
            operation,
        });
        self.send_request(&request_id, method, payload).await
    }

    pub(super) async fn cancel_file_preview(
        &mut self,
        preview_id: Option<Uuid>,
    ) -> Result<(), SessionStop> {
        self.preview_downloads.cancel(preview_id);
        if self
            .pending_file_preview
            .as_ref()
            .is_some_and(|pending| preview_id.is_none_or(|id| pending.id == id))
            && let Some(pending) = self.pending_file_preview.take()
        {
            self.interrupt(&pending.request_id).await?;
        }
        Ok(())
    }

    pub(super) fn matches_file_preview(&self, request_id: &str) -> bool {
        self.pending_file_preview
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
    }

    pub(super) fn finish_file_preview(&mut self, exit: RpcExit, events: &EventSender<impl Fn()>) {
        let Some(pending) = self.pending_file_preview.take() else {
            return;
        };
        match pending.operation {
            Operation::Read => events.send(Event::FilePreviewLoaded {
                id: pending.id,
                result: result(exit),
            }),
            Operation::Asset(purpose) => {
                let url = exit.into_result().ok().and_then(|value| {
                    self.preview_downloads
                        .resolve_url(value["relativeUrl"].as_str()?)
                });
                let result = match (url, purpose) {
                    (
                        Some(url),
                        purpose @ (AssetPurpose::Download
                        | AssetPurpose::ShowImage
                        | AssetPurpose::SaveImage),
                    ) => {
                        self.preview_downloads.start(pending.id, url, purpose);
                        return;
                    }
                    (Some(_), AssetPurpose::Inspect) => AssetOutcome::Available,
                    (None, _) => AssetOutcome::Failed("Unable to load file preview.".into()),
                };
                events.send(Event::FilePreviewAsset {
                    id: pending.id,
                    result,
                });
            }
        }
    }

    pub(super) async fn expire_file_preview(
        &mut self,
        events: &EventSender<impl Fn()>,
    ) -> Result<(), SessionStop> {
        if self
            .pending_file_preview
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= REQUEST_TIMEOUT)
            && let Some(pending) = self.pending_file_preview.take()
        {
            let message = "The environment did not return the file in time.";
            events.send(match pending.operation {
                Operation::Read => Event::FilePreviewLoaded {
                    id: pending.id,
                    result: Err(ReadFailure::new(message)),
                },
                Operation::Asset(_) => Event::FilePreviewAsset {
                    id: pending.id,
                    result: AssetOutcome::Failed(message.into()),
                },
            });
            self.interrupt(&pending.request_id).await?;
        }
        Ok(())
    }
}

fn result(exit: RpcExit) -> Result<Contents, ReadFailure> {
    match exit {
        RpcExit::Success { value } => serde_json::from_value::<Contents>(value)
            .ok()
            .filter(|file| {
                file.contents.len() <= MAX_PREVIEW_BYTES && !file.contents.contains('\0')
            })
            .ok_or_else(|| ReadFailure::new("Unable to load file preview.")),
        RpcExit::Failure { cause } => Err(cause
            .into_iter()
            .find_map(|reason| match reason {
                RpcCause::Fail { error } => {
                    Some(read_failure(error.file, error.message.or(error.detail)))
                }
                RpcCause::Other => None,
            })
            .unwrap_or_else(|| ReadFailure::new("Unable to load file preview."))),
    }
}

fn read_failure(fields: FailureFields, fallback: Option<String>) -> ReadFailure {
    if fields.tag.as_deref() != Some("ProjectReadFileError") {
        return ReadFailure::new(
            fallback.unwrap_or_else(|| "Unable to load file preview.".to_owned()),
        );
    }
    let message = match fields.failure.as_deref() {
        Some("path_not_file") => "The path is a directory or special file, not a regular file.",
        Some("binary_file") => "The file is binary and cannot be displayed as text.",
        Some("workspace_path_outside_root") => "The requested path is outside the workspace.",
        Some("resolved_path_outside_root") => {
            "The path resolves to a location outside the workspace."
        }
        Some("operation_failed")
            if fields.operation.as_deref() == Some("realpath-workspace-root") =>
        {
            "The workspace folder could not be accessed."
        }
        Some("operation_failed") => {
            "The file could not be accessed or read. It may be missing or inaccessible."
        }
        _ => fallback
            .as_deref()
            .unwrap_or("Unable to load file preview."),
    };
    ReadFailure {
        message: message.to_owned(),
        attempted_path: fields.resolved_path.or(fields.operation_path),
        cwd: fields.cwd,
        binary: fields.failure.as_deref() == Some("binary_file"),
    }
}
