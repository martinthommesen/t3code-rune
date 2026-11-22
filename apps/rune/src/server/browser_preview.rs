mod registry;
mod stream;
pub(super) use stream::upgrade;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::{Mutex, broadcast};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use super::preview_browser::{Browser, Navigation as BrowserNavigation, Page};
use super::turn_commands::timestamp;

pub(super) use registry::{List, Navigation, Open, Snapshot};

const DEFAULT_WIDTH: u32 = 1_280;
const DEFAULT_HEIGHT: u32 = 800;
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Preview {
    registry: registry::Registry,
    engine: Mutex<Engine>,
    browser: PathBuf,
    profiles: PathBuf,
    controllers: std::sync::Mutex<HashMap<String, Uuid>>,
}

#[derive(Default)]
struct Engine {
    browser: Option<Browser>,
    pages: HashMap<String, OpenPage>,
}

#[derive(Clone)]
struct OpenPage {
    target: String,
    page: Arc<Mutex<Page>>,
}

impl Preview {
    pub(super) fn new(browser: PathBuf, profiles: PathBuf) -> Self {
        Self {
            registry: registry::Registry::default(),
            engine: Mutex::default(),
            browser,
            profiles,
            controllers: std::sync::Mutex::default(),
        }
    }

    pub(super) fn configured(profile_dir: &Path) -> Option<Arc<Self>> {
        let path = std::env::var_os("PATH")?;
        let names = if cfg!(windows) {
            &["chrome.exe", "chromium.exe"][..]
        } else {
            &[
                "chromium",
                "chromium-browser",
                "google-chrome",
                "google-chrome-stable",
            ][..]
        };
        let browser = names.iter().find_map(|name| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|path| path.is_file())
        })?;
        Some(Arc::new(Self::new(
            browser,
            profile_dir.join("preview-chromium"),
        )))
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.registry.subscribe()
    }

    pub(super) fn open(self: &Arc<Self>, input: Open) -> Result<Snapshot, Value> {
        let snapshot = self.registry.open(input, &now())?;
        if let Navigation::Loading { url, .. } = &snapshot.nav_status {
            let (thread, tab, url) = (
                snapshot.thread_id.clone(),
                snapshot.tab_id.clone(),
                url.clone(),
            );
            let preview = Arc::clone(self);
            tokio::spawn(async move { preview.load(thread, tab, url).await });
        }
        Ok(snapshot)
    }

    pub(super) fn list(&self, thread: &str) -> List {
        self.registry.list(thread)
    }

    pub(super) async fn close(&self, thread: &str, tab: Option<&str>) -> io::Result<()> {
        let closed = self.registry.close(thread, tab, &now());
        if closed.is_empty() {
            return Ok(());
        }
        let mut engine = self.engine.lock().await;
        for tab in closed {
            self.controllers
                .lock()
                .expect("preview controllers")
                .remove(&tab);
            if let Some(page) = engine.pages.remove(&tab)
                && let Some(browser) = engine.browser.as_mut()
            {
                let _ = browser.close_page(&page.target).await;
            }
        }
        Ok(())
    }

    pub(super) async fn frame(&self, thread: &str, tab: &str) -> io::Result<Option<Vec<u8>>> {
        self.check_tab(thread, tab)?;
        let Some(page) = self.page(tab).await else {
            return Ok(None);
        };
        let mut page = page.page.lock().await;
        if let Ok(navigation) = page.navigation().await
            && let Some(before) = self.registry.current(thread, tab)
            && !before.matches(
                &navigation.url,
                &navigation.title,
                navigation.can_go_back,
                navigation.can_go_forward,
            )
            && page.document_ready(&navigation.url).await.unwrap_or(false)
        {
            self.complete(thread, tab, &navigation.url.clone(), Ok(navigation));
        }
        page.frame().await.map(Some)
    }

    pub(super) async fn resize(
        &self,
        thread: &str,
        tab: &str,
        width: u32,
        height: u32,
    ) -> io::Result<()> {
        self.check_tab(thread, tab)?;
        let page = self.page(tab).await.ok_or_else(page_pending)?;
        let mut page = page.page.lock().await;
        page.resize(width, height).await
    }

    pub(super) async fn navigate(&self, thread: &str, tab: &str, url: Url) -> io::Result<()> {
        self.check_tab(thread, tab)?;
        let page = self.page(tab).await.ok_or_else(page_pending)?;
        let mut page = page.page.lock().await;
        let previous = page.navigation().await?;
        self.registry.navigated(
            thread,
            tab,
            Navigation::Loading {
                url: url.as_str().to_owned(),
                title: String::new(),
            },
            previous.can_go_back,
            previous.can_go_forward,
            &now(),
        );
        let result = async {
            page.navigate(&url).await?;
            wait_document(&mut page, Some(url.as_str()), Some(&previous.url)).await
        }
        .await;
        self.complete(thread, tab, url.as_str(), result);
        Ok(())
    }

    pub(super) async fn step_history(
        &self,
        thread: &str,
        tab: &str,
        offset: isize,
    ) -> io::Result<()> {
        self.check_tab(thread, tab)?;
        let page = self.page(tab).await.ok_or_else(page_pending)?;
        let mut page = page.page.lock().await;
        let previous = page.navigation().await?;
        if let Some(url) = page.step_history(offset).await? {
            self.registry.navigated(
                thread,
                tab,
                Navigation::Loading {
                    url: url.clone(),
                    title: String::new(),
                },
                previous.can_go_back,
                previous.can_go_forward,
                &now(),
            );
            let result = wait_document(&mut page, Some(&url), Some(&previous.url)).await;
            self.complete(thread, tab, &url, result);
        }
        Ok(())
    }

    async fn reload(&self, thread: &str, tab: &str, ignore_cache: bool) -> io::Result<()> {
        self.check_tab(thread, tab)?;
        let page = self.page(tab).await.ok_or_else(page_pending)?;
        let mut page = page.page.lock().await;
        let previous = page.navigation().await?;
        self.registry.navigated(
            thread,
            tab,
            Navigation::Loading {
                url: previous.url.clone(),
                title: String::new(),
            },
            previous.can_go_back,
            previous.can_go_forward,
            &now(),
        );
        let result = async {
            page.reload(ignore_cache).await?;
            wait_document(&mut page, Some(&previous.url), None).await
        }
        .await;
        self.complete(thread, tab, &previous.url, result);
        Ok(())
    }

    async fn mouse(&self, thread: &str, tab: &str, event: Value) -> io::Result<()> {
        self.check_tab(thread, tab)?;
        let page = self.page(tab).await.ok_or_else(page_pending)?;
        page.page.lock().await.mouse(event).await
    }

    async fn text(&self, thread: &str, tab: &str, text: &str) -> io::Result<()> {
        self.check_tab(thread, tab)?;
        let page = self.page(tab).await.ok_or_else(page_pending)?;
        page.page.lock().await.text(text).await
    }

    fn controller(&self, tab: &str, viewer: Uuid) -> &'static str {
        match self
            .controllers
            .lock()
            .expect("preview controllers")
            .get(tab)
        {
            Some(id) if *id == viewer => "you",
            Some(_) => "another-viewer",
            None => "unclaimed",
        }
    }

    fn claim(&self, tab: &str, viewer: Uuid, force: bool) -> bool {
        let mut controllers = self.controllers.lock().expect("preview controllers");
        let controller = controllers.entry(tab.to_owned()).or_insert(viewer);
        if force {
            *controller = viewer;
        }
        *controller == viewer
    }

    fn release(&self, tab: &str, viewer: Uuid) {
        self.controllers
            .lock()
            .expect("preview controllers")
            .retain(|key, current| key != tab || *current != viewer);
    }

    pub(super) async fn shutdown(&self) -> io::Result<()> {
        let mut engine = self.engine.lock().await;
        engine.pages.clear();
        if let Some(browser) = engine.browser.take() {
            browser.shutdown().await?;
        }
        Ok(())
    }

    async fn load(&self, thread: String, tab: String, url: String) {
        let result = self.load_page(&thread, &tab, &url).await;
        self.complete(&thread, &tab, &url, result);
    }

    async fn load_page(&self, thread: &str, tab: &str, url: &str) -> io::Result<BrowserNavigation> {
        self.check_tab(thread, tab)?;
        let parsed = Url::parse(url).map_err(io::Error::other)?;
        let mut engine = self.engine.lock().await;
        if engine.browser.is_none() {
            std::fs::create_dir_all(&self.profiles)?;
            engine.browser = Some(Browser::launch(&self.browser, &self.profiles).await?);
        }
        self.check_tab(thread, tab)?;
        let browser = engine.browser.as_mut().expect("preview process started");
        let page = browser.open(&parsed, DEFAULT_WIDTH, DEFAULT_HEIGHT).await?;
        let target = page.target_id().to_owned();
        if !self.registry.mark_headless(thread, tab, &now()) {
            browser.close_page(&target).await?;
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "The preview tab was closed.",
            ));
        }
        let page = Arc::new(Mutex::new(page));
        engine.pages.insert(
            tab.to_owned(),
            OpenPage {
                target,
                page: Arc::clone(&page),
            },
        );
        drop(engine);
        let mut page = page.lock().await;
        wait_document(&mut page, Some(url), None).await
    }

    async fn page(&self, tab: &str) -> Option<OpenPage> {
        self.engine.lock().await.pages.get(tab).cloned()
    }

    fn check_tab(&self, thread: &str, tab: &str) -> io::Result<()> {
        if self.registry.current(thread, tab).is_some() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "The preview tab does not exist in this thread.",
            ))
        }
    }

    fn complete(
        &self,
        thread: &str,
        tab: &str,
        requested_url: &str,
        result: io::Result<BrowserNavigation>,
    ) {
        let (navigation, back, forward) = match result {
            Ok(result) => (
                Navigation::Success {
                    url: result.url,
                    title: result.title,
                },
                result.can_go_back,
                result.can_go_forward,
            ),
            Err(error) => (
                Navigation::LoadFailed {
                    url: requested_url.to_owned(),
                    title: String::new(),
                    code: -2,
                    description: error.to_string(),
                },
                false,
                false,
            ),
        };
        self.registry
            .navigated(thread, tab, navigation, back, forward, &now());
    }
}

async fn wait_document(
    page: &mut Page,
    target: Option<&str>,
    previous: Option<&str>,
) -> io::Result<BrowserNavigation> {
    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        if let Ok(status) = page.navigation().await {
            if let Some(target) = target
                && status.url == target
                && page.document_ready(&status.url).await.unwrap_or(false)
            {
                return Ok(status);
            }
            if previous.map_or_else(
                || target.is_some_and(|target| status.url != target),
                |previous| status.url != previous,
            ) && (status.url.starts_with("http://") || status.url.starts_with("https://"))
                && page.document_ready(&status.url).await.unwrap_or(false)
            {
                return Ok(status);
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "The preview page did not load.",
            ));
        }
        sleep(Duration::from_millis(40)).await;
    }
}

fn page_pending() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "The preview tab is still opening.",
    )
}

fn now() -> String {
    timestamp(OffsetDateTime::now_utc())
}
