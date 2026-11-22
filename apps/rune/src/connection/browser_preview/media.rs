use std::io::Cursor;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use image::{DynamicImage, ImageDecoder, Limits};
use reqwest::Url;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};

use super::{BrowserFrame, BrowserTab, Event};

const MAX_FRAME: usize = 8 * 1024 * 1024;

pub(super) struct Viewer {
    tab: BrowserTab,
    output: mpsc::Receiver<Event>,
    input: mpsc::Sender<Value>,
    task: JoinHandle<()>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Viewer {
    pub(super) fn start(base: Url, token: &str, tab: BrowserTab, width: u32, height: u32) -> Self {
        let (output, updates) = mpsc::channel(1);
        let (input, actions) = mpsc::channel(32);
        let key = tab.clone();
        let token = token.to_owned();
        let task = tokio::spawn(async move {
            if run(base, token, key.clone(), width, height, &output, actions)
                .await
                .is_none()
            {
                let _ = output.send(Event::BrowserPreviewStreamLost(key)).await;
            }
        });
        Self {
            tab,
            output: updates,
            input,
            task,
        }
    }

    pub(super) fn tab(&self) -> &BrowserTab {
        &self.tab
    }

    pub(super) fn send(&self, payload: Value) {
        let _ = self.input.try_send(payload);
    }

    pub(super) async fn next(&mut self) -> Option<Event> {
        self.output.recv().await
    }
}

async fn run(
    base: Url,
    token: String,
    tab: BrowserTab,
    width: u32,
    height: u32,
    output: &mpsc::Sender<Event>,
    mut input: mpsc::Receiver<Value>,
) -> Option<()> {
    let mut url = base.join("/api/preview-stream/ws").ok()?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .ok()?;
    url.query_pairs_mut()
        .append_pair("threadId", &tab.thread_id)
        .append_pair("tabId", &tab.tab_id)
        .append_pair("maxWidth", &width.to_string())
        .append_pair("maxHeight", &height.to_string());
    let mut request = url.as_str().into_client_request().ok()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().ok()?);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let (mut socket, _) = timeout(
        Duration::from_secs(10),
        connect_async_with_config(request, Some(config), false),
    )
    .await
    .ok()?
    .ok()?;
    let mut accepted: Option<oneshot::Receiver<()>> = None;
    loop {
        tokio::select! {
            action=input.recv() => {
                let Some(action)=action else { return Some(()); };
                socket.send(Message::Text(action.to_string().into())).await.ok()?;
            }
            frame=socket.next() => {
                match frame?.ok()? {
                    Message::Binary(bytes) => {
                        let image=tokio::task::spawn_blocking(move||decode(bytes.as_ref())).await.ok().flatten();
                        if let Some((width,height,pixels))=image {
                            let (receipt,received)=oneshot::channel();
                            let frame=BrowserFrame{width,height,pixels:pixels.into(),accepted:receipt};
                            output.send(Event::BrowserPreviewFrame{tab:tab.clone(),frame}).await.ok()?;
                            accepted=Some(received);
                        } else {
                            socket.send(Message::Text(json!({"type":"ack"}).to_string().into())).await.ok()?;
                        }
                    }
                    Message::Close(_) => return Some(()),
                    _=> (),
                }
            }
            ()=async { let _=accepted.as_mut().expect("in-flight browser frame").await; }, if accepted.is_some() => {
                accepted=None;
                socket.send(Message::Text(json!({"type":"ack"}).to_string().into())).await.ok()?;
            }
        }
    }
}

fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes)).ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(3840);
    limits.max_image_height = Some(2160);
    limits.max_alloc = Some(96 * 1024 * 1024);
    decoder.set_limits(limits).ok()?;
    let raster = DynamicImage::from_decoder(decoder).ok()?.into_rgba8();
    Some((raster.width(), raster.height(), raster.into_raw()))
}
