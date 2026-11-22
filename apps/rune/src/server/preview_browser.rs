use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) struct Browser {
    process: Child,
    devtools: DevTools,
    port: u16,
    _profile: TempDir,
}

pub(super) struct Page {
    target: String,
    devtools: DevTools,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Navigation {
    pub url: String,
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    current_index: usize,
    entries: Vec<HistoryEntry>,
}

#[derive(Deserialize)]
struct HistoryEntry {
    id: i64,
    url: String,
    title: String,
}

struct DevTools {
    socket: Socket,
    next_id: u64,
}

impl Browser {
    pub(super) async fn launch(binary: &Path, profile_parent: &Path) -> io::Result<Self> {
        let profile = tempfile::Builder::new()
            .prefix("chromium-")
            .tempdir_in(profile_parent)?;
        let mut process = Command::new(binary)
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--metrics-recording-only",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let (port, path) = endpoint(&mut process, profile.path()).await?;
        let devtools = DevTools::connect(port, &path).await?;
        Ok(Self {
            process,
            devtools,
            port,
            _profile: profile,
        })
    }

    pub(super) async fn open(&mut self, url: &Url, width: u32, height: u32) -> io::Result<Page> {
        let url = web_url(url)?;
        let result = self
            .devtools
            .call("Target.createTarget", json!({"url":url}))
            .await?;
        let target = result["targetId"]
            .as_str()
            .filter(|target| {
                !target.is_empty() && target.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .ok_or_else(|| io::Error::other("Chromium returned an invalid target ID."))?
            .to_owned();
        let page = async {
            let devtools =
                DevTools::connect(self.port, &format!("/devtools/page/{target}")).await?;
            let mut page = Page {
                target: target.clone(),
                devtools,
            };
            page.resize(width, height).await?;
            Ok(page)
        }
        .await;
        if page.is_err() {
            let _ = self.close_page(&target).await;
        }
        page
    }

    pub(super) async fn close_page(&mut self, target: &str) -> io::Result<()> {
        let result = self
            .devtools
            .call("Target.closeTarget", json!({"targetId":target}))
            .await?;
        if result["success"] != true {
            return Err(io::Error::other("Chromium did not close the preview page."));
        }
        Ok(())
    }

    pub(super) async fn shutdown(mut self) -> io::Result<()> {
        let _ = timeout(
            CLOSE_TIMEOUT,
            self.devtools.call("Browser.close", json!({})),
        )
        .await;
        match timeout(CLOSE_TIMEOUT, self.process.wait()).await {
            Ok(exit) => exit.map(|_| ()),
            Err(_) => self.process.kill().await,
        }
    }
}

impl Page {
    pub(super) fn target_id(&self) -> &str {
        &self.target
    }

    pub(super) async fn navigate(&mut self, url: &Url) -> io::Result<()> {
        let result = self
            .devtools
            .call("Page.navigate", json!({"url":web_url(url)?}))
            .await?;
        if let Some(error) = result["errorText"].as_str() {
            return Err(io::Error::other(error.to_owned()));
        }
        Ok(())
    }

    pub(super) async fn navigation(&mut self) -> io::Result<Navigation> {
        let history = self.history().await?;
        let current = history.entries.get(history.current_index).ok_or_else(|| {
            io::Error::other("Chromium returned an invalid navigation history position.")
        })?;
        Ok(Navigation {
            url: current.url.clone(),
            title: current.title.clone(),
            can_go_back: history.current_index > 0,
            can_go_forward: history.current_index + 1 < history.entries.len(),
        })
    }

    pub(super) async fn document_ready(&mut self, url: &str) -> io::Result<bool> {
        let result = self.devtools.call("Runtime.evaluate", json!({
            "expression":"({ready: document.readyState === 'complete', url: location.href})",
            "returnByValue": true,
        })).await?;
        Ok(result["result"]["value"]["ready"] == true && result["result"]["value"]["url"] == url)
    }

    pub(super) async fn step_history(&mut self, offset: isize) -> io::Result<Option<String>> {
        let history = self.history().await?;
        let Some(next) = history
            .current_index
            .checked_add_signed(offset)
            .and_then(|index| history.entries.get(index))
        else {
            return Ok(None);
        };
        self.devtools
            .call("Page.navigateToHistoryEntry", json!({"entryId":next.id}))
            .await?;
        Ok(Some(next.url.clone()))
    }

    pub(super) async fn reload(&mut self, ignore_cache: bool) -> io::Result<()> {
        self.devtools
            .call("Page.reload", json!({"ignoreCache":ignore_cache}))
            .await?;
        Ok(())
    }

    pub(super) async fn mouse(&mut self, event: Value) -> io::Result<()> {
        self.devtools
            .call("Input.dispatchMouseEvent", event)
            .await?;
        Ok(())
    }

    pub(super) async fn text(&mut self, text: &str) -> io::Result<()> {
        self.devtools
            .call("Input.insertText", json!({"text": text}))
            .await?;
        Ok(())
    }

    async fn history(&mut self) -> io::Result<History> {
        let result = self
            .devtools
            .call("Page.getNavigationHistory", json!({}))
            .await?;
        serde_json::from_value(result).map_err(io::Error::other)
    }

    pub(super) async fn resize(&mut self, width: u32, height: u32) -> io::Result<()> {
        self.devtools.call("Emulation.setDeviceMetricsOverride", json!({
            "width":width.clamp(1,3840),"height":height.clamp(1,2160),"deviceScaleFactor":1,"mobile":false,
        })).await?;
        Ok(())
    }

    pub(super) async fn frame(&mut self) -> io::Result<Vec<u8>> {
        let result = self
            .devtools
            .call(
                "Page.captureScreenshot",
                json!({
                    "format":"jpeg","quality":70,"fromSurface":true,"captureBeyondViewport":false,
                }),
            )
            .await?;
        let encoded = result["data"]
            .as_str()
            .ok_or_else(|| io::Error::other("Chromium did not return a preview frame."))?;
        STANDARD.decode(encoded).map_err(io::Error::other)
    }
}

impl DevTools {
    async fn connect(port: u16, path: &str) -> io::Result<Self> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (socket, _) = timeout(
            COMMAND_TIMEOUT,
            connect_async_with_config(url, Some(config), false),
        )
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
        Ok(Self { socket, next_id: 0 })
    }

    async fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        timeout(COMMAND_TIMEOUT, async {
            let request = json!({"id":id,"method":method,"params":params});
            self.socket
                .send(Message::Text(request.to_string().into()))
                .await
                .map_err(io::Error::other)?;
            while let Some(message) = self.socket.next().await {
                match message.map_err(io::Error::other)? {
                    Message::Text(text) => {
                        let message: Value =
                            serde_json::from_str(text.as_str()).map_err(io::Error::other)?;
                        if message["id"].as_u64() != Some(id) {
                            continue;
                        }
                        if let Some(error) = message.get("error") {
                            return Err(io::Error::other(format!(
                                "Chromium rejected {method}: {error}"
                            )));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Message::Close(_) => break,
                    _ => (),
                }
            }
            Err(io::Error::other(format!(
                "Chromium disconnected during {method}."
            )))
        })
        .await
        .map_err(io::Error::other)?
    }
}

async fn endpoint(process: &mut Child, profile: &Path) -> io::Result<(u16, String)> {
    let endpoint_path = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(status) = process.try_wait()? {
            return Err(io::Error::other(format!(
                "Chromium exited before opening its preview service: {status}"
            )));
        }
        if let Ok(portfile) = std::fs::read_to_string(&endpoint_path) {
            let mut lines = portfile.lines();
            if let (Some(port), Some(path)) = (
                lines.next().and_then(|line| line.parse::<u16>().ok()),
                lines.next(),
            ) && port != 0
                && let Some(browser) = path.strip_prefix("/devtools/browser/")
                && !browser.is_empty()
                && browser
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Ok((port, path.to_owned()));
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Chromium did not start its preview service.",
            ));
        }
        sleep(Duration::from_millis(30)).await;
    }
}

fn web_url(url: &Url) -> io::Result<&str> {
    match url.scheme() {
        "http" | "https" => Ok(url.as_str()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Preview requires an HTTP or HTTPS URL.",
        )),
    }
}
