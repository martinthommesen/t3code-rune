use std::fs::File;
use std::io::{self, Write};

use reqwest::{Client, Response, StatusCode, Url};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::workspace_image::Source;

use super::file_preview::{AssetOutcome, AssetPurpose};
use super::{Environment, project_favicon::asset_url};

pub(super) struct Downloads {
    base: Url,
    client: Client,
    active: Option<Transfer>,
}

struct Transfer {
    id: Uuid,
    task: JoinHandle<AssetOutcome>,
}

impl Drop for Transfer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Downloads {
    pub(super) fn new(environment: &Environment) -> Self {
        Self {
            base: environment.url.clone(),
            client: environment.client.clone(),
            active: None,
        }
    }

    pub(super) fn cancel(&mut self, id: Option<Uuid>) {
        if self
            .active
            .as_ref()
            .is_some_and(|transfer| id.is_none_or(|id| transfer.id == id))
        {
            self.active = None;
        }
    }

    pub(super) fn resolve_url(&self, relative: &str) -> Option<Url> {
        asset_url(&self.base, relative)
    }

    pub(super) fn start(&mut self, id: Uuid, url: Url, purpose: AssetPurpose) {
        let client = self.client.clone();
        let task = tokio::spawn(async move {
            let result = match purpose {
                AssetPurpose::ShowImage | AssetPurpose::CopyImage => image(client, url).await,
                _ => download(client, url).await.map(AssetOutcome::Prepared),
            };
            result.unwrap_or_else(|| {
                AssetOutcome::Failed("The file could not be loaded. Try again.".into())
            })
        });
        self.active = Some(Transfer { id, task });
    }

    pub(super) async fn next(&mut self) -> (Uuid, AssetOutcome) {
        let Some(transfer) = &mut self.active else {
            return std::future::pending().await;
        };
        let id = transfer.id;
        let result = (&mut transfer.task).await.unwrap_or_else(|_| {
            AssetOutcome::Failed("The file could not be loaded. Try again.".into())
        });
        self.active = None;
        (id, result)
    }
}

async fn open(client: Client, url: Url) -> Option<Response> {
    let response = client.get(url).send().await.ok()?;
    (response.status() == StatusCode::OK).then_some(response)
}

async fn image(client: Client, url: Url) -> Option<AssetOutcome> {
    let mut response = open(client, url).await?;
    let mut source = Vec::new();
    let limit = rune::clipboard::MAX_IMAGE_BYTES;
    while let Some(bytes) = response.chunk().await.ok()? {
        if source.len().saturating_add(bytes.len()) > limit {
            return None;
        }
        source.extend(bytes);
    }
    Some(
        Source::read(source)
            .await
            .map_or(AssetOutcome::InvalidImage, AssetOutcome::Image),
    )
}

async fn download(client: Client, url: Url) -> Option<File> {
    let mut response = open(client, url).await?;
    let mut file = tokio::task::spawn_blocking(tempfile::tempfile)
        .await
        .ok()?
        .ok()?;
    while let Some(bytes) = response.chunk().await.ok()? {
        file = tokio::task::spawn_blocking(move || {
            file.write_all(&bytes)?;
            Ok::<_, io::Error>(file)
        })
        .await
        .ok()?
        .ok()?;
    }
    Some(file)
}
