use alacritty_terminal::grid::{Dimensions, Scroll};
use rune::color::hex;
use rune::{
    El, Family, Key, PointerPress, PopupItem, PopupMenu, TextStyle, Tx, col, popup_menu_supported,
    row, text,
};

use crate::composer_context::TerminalExcerpt;

use super::{Pane, State, TerminalStatus, selection};

const WIDTH: f32 = 133.0;
const HEIGHT: f32 = 168.0;
const FONT: TextStyle = TextStyle::new(14.0, 400.0)
    .lh(20.0)
    .family(Family::Named("Cantarell, DejaVu Sans"));

#[derive(Clone, Copy)]
enum Action {
    Add,
    Copy,
    Paste,
    SelectAll,
    JumpToLatest,
}

const ACTIONS: [Action; 5] = [
    Action::Add,
    Action::Copy,
    Action::Paste,
    Action::SelectAll,
    Action::JumpToLatest,
];

impl Action {
    fn key(self) -> &'static str {
        match self {
            Self::Add => "terminal-add-to-chat",
            Self::Copy => "terminal-copy",
            Self::Paste => "terminal-paste",
            Self::SelectAll => "terminal-select-all",
            Self::JumpToLatest => "terminal-jump-to-latest",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Add => "Add to chat",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::SelectAll => "Select all",
            Self::JumpToLatest => "Jump to latest",
        }
    }

    fn enabled(self, menu: &Menu) -> bool {
        match self {
            Self::Add => menu.can_add && menu.selected.is_some(),
            Self::Copy => menu.selected.is_some(),
            Self::Paste => menu.can_paste,
            Self::SelectAll | Self::JumpToLatest => true,
        }
    }
}

pub(super) struct Menu {
    point: PointerPress,
    selected: Option<String>,
    can_add: bool,
    can_paste: bool,
}

impl State {
    pub(super) fn open_terminal_menu(&mut self, attachment: u64, point: PointerPress, tx: &mut Tx) {
        self.activate_terminal_attachment(attachment, tx);
        let Some(pane) = self
            .terminal
            .as_ref()
            .filter(|pane| pane.attachment == attachment)
        else {
            return;
        };
        let writable = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.reconnecting);
        let menu = Menu {
            point,
            selected: selection::selected_text(pane),
            can_add: writable && self.drafts.contains_key(&pane.thread_id),
            can_paste: writable && pane.status == TerminalStatus::Running,
        };
        if popup_menu_supported() {
            let elements = PopupMenu::new().items(ACTIONS.map(|action| {
                PopupItem::new(action.key(), action.label()).enabled(action.enabled(&menu))
            }));
            tx.popup_menu(elements, point, move |state: &mut State, picked, tx| {
                if let Some(action) = ACTIONS
                    .into_iter()
                    .find(|action| Some(action.key()) == picked)
                    && action.enabled(&menu)
                {
                    state.run_terminal_menu(attachment, action, menu.selected, tx);
                }
            });
        } else {
            if let Some(action) = ACTIONS.into_iter().find(|action| action.enabled(&menu)) {
                tx.focus_when_shown(action.key(), tx.by_keys());
            }
            if let Some(pane) = &mut self.terminal {
                pane.menu = Some(menu);
            }
        }
    }

    fn close_terminal_menu(&mut self, tx: &mut Tx) {
        if let Some(pane) = &mut self.terminal {
            pane.menu = None;
            tx.focus_when_shown(super::INPUT_KEY, tx.by_keys());
        }
    }

    fn pick_terminal_menu(&mut self, action: Action, tx: &mut Tx) {
        let Some(pane) = &mut self.terminal else {
            return;
        };
        let attachment = pane.attachment;
        let Some(menu) = pane.menu.take() else {
            return;
        };
        if action.enabled(&menu) {
            self.run_terminal_menu(attachment, action, menu.selected, tx);
        }
    }

    fn run_terminal_menu(
        &mut self,
        attachment: u64,
        action: Action,
        copied: Option<String>,
        tx: &mut Tx,
    ) {
        if self
            .terminal
            .as_ref()
            .is_none_or(|pane| pane.attachment != attachment)
        {
            return;
        }
        tx.focus_when_shown(super::INPUT_KEY, tx.by_keys());
        match action {
            Action::Copy => {
                if let Some(text) = copied {
                    tx.copy(text);
                }
            }
            Action::Paste => tx.read_clipboard(move |state: &mut State, content, _| {
                let text = content.text();
                if !text.is_empty() {
                    state.paste_terminal_at(attachment, text);
                }
            }),
            Action::SelectAll => {
                if let Some(pane) = &mut self.terminal {
                    selection::select_all(pane);
                }
            }
            Action::JumpToLatest => {
                self.scroll_history(attachment, Scroll::Bottom);
            }
            Action::Add => {
                if let Some(text) = copied {
                    self.add_terminal_excerpt(attachment, &text, tx);
                }
            }
        }
    }

    fn add_terminal_excerpt(&mut self, attachment: u64, captured: &str, tx: &mut Tx) {
        let Some(pane) = self
            .terminal
            .as_mut()
            .filter(|pane| pane.attachment == attachment)
        else {
            return;
        };
        if selection::selected_text(pane).as_deref() != Some(captured) {
            return;
        }
        let term = &mut pane.screen.term;
        let Some(range) = term.renderable_content().selection else {
            return;
        };
        let label = self
            .terminal_tabs
            .get(&pane.thread_id)
            .and_then(|tabs| tabs.tab(&pane.terminal_id))
            .map_or_else(|| pane.label.clone(), |tab| tab.label());
        let history = term.history_size() as i32;
        let line_number = |line: i32| (history + line + 1).max(1) as usize;
        let Some(excerpt) = TerminalExcerpt::new(
            pane.terminal_id.clone(),
            label,
            line_number(range.start.line.0),
            line_number(range.end.line.0),
            captured,
        ) else {
            return;
        };
        let thread = pane.thread_id.clone();
        term.selection = None;
        if self
            .questions
            .get(&thread)
            .is_some_and(|questions| questions.has_active_question())
        {
            self.project_notice = Some("Terminal output can't be added to a question answer. Answer or dismiss the question first, then add it to chat.".into());
            return;
        }
        let Some(draft) = self.drafts.get_mut(&thread).filter(|draft| !draft.pending) else {
            return;
        };
        if draft.edit.pills().any(|(_, id)| {
            draft
                .terminal_excerpts
                .iter()
                .any(|other| other.context_id == id && other.same_selection(&excerpt))
        }) {
            return;
        }
        let cursor = draft.edit.focus;
        let leading = draft.edit.text[..cursor]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace());
        let trailing = draft.edit.text[cursor..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_whitespace());
        draft.edit.set_selection(cursor, cursor);
        draft.edit.undo_group(|edit| {
            if leading {
                edit.insert(" ");
            }
            edit.insert_pill(excerpt.context_id.clone(), &excerpt.label);
            if trailing {
                edit.insert(" ");
            }
        });
        draft.terminal_excerpts.push(excerpt);
        tx.focus_when_shown("prompt", false);
    }
}

pub(in crate::ui) fn view(pane: &Pane, [width, height]: [f32; 2]) -> Option<El> {
    let menu = pane.menu.as_ref()?;
    let x = menu.point.x.clamp(0.0, (width - WIDTH).max(0.0));
    let y = menu.point.y.clamp(0.0, (height - HEIGHT).max(0.0));
    let canvas = hex("#fcfcfc").expect("GTK menu canvas");
    let ink = hex("#2e3436").expect("GTK menu text");
    let highlight = hex("#eaeaea").expect("GTK menu highlight");
    Some(
        col()
            .key("terminal-actions")
            .absolute(x, y)
            .size(WIDTH, HEIGHT)
            .py(4.0)
            .fill(canvas)
            .role("menu")
            .label("Terminal actions")
            .focus_group()
            .modal_tx(true, State::close_terminal_menu)
            .children(ACTIONS.map(|action| {
                let enabled = action.enabled(menu);
                row()
                    .key(action.key())
                    .role("menuitem")
                    .label(action.label())
                    .w_full()
                    .h(32.0)
                    .px(20.0)
                    .center_y()
                    .disabled(!enabled)
                    .focusable(enabled)
                    .focus_highlight(highlight)
                    .on_press_tx(move |state: &mut State, tx| state.pick_terminal_menu(action, tx))
                    .on_key(move |state: &mut State, event, tx| {
                        if event.key != Key::Enter {
                            return false;
                        }
                        if enabled && !tx.key_repeat() {
                            state.pick_terminal_menu(action, tx);
                        }
                        true
                    })
                    .child(text(action.label()).font(FONT).color(if enabled {
                        ink
                    } else {
                        ink.alpha(0.45)
                    }))
            })),
    )
}
