use rune::{
    Align, Color, El, Icon, Key, ScrollSpec, Shadow, Side, TextEdit, Tx, col, el, icon, row, text,
    text_input,
};

use crate::connection::{KeybindingCondition as Condition, KeybindingRule};

use super::{CAPTION, Draft, LABEL, MONO, PLUS, Palette, State, action, input_fill, row_key};

mod tree;

const EXPRESSION: &str = "keybinding-when-expression";
const ADD_CONDITION: &str = "keybinding-when-add-condition";
const ADD_GROUP: &str = "keybinding-when-add-group";
const INVALID: &str = "Use variables with !, &&, ||, and parentheses.";
const FIELD: rune::TextStyle = rune::TextStyle::new(12.0, 400.0).mono().lh(18.0);

static CHEVRON: Icon = glyph!("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#);

pub(super) struct Editor {
    field: TextEdit,
    value: Option<Condition>,
    valid: bool,
    open: bool,
    picker: Option<tree::Picker>,
}

impl Editor {
    fn new(expression: Option<&str>) -> Self {
        let mut editor = Self {
            field: TextEdit::default(),
            value: None,
            valid: true,
            open: false,
            picker: None,
        };
        editor.field.replace_text(expression.unwrap_or_default());
        editor.refresh();
        editor
    }

    pub(super) fn expression(&self) -> Option<String> {
        self.value.as_ref().map(Condition::expression)
    }

    pub(super) fn valid(&self) -> bool {
        self.valid
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    fn refresh(&mut self) {
        match Condition::parse(&self.field.text) {
            Ok(value) => {
                self.value = value;
                self.valid = true;
            }
            Err(()) => self.valid = false,
        }
    }

    fn replace(&mut self, value: Option<Condition>) {
        self.field.replace_text(
            value
                .as_ref()
                .map(Condition::expression)
                .unwrap_or_default(),
        );
        self.refresh();
    }
}

fn editor(state: &mut State) -> Option<&mut Editor> {
    state.settings.keybindings.draft.as_mut()?.when.as_mut()
}

fn trigger(rule: Option<&KeybindingRule>) -> String {
    rule.map_or_else(
        || "when-new-keybinding".into(),
        |rule| format!("when-{}", row_key(rule)),
    )
}

fn open(state: &mut State, original: Option<KeybindingRule>, tx: &mut Tx) {
    if !state.can_edit_keybindings() {
        return;
    }
    let panel = &mut state.settings.keybindings;
    panel.actions = None;
    panel.picker = None;
    let same_draft = panel
        .draft
        .as_ref()
        .is_some_and(|draft| draft.original == original);
    if !same_draft {
        panel.draft = Some(Draft {
            original,
            command: None,
            recorded: None,
            recording: false,
            when: None,
        });
    }
    let Some(draft) = panel.draft.as_mut() else {
        return;
    };
    let initial = draft.condition();
    let when = draft
        .when
        .get_or_insert_with(|| Editor::new(initial.as_deref()));
    if when.open {
        close(state, tx);
    } else {
        when.open = true;
        tx.focus_when_shown(ADD_CONDITION, tx.focus_visible());
    }
}

fn close(state: &mut State, tx: &mut Tx) {
    let panel = &mut state.settings.keybindings;
    let Some(draft) = panel.draft.as_mut() else {
        return;
    };
    if let Some(when) = draft.when.as_mut() {
        when.open = false;
        when.picker = None;
        when.field
            .replace_text(when.expression().unwrap_or_default());
        when.valid = true;
    }
    let focus = trigger(draft.original.as_ref());
    if draft.original.is_some() && !draft.needs_editor() {
        panel.draft = None;
    }
    tx.focus_when_shown(focus, tx.focus_visible());
}

pub(super) fn control(
    rule: Option<&KeybindingRule>,
    value: Option<&str>,
    expanded: bool,
    command_label: &str,
    enabled: bool,
    palette: Palette,
) -> El {
    let mut content = row()
        .h(20.0)
        .items_center()
        .gap(4.0)
        .min_w(0.0)
        .px(6.0)
        .child(
            text(value.unwrap_or("Always"))
                .font(MONO)
                .color(palette.muted)
                .min_w(0.0)
                .truncate(),
        );
    if enabled {
        let original = rule.cloned();
        content = action(
            content.child(icon(&CHEVRON, 11.0).color(palette.muted).a11y_hidden()),
            trigger(rule),
            format!("Edit when clause for {command_label}"),
            palette,
            true,
            move |state, tx| open(state, original.clone(), tx),
        )
        .expanded(expanded);
    }
    row()
        .min_w(0.0)
        .items_center()
        .gap(6.0)
        .child(text("When").font(CAPTION).color(palette.muted))
        .child(content)
}

fn field(when: &Editor, palette: Palette) -> El {
    text_input(&when.field, FIELD)
        .key(EXPRESSION)
        .label("When expression")
        .placeholder("Always", palette.muted)
        .invalid(!when.valid)
        .color(palette.ink)
        .single_line()
        .h(28.0)
        .w_full()
        .px(8.0)
        .py(5.0)
        .radius(7.0)
        .fill(input_fill(palette))
        .border(
            1.0,
            if when.valid {
                palette.line
            } else {
                palette.error
            },
        )
        .focus_ring(
            1.0,
            1.0,
            if when.valid {
                palette.accent
            } else {
                palette.error
            },
        )
        .on_edit(|state: &mut State| &mut editor(state).expect("When editor shown").field)
        .on_input_change(|state: &mut State, _| {
            if let Some(when) = editor(state) {
                when.refresh();
            }
        })
        .on_input_key(|state: &mut State, event, tx| {
            if event.key == Key::Escape {
                close(state, tx);
                true
            } else {
                false
            }
        })
}

pub(super) fn overlays(state: &State, palette: Palette, width: f32) -> Vec<El> {
    let Some(draft) = state.settings.keybindings.draft.as_ref() else {
        return Vec::new();
    };
    let Some(when) = draft.when.as_ref().filter(|when| when.open) else {
        return Vec::new();
    };
    let header = row()
        .w_full()
        .items_center()
        .gap(8.0)
        .child(
            text("When")
                .font(super::CONTROL)
                .color(palette.ink)
                .grow(1.0),
        )
        .child(add_button(
            ADD_CONDITION,
            "Condition",
            palette,
            when.valid,
            |state, _| tree::add_root(state, false),
        ))
        .child(add_button(
            ADD_GROUP,
            "Group",
            palette,
            when.valid,
            |state, _| tree::add_root(state, true),
        ));
    let mut form = col()
        .w_full()
        .gap(12.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(header)
        .child(field(when, palette));
    if !when.valid {
        form = form.child(
            text(INVALID)
                .role("alert")
                .font(CAPTION)
                .color(palette.error)
                .wrap(),
        );
    }
    let builder = match &when.value {
        Some(node) => tree::view(node, Vec::new(), when.valid, palette),
        None => row()
            .w_full()
            .gap(8.0)
            .padding(12.0)
            .radius(7.0)
            .border(1.0, palette.line)
            .child(add_button(
                "when-empty-add-condition",
                "Condition",
                palette,
                when.valid,
                |state, _| tree::add_root(state, false),
            ))
            .child(add_button(
                "when-empty-add-group",
                "Group",
                palette,
                when.valid,
                |state, _| tree::add_root(state, true),
            )),
    };
    form = form.child(builder);
    if !when.valid {
        form = form.child(
            text("Fix the expression above to continue editing visually.")
                .font(CAPTION)
                .color(palette.error)
                .wrap(),
        );
    }

    let popup = surface(palette)
        .key("keybinding-when-popup")
        .role("dialog")
        .label("When")
        .anchored_fit(
            trigger(draft.original.as_ref()),
            Side::Bottom,
            Align::Start,
            6.0,
        )
        .anchored_round()
        .w((width - 16.0).clamp(0.0, 578.0))
        .max_h(516.0)
        .padding(16.0)
        .on_key(|state: &mut State, event, tx| {
            if event.key == Key::Escape {
                close(state, tx);
                true
            } else {
                false
            }
        })
        .child(form);
    let dismiss = el()
        .key("keybinding-when-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(close);
    let mut layers = vec![dismiss, popup];
    if let Some(picker) = when.picker.as_ref() {
        layers.extend(tree::picker_overlay(picker, palette));
    }
    layers
}

fn surface(palette: Palette) -> El {
    col()
        .radius(8.0)
        .border(1.0, palette.line)
        .border_layout()
        .fill(if palette.dark {
            rune::color::hex("#101010").expect("When editor surface")
        } else {
            Color::WHITE
        })
        .box_shadow(Shadow {
            x: 0.0,
            y: 12.0,
            blur: 24.0,
            spread: -6.0,
            color: Color::BLACK.alpha(if palette.dark { 0.62 } else { 0.22 }),
            inset: false,
        })
}

fn add_button(
    key: impl Into<String>,
    label: &'static str,
    palette: Palette,
    enabled: bool,
    press: impl Fn(&mut State, &mut Tx) + Clone + 'static,
) -> El {
    let inner = row()
        .h(28.0)
        .px(8.0)
        .gap(3.0)
        .items_center()
        .fill(input_fill(palette))
        .border(1.0, palette.line)
        .child(icon(&PLUS, 12.0).color(palette.muted).a11y_hidden());
    action(
        inner.child(text(label).font(LABEL).color(if enabled {
            palette.ink
        } else {
            palette.muted
        })),
        key,
        label,
        palette,
        enabled,
        press,
    )
}
