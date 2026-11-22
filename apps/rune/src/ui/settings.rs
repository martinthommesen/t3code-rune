use rune::{Color, Cx, El, Icon, Key, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};

use super::{Palette, State, error_text, sidebar_header};

mod appearance;
mod keybinding_catalog;
mod project;
mod workspace;
mod worktrees;

pub(super) use project::{
    gear as project_settings_gear, poll as poll_project, settle as settle_project_edit,
};
pub(super) use project::{poll_files as poll_project_files, settle_files as settle_project_files};

const CONTROL: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const SECTION: TextStyle = TextStyle::new(16.0, 600.0).lh(24.0);
const DESCRIPTION: TextStyle = TextStyle::new(12.0, 400.0).lh(18.0);
const DIALOG: &str = "client-settings-dialog";

macro_rules! glyph {
    ($name:literal, $path:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#, $path, "</g>"),
        }
    };
}

mod archive;
mod keybindings;
pub(super) mod path_input;
pub(super) mod search;
pub(super) use archive::settle as settle_archive_action;
pub(super) use workspace::project_start::{
    poll as poll_add_project_base_directory, warning as add_project_base_scope_warning,
};
pub(super) use workspace::snooze::poll as poll_snooze_options;
pub(super) use worktrees::{poll_branch_fields, poll_location as poll_worktree_location};

static GEAR: Icon = glyph!(
    "settings",
    r#"<path d="M12.2 2h-.4a2 2 0 0 0-2 2v.2a2 2 0 0 1-1 1.7l-.4.2a2 2 0 0 1-2 0l-.1-.1a2 2 0 0 0-2.7.7l-.2.4a2 2 0 0 0 .7 2.7l.2.1a2 2 0 0 1 1 1.7v.4a2 2 0 0 1-1 1.8l-.2.1a2 2 0 0 0-.7 2.7l.2.4a2 2 0 0 0 2.7.7l.1-.1a2 2 0 0 1 2 0l.4.2a2 2 0 0 1 1 1.7v.2a2 2 0 0 0 2 2h.4a2 2 0 0 0 2-2v-.2a2 2 0 0 1 1-1.7l.4-.2a2 2 0 0 1 2 0l.1.1a2 2 0 0 0 2.7-.7l.2-.4a2 2 0 0 0-.7-2.7l-.2-.1a2 2 0 0 1-1-1.8v-.4a2 2 0 0 1 1-1.7l.2-.1a2 2 0 0 0 .7-2.7l-.2-.4a2 2 0 0 0-2.7-.7l-.1.1a2 2 0 0 1-2 0l-.4-.2a2 2 0 0 1-1-1.7V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>"#
);
static GENERAL: Icon = glyph!(
    "settings-2",
    r#"<path d="M20 7h-9M14 17H5"/><circle cx="7" cy="7" r="3"/><circle cx="17" cy="17" r="3"/>"#
);
static APPEARANCE: Icon = glyph!(
    "palette",
    r#"<path d="M12 3a9 9 0 1 0 0 18h1.5a1.5 1.5 0 0 0 0-3H13a1 1 0 0 1 0-2h2a6 6 0 0 0 0-12z"/><circle cx="7.5" cy="12" r=".5"/><circle cx="9" cy="8" r=".5"/><circle cx="13" cy="7.5" r=".5"/><circle cx="16.5" cy="10" r=".5"/>"#
);
static KEYBOARD: Icon = glyph!(
    "keyboard",
    r#"<rect x="2" y="5" width="20" height="14" rx="2"/><path d="M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 13h.01M10 13h.01M14 13h.01M18 13h.01M8 16h8"/>"#
);
static THREADS: Icon = glyph!(
    "messages-square",
    r#"<path d="M14 9a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2v7l3-3h7a2 2 0 0 0 2-2zM22 17a2 2 0 0 0-2-2h-6a2 2 0 0 0-2 2v2a2 2 0 0 0 2 2h5l3 3z"/>"#
);
static WORKTREES: Icon = glyph!(
    "git-fork",
    r#"<circle cx="12" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><circle cx="18" cy="6" r="3"/><path d="M18 9v2a2 2 0 0 1-2 2h-4m-6-4v2a2 2 0 0 0 2 2h4m0 2v-2"/>"#
);
static CLOSE: Icon = glyph!("x", r#"<path d="m18 6-12 12M6 6l12 12"/>"#);
static CHEVRON: Icon = glyph!("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static RESET: Icon = glyph!(
    "undo-2",
    r#"<path d="m9 14-5-5 5-5M4 9h10.5a5.5 5.5 0 1 1 0 11H11"/>"#
);

pub(super) struct Panel {
    open: bool,
    pub(super) archive: archive::Panel,
    error: Option<String>,
    page: Page,
    project: Option<project::Form>,
    workspace: workspace::Panel,
    worktree_location: worktrees::LocationPanel,
    branch_fields: worktrees::FieldsPanel,
    keybindings: keybindings::Panel,
    return_focus: &'static str,
}

impl Panel {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            open: false,
            archive: archive::Panel::default(),
            error: None,
            page: Page::General,
            project: None,
            workspace: workspace::Panel::default(),
            worktree_location: worktrees::LocationPanel::default(),
            branch_fields: worktrees::FieldsPanel::default(),
            keybindings: keybindings::Panel::default(),
            return_focus: "open-client-settings",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    General,
    Appearance,
    Keybindings,
    Threads,
    Projects,
    Worktrees,
    Archive,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Keybindings => "Keybindings",
            Self::Threads => "Threads",
            Self::Projects => "Projects",
            Self::Worktrees => "Worktrees",
            Self::Archive => "Archive",
        }
    }
}

impl State {
    pub(super) fn open_client_settings(&mut self, tx: &mut Tx) {
        self.settings.open = true;
        self.settings.page = Page::General;
        self.settings.return_focus = "open-client-settings";
        tx.focus(DIALOG, false);
    }

    pub(super) fn close_client_settings(&mut self, tx: &mut Tx) {
        self.save_project_name();
        self.settings.open = false;
        self.settings.keybindings.clear_view();
        self.settings.archive.leave();
        self.settings.workspace.close_popups();
        self.settings.error = None;
        tx.focus(self.settings.return_focus, tx.focus_visible());
    }

    fn toggle_terminal_close_confirmation(&mut self, _: &mut Tx) {
        self.set_terminal_close_confirmation(!self.client_settings.confirm_terminal_close());
    }

    fn reset_terminal_close_confirmation(&mut self, tx: &mut Tx) {
        self.set_terminal_close_confirmation(true);
        tx.focus("confirm-terminal-closing", tx.focus_visible());
    }

    fn set_terminal_close_confirmation(&mut self, enabled: bool) {
        self.settings.error = self
            .client_settings
            .set_confirm_terminal_close(enabled)
            .err()
            .map(|error| format!("Could not save client settings. {error}"));
    }

    fn set_thread_delete_confirmation(&mut self, enabled: bool) {
        self.settings.error = self
            .client_settings
            .set_confirm_thread_delete(enabled)
            .err()
            .map(|error| format!("Could not save client settings. {error}"));
    }

    fn select_settings_page(&mut self, page: Page) {
        self.save_project_name();
        if self.settings.page == Page::Keybindings && page != Page::Keybindings {
            self.settings.keybindings.clear_view();
        }
        self.settings.page = page;
        if page != Page::Archive {
            self.settings.archive.leave();
        }
        self.settings.workspace.close_popups();
        self.ensure_open_workspace_settings();
        self.settings.error = None;
    }

    pub(super) fn set_workspace_configuration(
        &mut self,
        setting: Option<crate::connection::WorkspaceSetting>,
    ) {
        self.settings.worktree_location.update(
            setting
                .as_ref()
                .and_then(|settings| settings.worktree_location.as_ref()),
        );
        self.settings.workspace.snooze.apply(
            setting
                .as_ref()
                .and_then(|settings| settings.snooze.as_ref()),
        );
        self.settings.workspace.project_start.apply(
            setting
                .as_ref()
                .and_then(|settings| settings.add_project_base.as_ref()),
        );
        self.settings.workspace.current = setting;
    }

    pub(super) fn settle_workspace_setting(
        &mut self,
        operation: uuid::Uuid,
        result: Result<(), String>,
    ) {
        self.settings
            .worktree_location
            .settle_reset(operation, result.is_ok());
        self.settings
            .branch_fields
            .settle(operation, result.is_ok());
        self.settings
            .workspace
            .snooze
            .settle(operation, result.is_ok());
        self.settings
            .workspace
            .project_start
            .settle(operation, result.is_ok());
        self.settings.workspace.settle(operation, result);
    }

    pub(super) fn configured_project_start_directory(&self) -> Option<&str> {
        Some(
            self.settings
                .workspace
                .current
                .as_ref()?
                .add_project_base
                .as_ref()?
                .path
                .as_str(),
        )
    }

    pub(super) fn workspace_settings_disconnected(&mut self, final_disconnect: bool) {
        self.settings.worktree_location.update(None);
        if final_disconnect {
            self.settings.workspace.stop();
        } else {
            self.settings.workspace.disconnect();
        }
    }

    pub(super) fn set_workspace_project_file(
        &mut self,
        operation: uuid::Uuid,
        mode: Option<crate::connection::WorkspaceMode>,
    ) {
        self.settings.workspace.apply_project_file(operation, mode);
    }

    pub(super) fn ensure_open_workspace_settings(&mut self) {
        if self.settings.open
            && matches!(self.settings.page, Page::Threads | Page::Worktrees)
            && let Some(connection) = &self.connection
        {
            connection.ensure_workspace_settings();
            workspace::Panel::refresh_project_file(self);
        }
    }
}

pub(super) fn opener(palette: Palette) -> El {
    icon_button(
        "open-client-settings",
        "Settings",
        &GEAR,
        16.0,
        32.0,
        palette,
        State::open_client_settings,
    )
    .on_key(|state: &mut State, event, tx| {
        sidebar_header::activate_on_return(state, event, tx, State::open_client_settings)
    })
}

pub(super) fn overlay(
    state: &State,
    palette: Palette,
    width: f32,
    height: f32,
    cx: &Cx<State>,
) -> Option<El> {
    if !state.settings.open {
        return None;
    }
    let canvas = if palette.dark {
        Color::BLACK
    } else {
        rune::color::hex("#fcfcfc").expect("static settings background")
    };
    let backdrop = canvas.alpha(if palette.dark { 0.64 } else { 0.60 });
    let palette = Palette { canvas, ..palette };
    let inset = if width >= 1024.0 {
        40.0
    } else if width >= 640.0 {
        24.0
    } else {
        0.0
    };
    let navigation_width = if width >= 768.0 { 240.0 } else { 56.0 };
    let content_width = (width - 2.0 * inset).min(1280.0) - navigation_width;
    let radius = if width >= 640.0 { 16.0 } else { 0.0 };
    let color = sidebar_header::Colors::new(palette.dark);
    let page = state.settings.page;
    let content = match page {
        Page::General => general(state, palette, content_width, width < 640.0),
        Page::Appearance => appearance::page(state, palette, content_width),
        Page::Keybindings => keybindings::page(state, palette, content_width),
        Page::Threads => workspace::page(state, palette, content_width),
        Page::Worktrees => worktrees::page(state, palette, content_width, width < 640.0),
        Page::Projects => project::page(state, palette, content_width),
        Page::Archive => archive::page(state, palette, content_width),
    };
    let picker = project::icon_overlay(state, palette, width)
        .or_else(|| project::file_overlay(state, palette, width, height))
        .or_else(|| archive::confirmation(state, palette));
    let dialog = row()
        .key(DIALOG)
        .hit()
        .role("dialog")
        .label("Settings")
        .focusable(true)
        .w_full()
        .h_full()
        .max_w(1280.0)
        .max_h(960.0)
        .radius(radius)
        .clip_rounded(radius)
        .fill(palette.canvas)
        .border(1.0, color.border)
        .border_layout()
        .on_key(|state: &mut State, event, tx| {
            if event.key == Key::Escape {
                if state.settings.page != Page::Archive || !state.settings.archive.dismiss_scope(tx)
                {
                    state.close_client_settings(tx);
                }
                return true;
            }
            false
        })
        .child(navigation(page, navigation_width, color))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .child(header(page, palette, color.border))
                .child(content),
        );
    let dialog = if picker.is_some() {
        dialog.inert()
    } else {
        dialog
    };
    let mut overlay = col()
        .key("client-settings-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .padding(inset)
        .center()
        .child(
            el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(backdrop)
                .on_press_tx(State::close_client_settings),
        )
        .child(dialog);
    if let Some(picker) = picker {
        overlay = overlay.child(picker);
    }
    if let Some(items) =
        workspace::overlay(state, palette).or_else(|| worktrees::overlay(state, palette))
    {
        overlay = overlay.children(items);
    }
    if page == Page::Keybindings {
        overlay = overlay.children(keybindings::when_overlays(
            state,
            palette,
            content_width,
            height,
            cx,
        ));
    }
    Some(overlay)
}

fn navigation(active: Page, width: f32, colors: sidebar_header::Colors) -> El {
    let mut navigation = col()
        .role("navigation")
        .label("Settings navigation")
        .w(width)
        .min_w(width)
        .h_full()
        .padding(8.0)
        .gap(4.0)
        .fill(colors.background)
        .border_sides([0.0, 1.0, 0.0, 0.0], colors.border);
    for (page, glyph, key) in [
        (Page::General, &GENERAL, "settings-general"),
        (Page::Appearance, &APPEARANCE, "settings-appearance"),
        (Page::Keybindings, &KEYBOARD, "settings-keybindings"),
        (Page::Threads, &THREADS, "settings-threads"),
        (Page::Projects, &GEAR, "settings-projects"),
        (Page::Worktrees, &WORKTREES, "settings-worktrees"),
        (Page::Archive, &archive::ARCHIVE, "settings-archive"),
    ] {
        let selected = active == page;
        let ink = if selected {
            colors.foreground
        } else {
            colors.muted
        };
        let mut button = row()
            .key(key)
            .role("button")
            .label(page.label())
            .checked(selected)
            .focusable(true)
            .focus_ring(1.0, 1.0, colors.ring)
            .h(32.0)
            .w_full()
            .items_center()
            .px(8.0)
            .gap(8.0)
            .radius(6.0)
            .fill(if selected {
                colors.hover
            } else {
                Color::TRANSPARENT
            })
            .hover(colors.hover)
            .child(icon(glyph, 16.0).color(ink).a11y_hidden());
        let select = move |state: &mut State, _: &mut Tx| state.select_settings_page(page);
        button = if page == Page::Archive {
            archive::press(button, select)
        } else if page == Page::Keybindings {
            button
                .on_press_tx(select)
                .on_key(move |state: &mut State, event, tx| {
                    sidebar_header::activate_on_return(state, event, tx, select)
                })
        } else {
            button.on_press_tx(select)
        };
        if width > 56.0 {
            button = button.child(text(page.label()).font(CONTROL).color(ink));
        }
        navigation = navigation.child(button);
    }
    navigation
}

fn header(page: Page, palette: Palette, border: Color) -> El {
    row()
        .w_full()
        .h(48.0)
        .items_center()
        .gap(8.0)
        .pl(20.0)
        .pr(8.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], border)
        .child(text("Settings").font(CONTROL).color(palette.muted))
        .child(icon(&CHEVRON, 14.0).color(palette.muted).a11y_hidden())
        .child(
            el().grow(1.0)
                .child(text(page.label()).font(CONTROL).color(palette.ink)),
        )
        .child(icon_button(
            "close-client-settings",
            "Close",
            &CLOSE,
            16.0,
            32.0,
            palette,
            State::close_client_settings,
        ))
}

fn general(state: &State, palette: Palette, width: f32, large_touch_switch: bool) -> El {
    let terminal_confirmation = state.client_settings.confirm_terminal_close();
    let thread_confirmation = state.client_settings.confirm_thread_delete();
    let delete_switch = switch_track(
        thread_confirmation,
        large_touch_switch,
        palette,
        palette.accent,
    )
    .key("confirm-thread-deletion")
    .label("Confirm thread deletion")
    .focusable(true)
    .on_press_tx(|state: &mut State, _| {
        state.set_thread_delete_confirmation(!state.client_settings.confirm_thread_delete());
    });
    let delete_reset = (!thread_confirmation).then(|| {
        icon_button(
            "reset-delete-confirmation",
            "Reset delete confirmation to default",
            &RESET,
            12.0,
            20.0,
            palette,
            |state, tx| {
                state.set_thread_delete_confirmation(true);
                tx.focus("confirm-thread-deletion", tx.focus_visible());
            },
        )
    });
    let terminal_reset = (!terminal_confirmation).then(|| {
        icon_button(
            "reset-terminal-close-confirmation",
            "Reset terminal close confirmation to default",
            &RESET,
            12.0,
            20.0,
            palette,
            State::reset_terminal_close_confirmation,
        )
    });
    let mut group = col()
        .w_full()
        .gap(10.0)
        .child(
            text("Confirmations")
                .font(SECTION)
                .color(palette.ink)
                .role("heading")
                .px(16.0),
        )
        .child(
            col()
                .w_full()
                .radius(12.0)
                .border(1.0, palette.line)
                .child(confirmation_row(
                    palette,
                    width,
                    "delete-confirmation",
                    "Delete confirmation",
                    "Ask before deleting a thread and its chat history.",
                    delete_switch,
                    delete_reset,
                ))
                .child(el().w_full().h(1.0).fill(palette.line))
                .child(confirmation_row(
                    palette,
                    width,
                    "terminal-close-confirmation",
                    "Terminal close confirmation",
                    "Ask before closing terminals and stopping their running processes.",
                    confirmation_switch(terminal_confirmation, large_touch_switch, palette),
                    terminal_reset,
                )),
        );
    if let Some(error) = &state.settings.error {
        group = group.child(error_text(error, palette));
    }
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .padding(24.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(group.max_w(800.0))
}

fn confirmation_row(
    palette: Palette,
    width: f32,
    key: &str,
    title: &str,
    description: &str,
    switch: El,
    reset: Option<El>,
) -> El {
    let mut heading = row()
        .items_center()
        .gap(6.0)
        .child(text(title).font(CONTROL).color(palette.ink));
    if let Some(reset) = reset {
        heading = heading.child(reset);
    }
    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(heading).child(
        text(description)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let control = if width >= 560.0 {
        row().items_center().gap(32.0).child(label).child(switch)
    } else {
        col().gap(12.0).child(label).child(switch)
    };
    control
        .key(key)
        .focusable(true)
        .tab_index(-1)
        .no_focus_ring()
        .w_full()
        .px(16.0)
        .py(12.0)
}

fn confirmation_switch(enabled: bool, large: bool, palette: Palette) -> El {
    switch_track(enabled, large, palette, palette.accent)
        .key("confirm-terminal-closing")
        .label("Confirm terminal closing")
        .focusable(true)
        .on_press_tx(State::toggle_terminal_close_confirmation)
}

fn switch_track(enabled: bool, large: bool, palette: Palette, active: Color) -> El {
    let (width, height, thumb) = if large {
        (38.0, 22.0, 18.0)
    } else {
        (30.0, 18.0, 14.0)
    };
    let neutral = rune::color::hex(if palette.dark { "#2e2e2e" } else { "#d4d4d8" })
        .expect("static switch palette");
    el().role("switch")
        .checked(enabled)
        .focus_ring(2.0, 1.0, palette.accent)
        .w(width)
        .h(height)
        .radius(height * 0.5)
        .fill(if enabled { active } else { neutral })
        .child(
            el().absolute(if enabled { width - thumb - 2.0 } else { 2.0 }, 2.0)
                .w(thumb)
                .h(thumb)
                .radius(thumb * 0.5)
                .fill(if !enabled && palette.dark {
                    palette.muted
                } else {
                    palette.canvas
                }),
        )
}

fn icon_button(
    key: &str,
    label: &str,
    glyph: &'static Icon,
    icon_size: f32,
    size: f32,
    palette: Palette,
    press: fn(&mut State, &mut Tx),
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .w(size)
        .h(size)
        .radius(6.0)
        .center()
        .focusable(true)
        .focus_ring(1.0, 2.0, palette.accent)
        .hover(palette.hover)
        .on_press_tx(press)
        .child(icon(glyph, icon_size).color(palette.muted).a11y_hidden())
}
