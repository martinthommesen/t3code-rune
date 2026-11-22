use std::collections::HashMap;

use rune::{
    El, Icon, Image, Key, Phase, SurfaceFit, TextEdit, TextStyle, Tx, col, el, icon, row, text,
    text_input,
};
use serde_json::{Value, json};

use crate::client_settings::BrowserLinkTarget;
use crate::connection::{BrowserFrame, BrowserNavigation, BrowserSnapshot, BrowserTab};
use rune::edit_access;

use super::{Palette, ProjectView, State, conversation::Layout};

const ADDRESS: &str = "browser-preview-address";
const BODY: &str = "browser-preview-page";
const LABEL: TextStyle = TextStyle::new(13.0, 500.0).lh(18.0);

macro_rules! glyph {
    ($name:literal, $body:literal) => {
        Icon { name:$name,view:24.0,body:concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $body, "</g>") }
    };
}

const BACK: Icon = glyph!("arrow-left", r#"<path d="m12 19-7-7 7-7m-7 7h14"/>"#);
const FORWARD: Icon = glyph!("arrow-right", r#"<path d="m12 5 7 7-7 7m7-7H5"/>"#);
const REFRESH: Icon = glyph!("refresh-cw", r#"<path d="M20 7v6h-6m6-1a8 8 0 1 0-2 6"/>"#);
const CLOSE: Icon = glyph!("x", r#"<path d="m18 6-12 12m0-12 12 12"/>"#);
const GLOBE: Icon = glyph!(
    "globe",
    r#"<circle cx="12" cy="12" r="9"/><path d="M3 12h18m-9-9c-4 5-4 13 0 18 4-5 4-13 0-18z"/>"#
);
const EXTERNAL: Icon = glyph!(
    "external-link",
    r#"<path d="M13 5h6v6m0-6-8 8m5 0v5H6V8h5"/>"#
);

#[derive(Default)]
pub(super) struct Dock {
    threads: HashMap<String, Tabs>,
    pending: HashMap<BrowserTab, BrowserSnapshot>,
    address: TextEdit,
    viewport: Option<Observation>,
    watching: Option<Observation>,
}

#[derive(Default)]
struct Tabs {
    entries: Vec<Tab>,
    selected: usize,
}

struct Tab {
    snapshot: BrowserSnapshot,
    image: Option<BrowserImage>,
    stream_lost: bool,
}

struct BrowserImage {
    image: Image,
    size: (u32, u32),
}

#[derive(Clone, PartialEq, Eq)]
struct Observation {
    tab: BrowserTab,
    width: u32,
    height: u32,
}

impl State {
    pub(super) fn open_terminal_web_link(&mut self, thread: String, url: String, tx: &mut Tx) {
        let mods = tx.mods();
        let wants_preview = self.client_settings.browser_link_target() == BrowserLinkTarget::App
            && !mods.ctrl
            && !mods.logo
            && !mods.cmd;
        if wants_preview
            && self
                .connection
                .as_ref()
                .is_some_and(|connection| connection.open_browser_preview(thread, url.clone()))
        {
            return;
        }
        let _ = tx.open_url(&url);
    }

    pub(super) fn browser_preview_opened(
        &mut self,
        thread: &str,
        url: &str,
        snapshot: Option<BrowserSnapshot>,
        tx: &mut Tx,
    ) {
        let Some(initial) = snapshot else {
            let _ = tx.open_url(url);
            return;
        };
        let current = self
            .browser_preview
            .pending
            .remove(&initial.key)
            .unwrap_or(initial);
        let tabs = self
            .browser_preview
            .threads
            .entry(thread.to_owned())
            .or_default();
        tabs.entries.push(Tab {
            snapshot: current,
            image: None,
            stream_lost: false,
        });
        tabs.selected = tabs.entries.len() - 1;
        if selected_thread(self) == Some(thread) {
            self.browser_preview_sync_address();
        }
    }

    pub(super) fn browser_preview_updated(&mut self, snapshot: BrowserSnapshot, tx: &Tx) {
        let Some(tabs) = self
            .browser_preview
            .threads
            .get_mut(&snapshot.key.thread_id)
        else {
            self.browser_preview
                .pending
                .insert(snapshot.key.clone(), snapshot);
            return;
        };
        let Some(tab) = tabs
            .entries
            .iter_mut()
            .find(|tab| tab.snapshot.key == snapshot.key)
        else {
            self.browser_preview
                .pending
                .insert(snapshot.key.clone(), snapshot);
            return;
        };
        tab.snapshot = snapshot;
        if tx.focused() != Some(ADDRESS) {
            self.browser_preview_sync_address();
        }
    }

    pub(super) fn browser_preview_closed(&mut self, key: &BrowserTab) {
        self.browser_preview.pending.remove(key);
        let Some(tabs) = self.browser_preview.threads.get_mut(&key.thread_id) else {
            return;
        };
        let Some(index) = tabs.entries.iter().position(|tab| &tab.snapshot.key == key) else {
            return;
        };
        tabs.entries.remove(index);
        if index < tabs.selected {
            tabs.selected -= 1;
        }
        tabs.selected = tabs.selected.min(tabs.entries.len().saturating_sub(1));
        if tabs.entries.is_empty() {
            self.browser_preview.threads.remove(&key.thread_id);
        }
        self.browser_preview_sync_address();
    }

    pub(super) fn browser_preview_frame(&mut self, key: &BrowserTab, frame: BrowserFrame) {
        if let Some(cache) = self.image_cache.as_ref()
            && let Some(tab) = self
                .browser_preview
                .threads
                .get_mut(&key.thread_id)
                .and_then(|tabs| tabs.entries.iter_mut().find(|tab| &tab.snapshot.key == key))
        {
            tab.image = Some(BrowserImage {
                image: cache.rgba(frame.width, frame.height, frame.pixels),
                size: (frame.width, frame.height),
            });
            tab.stream_lost = false;
        }
        let _ = frame.accepted.send(());
    }

    pub(super) fn browser_preview_stream_lost(&mut self, key: &BrowserTab) {
        if let Some(tab) = self
            .browser_preview
            .threads
            .get_mut(&key.thread_id)
            .and_then(|tabs| tabs.entries.iter_mut().find(|tab| &tab.snapshot.key == key))
        {
            tab.stream_lost = true;
        }
    }

    pub(super) fn poll_browser_preview(&mut self) -> bool {
        let active = active_tab(self).map(|tab| tab.snapshot.key.clone());
        let observation = active.as_ref().map(|tab| {
            self.browser_preview
                .viewport
                .as_ref()
                .filter(|seen| &seen.tab == tab)
                .cloned()
                .unwrap_or_else(|| Observation {
                    tab: tab.clone(),
                    width: 483,
                    height: 700,
                })
        });
        if self.browser_preview.watching == observation {
            return false;
        }
        let scope_changed =
            self.browser_preview.watching.as_ref().map(|seen| &seen.tab) != active.as_ref();
        self.browser_preview.watching = observation.clone();
        if let Some(connection) = self.connection.as_ref() {
            if let Some(observation) = observation {
                connection.watch_browser_preview(
                    observation.tab,
                    observation.width,
                    observation.height,
                );
            } else {
                connection.hide_browser_preview();
            }
        }
        if scope_changed {
            self.browser_preview_sync_address();
        }
        scope_changed
    }

    fn browser_preview_sync_address(&mut self) {
        let url = active_tab(self)
            .map_or_else(String::new, |tab| tab.snapshot.nav_status.url().to_owned());
        self.browser_preview.address.replace_text(url);
    }

    fn browser_preview_input(&mut self, key: BrowserTab, action: Value) {
        if let Some(connection) = self.connection.as_ref() {
            connection.browser_input(key, action);
        }
    }

    fn browser_preview_navigate(&mut self, tx: &mut Tx) {
        if let Some(key) = active_tab(self).map(|tab| tab.snapshot.key.clone()) {
            let url = self.browser_preview.address.text.trim();
            if !url.is_empty() {
                self.browser_preview_input(key, json!({"type":"navigate","url":url}));
                tx.focus_when_shown(BODY, false);
            }
        }
    }
}

fn selected_thread(state: &State) -> Option<&str> {
    match &state.workspace.as_ref()?.selection.as_ref()?.view {
        ProjectView::Conversation(conversation) => Some(&conversation.id),
        _ => None,
    }
}

fn active_tabs(state: &State) -> Option<&Tabs> {
    state.browser_preview.threads.get(selected_thread(state)?)
}

fn active_tab(state: &State) -> Option<&Tab> {
    let tabs = active_tabs(state)?;
    tabs.entries.get(tabs.selected)
}

pub(super) fn visible(state: &State) -> bool {
    active_tab(state).is_some()
}

pub(super) fn width(window: f32, workspace: f32) -> f32 {
    (window * 0.44).floor().min((workspace - 170.0).max(0.0))
}

pub(super) fn view(state: &State, layout: Layout, palette: Palette) -> Option<El> {
    let tabs = active_tabs(state)?;
    let tab = tabs.entries.get(tabs.selected)?;
    let key = &tab.snapshot.key;
    let mut tab_strip = row()
        .key("browser-preview-tabs")
        .w_full()
        .h(layout.header_height.max(40.0))
        .gap(3.0)
        .px(8.0)
        .center_y();
    for (index, tab) in tabs.entries.iter().enumerate() {
        let title = display_title(&tab.snapshot.nav_status);
        let tab_key = tab.snapshot.key.clone();
        let thread = tab_key.thread_id.clone();
        let select = row()
            .key(format!("select-preview-{}", tab_key.tab_id))
            .role("tab")
            .label(&title)
            .h(26.0)
            .min_w(0.0)
            .max_w(163.0)
            .center_y()
            .gap(6.0)
            .px(6.0)
            .radius(6.0)
            .focusable(true)
            .hover(palette.accent)
            .focus_selected(index == tabs.selected)
            .on_press(move |state: &mut State| {
                if let Some(tabs) = state.browser_preview.threads.get_mut(&thread)
                    && index < tabs.entries.len()
                {
                    tabs.selected = index;
                }
                state.browser_preview_sync_address();
            })
            .child(icon(&GLOBE, 14.0).color(palette.ink))
            .child(text(&title).font(LABEL).color(palette.ink).truncate());
        let close = tool(
            &format!("Close {title}"),
            &CLOSE,
            palette,
            true,
            move |state, _| {
                if let Some(connection) = state.connection.as_ref() {
                    connection.close_browser_preview(tab_key.clone());
                }
            },
        );
        tab_strip = tab_strip.child(
            row()
                .min_w(0.0)
                .h(26.0)
                .fill(if index == tabs.selected {
                    palette.accent
                } else {
                    palette.canvas
                })
                .radius(6.0)
                .child(select)
                .child(close),
        );
    }
    let back = key.clone();
    let forward = key.clone();
    let refresh = key.clone();
    let url = tab.snapshot.nav_status.url().to_owned();
    let toolbar = row()
        .key("browser-preview-toolbar")
        .label("Navigation")
        .w_full()
        .h(40.0)
        .px(7.0)
        .gap(4.0)
        .center_y()
        .child(tool(
            "Back",
            &BACK,
            palette,
            tab.snapshot.can_go_back,
            move |state, _| {
                state.browser_preview_input(back.clone(), json!({"type":"history","delta":-1}))
            },
        ))
        .child(tool(
            "Forward",
            &FORWARD,
            palette,
            tab.snapshot.can_go_forward,
            move |state, _| {
                state.browser_preview_input(forward.clone(), json!({"type":"history","delta":1}))
            },
        ))
        .child(tool("Refresh", &REFRESH, palette, true, move |state, _| {
            state.browser_preview_input(refresh.clone(), json!({"type":"reload"}))
        }))
        .child(
            text_input(&state.browser_preview.address, LABEL)
                .key(ADDRESS)
                .label("Search or enter URL")
                .placeholder("Search or enter URL", palette.muted)
                .color(palette.ink)
                .w_full()
                .h(28.0)
                .single_line()
                .grow(1.0)
                .min_w(0.0)
                .on_edit(edit_access(|state: &mut State| {
                    &mut state.browser_preview.address
                }))
                .on_input_key(|state: &mut State, event, tx| {
                    if event.key == Key::Enter {
                        state.browser_preview_navigate(tx);
                        true
                    } else if event.key == Key::Escape {
                        state.browser_preview_sync_address();
                        tx.focus_when_shown(BODY, false);
                        true
                    } else {
                        false
                    }
                }),
        )
        .child(tool(
            "Open in system browser",
            &EXTERNAL,
            palette,
            !url.is_empty(),
            move |_, tx| {
                let _ = tx.open_url(&url);
            },
        ));
    let layout_key = key.clone();
    let mut page = el()
        .key(BODY)
        .role("group")
        .label("Browser page")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .focusable(true)
        .on_layout(move |state: &mut State, rect| {
            state.browser_preview.viewport = Some(Observation {
                tab: layout_key.clone(),
                width: rect[2].round().max(1.0) as u32,
                height: rect[3].round().max(1.0) as u32,
            });
        });
    if let BrowserNavigation::LoadFailed { description, .. } = &tab.snapshot.nav_status {
        page = page.child(
            col()
                .padding(16.0)
                .gap(8.0)
                .child(
                    text("This page could not be loaded")
                        .font(LABEL)
                        .color(palette.ink),
                )
                .child(text(description).font(LABEL).color(palette.muted)),
        );
    } else if let Some(frame) = &tab.image {
        let pointer = key.clone();
        let keyboard = key.clone();
        let size = frame.size;
        page=page.pixels_fit(frame.image.pixels(),SurfaceFit::Fit)
            .on_drag(move|state:&mut State,drag,tx| {
                let (x,y)=((drag.x/drag.w.max(1.0))*size.0 as f32,(drag.y/drag.h.max(1.0))*size.1 as f32);
                let action=match drag.phase {Phase::Down=>"down",Phase::Move=>"move",Phase::Up=>"up"};
                let mods = tx.mods();
                let bits = u8::from(mods.alt)
                    | (u8::from(mods.ctrl) << 1)
                    | (u8::from(mods.logo) << 2)
                    | (u8::from(mods.shift) << 3);
                state.browser_preview_input(pointer.clone(),json!({"type":"mouse","action":action,"x":x,"y":y,"button":"left","buttons":u8::from(drag.phase!=Phase::Up),"clickCount":drag.clicks,"modifiers":bits}));
            })
            .on_key(move|state:&mut State,event,_| {
                if let Key::Text(value)=&event.key {state.browser_preview_input(keyboard.clone(),json!({"type":"text","text":value}));true} else {false}
            });
    } else {
        let message = if tab.stream_lost {
            "The Preview connection closed."
        } else {
            "Loading…"
        };
        page = page.child(
            col()
                .padding(16.0)
                .child(text(message).font(LABEL).color(palette.muted)),
        );
    }
    Some(
        col()
            .key("browser-preview-dock")
            .role("region")
            .label("Browser Preview")
            .w(layout.file_preview_width)
            .h_full()
            .min_h(0.0)
            .fill(palette.canvas)
            .border_sides([0.0, 0.0, 0.0, 1.0], palette.accent)
            .child(tab_strip)
            .child(toolbar)
            .child(page),
    )
}

fn display_title(navigation: &BrowserNavigation) -> String {
    let title = navigation.title();
    if title.is_empty() {
        return "New tab".into();
    }
    if title == navigation.url()
        && let Ok(url) = reqwest::Url::parse(title)
        && let Some(host) = url.host_str()
    {
        return url
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
    }
    title.to_owned()
}

fn tool(
    label: &str,
    graphic: &'static Icon,
    palette: Palette,
    enabled: bool,
    click: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    el().role("button")
        .label(label)
        .size(24.0, 24.0)
        .center()
        .radius(4.0)
        .disabled(!enabled)
        .focusable(true)
        .hover(palette.accent)
        .on_press_tx(click)
        .child(icon(graphic, 14.0).color(if enabled { palette.ink } else { palette.muted }))
}
