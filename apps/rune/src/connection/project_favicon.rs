use std::collections::{HashMap, VecDeque};

use reqwest::{Client, Url, header::CONTENT_TYPE};
use serde_json::{Value, json};
use tokio::{task::JoinSet, time::Instant};

use super::{Environment, Event, EventSender, REQUEST_TIMEOUT, Session, SessionStop};

const CONCURRENT_FAVICONS: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FaviconTarget {
    pub(crate) cwd: String,
    pub(crate) path: Option<String>,
}

impl FaviconTarget {
    fn payload(&self) -> Value {
        let mut resource = json!({"_tag":"project-favicon", "cwd":self.cwd});
        if let Some(path) = &self.path {
            resource["path"] = json!(path);
        }
        json!({"resource":resource})
    }
}

pub(crate) struct FaviconBytes {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: String,
}

type Loaded = (FaviconTarget, Option<FaviconBytes>);

pub(super) struct Favicons {
    base: Url,
    client: Client,
    waiting: VecDeque<FaviconTarget>,
    grants: HashMap<String, (FaviconTarget, Instant)>,
    downloads: JoinSet<Loaded>,
}

impl Favicons {
    pub(super) fn new(environment: &Environment) -> Self {
        Self {
            base: environment.url.clone(),
            client: environment.client.clone(),
            waiting: VecDeque::new(),
            grants: HashMap::new(),
            downloads: JoinSet::new(),
        }
    }

    pub(super) fn enqueue(&mut self, targets: Vec<FaviconTarget>) {
        self.waiting.extend(targets);
    }

    fn next_target(&mut self) -> Option<FaviconTarget> {
        if self.grants.len() + self.downloads.len() >= CONCURRENT_FAVICONS {
            return None;
        }
        self.waiting.pop_front()
    }

    pub(super) fn has_grant(&self, request: &str) -> bool {
        self.grants.contains_key(request)
    }

    pub(super) fn resolve(
        &mut self,
        request: &str,
        result: Result<Value, String>,
        events: &EventSender<impl Fn()>,
    ) {
        let Some((target, _)) = self.grants.remove(request) else {
            return;
        };
        let url = result
            .ok()
            .and_then(|issued| asset_url(&self.base, issued["relativeUrl"].as_str()?));
        if let Some(url) = url {
            let client = self.client.clone();
            self.downloads
                .spawn(async move { (target, download(client, url).await) });
        } else {
            events.send(Event::ProjectFavicon {
                target,
                image: None,
            });
        }
    }

    pub(super) fn expire(&mut self, events: &EventSender<impl Fn()>) {
        self.grants.retain(|_, (target, started)| {
            if started.elapsed() < REQUEST_TIMEOUT {
                return true;
            }
            events.send(Event::ProjectFavicon {
                target: target.clone(),
                image: None,
            });
            false
        });
    }

    pub(super) async fn next_download(&mut self) -> Option<Loaded> {
        if self.downloads.is_empty() {
            return std::future::pending().await;
        }
        self.downloads.join_next().await.and_then(Result::ok)
    }
}

impl Session {
    pub(super) async fn request_project_favicons(&mut self) -> Result<(), SessionStop> {
        while let Some(target) = self.favicons.next_target() {
            let request = self.allocate_id();
            let payload = target.payload();
            self.favicons
                .grants
                .insert(request.clone(), (target, Instant::now()));
            self.send_request(&request, "assets.createUrl", payload)
                .await?;
        }
        Ok(())
    }
}

pub(super) fn asset_url(base: &Url, relative: &str) -> Option<Url> {
    if !relative.starts_with("/api/assets/") {
        return None;
    }
    let asset = base.join(relative).ok()?;
    if asset.origin() != base.origin() || !asset.path().starts_with("/api/assets/") {
        return None;
    }
    let name = asset.path_segments()?.next_back()?;
    (!name.is_empty() && name != "project-favicon-missing").then_some(asset)
}

async fn download(client: Client, url: Url) -> Option<FaviconBytes> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mime = response
        .headers()
        .get(CONTENT_TYPE)?
        .to_str()
        .ok()?
        .to_owned();
    let bytes = response.bytes().await.ok()?.to_vec();
    Some(FaviconBytes { bytes, mime })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fetches_assets_on_the_paired_origin() {
        let base = Url::parse("https://workspace.example:3773").unwrap();
        assert_eq!(
            asset_url(&base, "/api/assets/grant/project%20icon.svg")
                .unwrap()
                .as_str(),
            "https://workspace.example:3773/api/assets/grant/project%20icon.svg"
        );
        for path in [
            "//other.example/api/assets/grant/icon.svg",
            "https://workspace.example:3773/api/assets/grant/icon.svg",
            "/api/assets/grant/project-favicon-missing",
            "/api/assets/%2e%2e/oauth/token",
        ] {
            assert!(asset_url(&base, path).is_none(), "{path}");
        }
    }
}
