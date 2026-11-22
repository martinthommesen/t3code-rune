use rune::{El, Key, TextEdit, TextStyle, Tx, col, row, text, text_input};

use crate::connection::WorktreeLocation;

use super::super::{CONTROL, DESCRIPTION, Palette, RESET, State, icon_button};
use super::workspace;

mod path_input;

const ROW: &str = "worktree-location-row";
const INPUT: &str = "worktree-location";
const RESET_LOCATION: &str = "reset-worktree-location";
const LABEL: &str = "Worktree location";
const DESCRIPTION_COPY: &str = concat!(
    "Folder where new worktrees are created, on any drive, such as D:\\worktrees or ~/worktrees. ",
    "Existing worktrees stay where they are. Leave empty to use the T3 home folder.",
);
const INPUT_TEXT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

#[derive(Default)]
pub(in crate::ui::settings) struct LocationPanel {
    saved: Option<String>,
    draft: TextEdit,
    previous: String,
    edited: bool,
    pending_reset: Option<uuid::Uuid>,
}

impl LocationPanel {
    pub(in crate::ui::settings) fn update(&mut self, setting: Option<&WorktreeLocation>) {
        let saved = setting.map(|setting| setting.path.as_str());
        if saved.is_none() {
            self.pending_reset = None;
        }
        if self.saved.as_deref() == saved {
            return;
        }
        self.saved = saved.map(str::to_owned);
        self.replace_draft(saved.unwrap_or_default().to_owned());
    }

    pub(in crate::ui::settings) fn settle_reset(&mut self, operation: uuid::Uuid, saved: bool) {
        if self.pending_reset == Some(operation) {
            self.pending_reset = None;
            if !saved {
                self.replace_draft(self.saved.clone().unwrap_or_default());
            }
        }
    }

    fn replace_draft(&mut self, value: String) {
        self.draft.replace_text(&value);
        self.previous = value;
        self.edited = false;
    }

    fn change(&mut self) {
        let next = path_input::normalize(&self.previous, &self.draft.text);
        if next != self.draft.text {
            self.draft.replace_text(&next);
        }
        self.previous = next;
        self.edited = true;
    }
}

pub(in crate::ui) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let panel = &mut state.settings.worktree_location;
    if !panel.edited
        || tx.focused() == Some(INPUT)
        || (tx.focused() == Some(RESET_LOCATION) && !tx.focus_visible())
    {
        return false;
    }
    panel.edited = false;
    let path = panel
        .draft
        .text
        .trim_matches(path_input::whitespace)
        .to_owned();
    let _ = workspace::save_worktree_location(state, path);
    true
}

pub(super) fn view(state: &State, palette: Palette, width: f32) -> Option<El> {
    let workspace = &state.settings.workspace;
    let setting = workspace.worktree_location()?;
    let writable = workspace.can_edit_worktree_location(state);
    let panel = &state.settings.worktree_location;
    let mut input = text_input(&panel.draft, INPUT_TEXT)
        .key(INPUT)
        .label(LABEL)
        .placeholder("Default", palette.muted)
        .single_line()
        .color(palette.ink)
        .h(30.0)
        .px(11.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent);
    if writable {
        input = input
            .on_edit(|state: &mut State| &mut state.settings.worktree_location.draft)
            .on_input_change(|state: &mut State, _| state.settings.worktree_location.change());
    } else {
        input = input.disabled(true).read_only(true);
        if let Some(organization) = setting.managed_by.as_deref() {
            input = input.help(format!("Managed by {organization}."));
        }
    }

    let mut title = row()
        .gap(6.0)
        .items_center()
        .child(text(LABEL).font(CONTROL).color(palette.ink));
    if writable && !setting.path.is_empty() {
        title = title.child(
            icon_button(
                RESET_LOCATION,
                "Reset worktree location to default",
                &RESET,
                12.0,
                20.0,
                palette,
                reset,
            )
            .help("Reset to default")
            .on_key(|state: &mut State, event, tx| {
                if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ")
                {
                    reset(state, tx);
                    true
                } else {
                    false
                }
            }),
        );
    }
    let label = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text(DESCRIPTION_COPY)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let content = if width >= 560.0 {
        row()
            .items_center()
            .gap(32.0)
            .child(label)
            .child(input.w(232.0))
    } else {
        col().gap(12.0).child(label).child(input.w_full())
    };
    Some(
        content
            .key(ROW)
            .focusable(true)
            .tab_index(-1)
            .no_focus_ring()
            .w_full()
            .px(16.0)
            .py(12.0),
    )
}

fn reset(state: &mut State, tx: &mut Tx) {
    if let Some(operation) = workspace::save_worktree_location(state, String::new()) {
        let panel = &mut state.settings.worktree_location;
        panel.pending_reset = Some(operation);
        panel.replace_draft(String::new());
        tx.focus(ROW, tx.focus_visible());
    }
}
