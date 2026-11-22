use rune::{
    Align, Color, El, Icon, Key, KeyEvent, ScrollSpec, Shadow, Side, TextEdit, Tx, col, el, icon,
    row, text, text_input,
};

use super::{CAPTION, LABEL, MAGNIFIER, Palette, State, action, catalog, input_fill};

pub(super) const TRIGGER: &str = "keybinding-command";
const SEARCH: &str = "search-keybinding-commands";
const LIST: &str = "keybinding-commands";

static CHEVRON: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);

#[derive(Default)]
pub(super) struct Picker {
    query: TextEdit,
    highlighted: usize,
}

pub(super) fn open(state: &mut State, tx: &mut Tx) {
    let panel = &mut state.settings.keybindings;
    if panel.picker.is_some() {
        close(state, tx);
    } else {
        panel.picker = Some(Picker::default());
        tx.focus_when_shown(SEARCH, tx.focus_visible());
    }
}

fn close(state: &mut State, tx: &mut Tx) {
    state.settings.keybindings.picker = None;
    tx.focus_when_shown(TRIGGER, tx.focus_visible());
}

fn choose(state: &mut State, command: &'static str, tx: &mut Tx) {
    if let Some(draft) = state.settings.keybindings.draft.as_mut() {
        draft.command = Some(command);
        draft.recording = false;
    }
    close(state, tx);
}

fn choices(query: &str) -> Vec<&'static str> {
    let needle = query.trim().to_lowercase();
    catalog::EDITABLE_COMMANDS
        .iter()
        .copied()
        .filter(|command| {
            command.to_lowercase().contains(&needle)
                || catalog::command_label(command)
                    .to_lowercase()
                    .contains(&needle)
        })
        .collect()
}

fn option_key(command: &str) -> String {
    format!("keybinding-command-{command}")
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(picker) = state.settings.keybindings.picker.as_mut() else {
        return false;
    };
    if event.key == Key::Escape {
        close(state, tx);
        return true;
    }
    if !matches!(event.key, Key::Up | Key::Down | Key::Enter | Key::Tab) {
        return false;
    }
    if event.key == Key::Tab {
        close(state, tx);
        return true;
    }
    let matches = choices(&picker.query.text);
    if matches.is_empty() {
        return true;
    }
    let index = picker.highlighted.min(matches.len() - 1);
    match event.key {
        Key::Up => picker.highlighted = (index + matches.len() - 1) % matches.len(),
        Key::Down => picker.highlighted = (index + 1) % matches.len(),
        Key::Enter if !tx.key_repeat() => choose(state, matches[index], tx),
        _ => {}
    }
    true
}

pub(super) fn trigger(state: &State, palette: Palette, compact: bool) -> El {
    let panel = &state.settings.keybindings;
    let label = panel
        .draft
        .as_ref()
        .and_then(|draft| draft.command)
        .map(catalog::command_label)
        .unwrap_or_else(|| "Command".into());
    let trigger = action(
        row()
            .h(28.0)
            .px(9.0)
            .items_center()
            .gap(6.0)
            .border(1.0, palette.line)
            .fill(input_fill(palette))
            .child(
                text(label)
                    .font(LABEL)
                    .color(palette.ink)
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate(),
            )
            .child(icon(&CHEVRON, 12.0).color(palette.muted).a11y_hidden()),
        TRIGGER,
        "Command",
        palette,
        panel.pending.is_none(),
        open,
    )
    .role("combobox")
    .expanded(panel.picker.is_some());
    if compact {
        trigger.w_full()
    } else {
        trigger.w(224.0)
    }
}

pub(super) fn overlay(state: &State, palette: Palette, width: f32) -> Option<[El; 2]> {
    let panel = &state.settings.keybindings;
    let picker = panel.picker.as_ref()?;
    let commands = choices(&picker.query.text);
    let selected = panel.draft.as_ref().and_then(|draft| draft.command);
    let highlighted = commands.get(picker.highlighted).copied();
    let mut list = col()
        .key(LIST)
        .role("listbox")
        .label("Command")
        .w_full()
        .min_h(0.0)
        .max_h(300.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .gap(2.0);
    for (index, command) in commands.iter().enumerate() {
        let mut item = row()
            .key(option_key(command))
            .role("option")
            .label(catalog::command_label(command))
            .focus_selected(selected == Some(*command))
            .min_h(28.0)
            .py(4.0)
            .px(8.0)
            .w_full()
            .gap(6.0)
            .items_center()
            .radius(5.0)
            .fill(if highlighted == Some(*command) {
                palette.hover
            } else {
                Color::TRANSPARENT
            })
            .on_hover(move |state: &mut State| {
                if let Some(picker) = &mut state.settings.keybindings.picker {
                    picker.highlighted = index;
                }
            })
            .child(
                text(catalog::command_label(command))
                    .font(LABEL)
                    .color(palette.ink)
                    .grow(1.0)
                    .min_w(0.0),
            );
        if let Some(bindings) = &state.keybindings {
            let mut previews = col().items_end().gap(3.0).min_w(0.0);
            for binding in bindings.bindings().iter().filter(|binding| {
                binding.rule.command == *command && binding.rule.when.as_deref() != Some("false")
            }) {
                previews = previews.child(
                    text(binding.display_keys.join("+"))
                        .font(CAPTION)
                        .color(palette.muted)
                        .fill(palette.ink.alpha(0.08))
                        .radius(4.0)
                        .px(4.0),
                );
            }
            item = item.child(previews);
        }
        let command = *command;
        list = list.child(item.on_press_tx(move |state, tx| choose(state, command, tx)));
    }
    if commands.is_empty() {
        list = list.child(
            text("No results found.")
                .font(LABEL)
                .color(palette.muted)
                .padding(8.0),
        );
    }

    let mut search = text_input(&picker.query, LABEL)
        .key(SEARCH)
        .role("searchbox")
        .label("Search")
        .placeholder("Search", palette.muted)
        .color(palette.ink)
        .single_line()
        .h(26.0)
        .py(3.0)
        .grow(1.0)
        .min_w(0.0)
        .on_edit(|state: &mut State| {
            &mut state
                .settings
                .keybindings
                .picker
                .as_mut()
                .expect("command picker is open")
                .query
        })
        .on_input_change(|state: &mut State, _| {
            if let Some(picker) = &mut state.settings.keybindings.picker {
                picker.highlighted = 0;
            }
        })
        .on_input_key(navigate);
    if let Some(command) = highlighted {
        search = search.active_descendant(option_key(command));
    }
    let menu = col()
        .key("keybinding-command-popup")
        .anchored_fit(TRIGGER, Side::Bottom, Align::Start, 4.0)
        .anchored_round()
        .w((width - 16.0).clamp(0.0, 384.0))
        .padding(4.0)
        .gap(4.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(if palette.dark {
            rune::color::hex("#151515").expect("command popup surface")
        } else {
            Color::WHITE
        })
        .box_shadow(Shadow {
            x: 0.0,
            y: 12.0,
            blur: 24.0,
            spread: -6.0,
            color: Color::BLACK.alpha(if palette.dark { 0.55 } else { 0.22 }),
            inset: false,
        })
        .child(
            row()
                .w_full()
                .items_center()
                .h(36.0)
                .px(8.0)
                .gap(5.0)
                .border_sides([0.0, 0.0, 1.0, 0.0], palette.accent)
                .child(icon(&MAGNIFIER, 13.0).color(palette.muted).a11y_hidden())
                .child(search),
        )
        .child(list);
    let dismiss = el()
        .key("keybinding-command-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    Some([dismiss, menu])
}
