use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{MissedTickBehavior, interval, timeout_at};
use uuid::Uuid;

use super::super::{SharedEnvironment, bearer_token};
use super::{Preview, registry::normalize_url};

const MAX_INPUT: usize = 16 * 1024;
const FRAME_INTERVAL: Duration = Duration::from_millis(125);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct ViewerQuery {
    thread_id: String,
    tab_id: String,
    max_width: Option<u32>,
    max_height: Option<u32>,
    interactive: Option<bool>,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Input {
    Ack,
    TakeControl,
    ReleaseControl,
    Navigate {
        url: String,
    },
    History {
        delta: isize,
    },
    Reload {
        ignore_cache: Option<bool>,
    },
    Resize {
        width: u32,
        height: u32,
    },
    Text {
        text: String,
    },
    Mouse {
        action: MouseAction,
        x: f64,
        y: f64,
        button: MouseButton,
        buttons: u8,
        click_count: u32,
        modifiers: u8,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        modifiers: u8,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum MouseAction {
    Move,
    Down,
    Up,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum MouseButton {
    None,
    Left,
    Middle,
    Right,
}

pub(in crate::server) async fn upgrade(
    State(environment): State<SharedEnvironment>,
    Query(view): Query<ViewerQuery>,
    headers: HeaderMap,
    socket: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let token = bearer_token(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let (expires, permissions) = environment
        .credentials
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .session_permissions(token, Instant::now())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let preview = environment
        .local
        .as_ref()
        .and_then(|local| local.preview.clone())
        .ok_or(StatusCode::NOT_FOUND)?;
    if environment
        .store
        .terminal_workspace(&view.thread_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
        || preview.check_tab(&view.thread_id, &view.tab_id).is_err()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let can_operate = permissions.preview_operate && view.interactive.unwrap_or(true);
    Ok(socket
        .max_message_size(MAX_INPUT)
        .max_frame_size(MAX_INPUT)
        .on_upgrade(move |socket| async move {
            let viewer = Viewer {
                preview,
                view,
                can_operate,
                id: Uuid::new_v4(),
            };
            let _ = timeout_at(tokio::time::Instant::from_std(expires), viewer.run(socket)).await;
        }))
}

struct Viewer {
    preview: Arc<Preview>,
    view: ViewerQuery,
    can_operate: bool,
    id: Uuid,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.preview.release(&self.view.tab_id, self.id);
    }
}

impl Viewer {
    async fn run(&self, mut socket: WebSocket) {
        let thread = self.view.thread_id.as_str();
        let tab = self.view.tab_id.as_str();
        let mut size = (
            self.view.max_width.unwrap_or(1280).clamp(1, 3840),
            self.view.max_height.unwrap_or(800).clamp(1, 2160),
        );
        if !send_viewport(&mut socket, size).await {
            return;
        }
        let mut tick = interval(FRAME_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut controller = None;
        let mut generation = 0;
        let mut resized = !self.can_operate;
        let mut waiting_ack = false;
        let mut last_frame: Option<Vec<u8>> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if self.preview.check_tab(thread, tab).is_err() {
                        let _ = socket.send(Message::Close(Some(CloseFrame { code: 4404, reason: "tab closed".into() }))).await;
                        return;
                    }
                    let current = self.preview.controller(tab, self.id);
                    if controller != Some(current) {
                        controller = Some(current);
                        generation += 1;
                        if !send_json(&mut socket, json!({"type":"control","canOperate":self.can_operate,"controller":current,"generation":generation,"dialog":null})).await {
                            return;
                        }
                    }
                    if waiting_ack { continue; }
                    if !resized {
                        resized = self.preview.resize(thread, tab, size.0, size.1).await.is_ok();
                    }
                    if resized && let Ok(Some(frame)) = self.preview.frame(thread, tab).await
                        && last_frame.as_ref() != Some(&frame) {
                        if socket.send(Message::Binary(frame.clone().into())).await.is_err() { return; }
                        last_frame = Some(frame);
                        waiting_ack = true;
                    }
                }
                message = socket.recv() => {
                    let message = match message {
                        Some(Ok(Message::Text(message))) => message,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        _ => return,
                    };
                    let Ok(input)=serde_json::from_str::<Input>(message.as_str()) else { continue; };
                    match input {
                        Input::Ack => waiting_ack = false,
                        _ if !self.can_operate => (),
                        Input::TakeControl => { self.preview.claim(tab, self.id, true); }
                        Input::ReleaseControl => self.preview.release(tab, self.id),
                        Input::Resize { width, height } if self.preview.claim(tab, self.id, false) => {
                            size = (width.clamp(1,3840),height.clamp(1,2160));
                            resized = self.preview.resize(thread,tab,size.0,size.1).await.is_ok();
                            if !send_viewport(&mut socket, size).await { return; }
                        }
                        input if self.preview.claim(tab, self.id, false) => self.input(thread, tab, input).await,
                        _ => (),
                    }
                }
            }
        }
    }

    async fn input(&self, thread: &str, tab: &str, input: Input) {
        match input {
            Input::Navigate { url } => {
                if let Ok(url) =
                    normalize_url(&url).and_then(|url| Url::parse(&url).map_err(|_| Value::Null))
                {
                    let _ = self.preview.navigate(thread, tab, url).await;
                }
            }
            Input::History {
                delta: delta @ (-1 | 1),
            } => {
                let _ = self.preview.step_history(thread, tab, delta).await;
            }
            Input::Reload { ignore_cache } => {
                let _ = self
                    .preview
                    .reload(thread, tab, ignore_cache.unwrap_or_default())
                    .await;
            }
            Input::Text { text } => {
                let _ = self.preview.text(thread, tab, &text).await;
            }
            Input::Mouse {
                action,
                x,
                y,
                button,
                buttons,
                click_count,
                modifiers,
            } => {
                let action = match action {
                    MouseAction::Move => "mouseMoved",
                    MouseAction::Down => "mousePressed",
                    MouseAction::Up => "mouseReleased",
                };
                let _ = self.preview.mouse(thread, tab, json!({"type":action,"x":x,"y":y,"button":button,"buttons":buttons,"clickCount":click_count,"modifiers":modifiers})).await;
            }
            Input::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                let _ = self.preview.mouse(thread, tab, json!({"type":"mouseWheel","x":x,"y":y,"deltaX":delta_x,"deltaY":delta_y,"modifiers":modifiers})).await;
            }
            _ => (),
        }
    }
}

async fn send_viewport(socket: &mut WebSocket, (width, height): (u32, u32)) -> bool {
    send_json(
        socket,
        json!({"type":"viewport","width":width,"height":height}),
    )
    .await
}

async fn send_json(socket: &mut WebSocket, value: Value) -> bool {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .is_ok()
}
