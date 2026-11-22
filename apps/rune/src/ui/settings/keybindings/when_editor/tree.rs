use rune::{Align, Color, El, Icon, Key, ScrollSpec, Side, Tx, col, el, icon, row, text};

use super::{
    CAPTION, CHEVRON, Condition, LABEL, Palette, State, action, add_button, editor, input_fill,
    surface,
};

const OPTIONS: &str = "keybinding-when-options";
const VARIABLES: &[&str] = &[
    "terminalFocus",
    "terminalOpen",
    "previewOpen",
    "isWeb",
    "isDesktop",
    "true",
    "false",
    "composerFocus",
    "draftThreadRoute",
    "editableFocus",
    "modelPickerOpen",
    "previewFocus",
    "rightPanelOpen",
    "turnRunning",
    "usagePageOpen",
];

static MINUS: Icon = glyph!("minus", r#"<path d="M5 12h14"/>"#);

#[derive(Clone, Copy)]
pub(super) enum Branch {
    Left,
    Right,
    Negated,
}

type Path = Vec<Branch>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operator {
    And,
    Or,
}

impl Operator {
    fn of(node: &Condition) -> Option<Self> {
        match node {
            Condition::And { .. } => Some(Self::And),
            Condition::Or { .. } => Some(Self::Or),
            _ => None,
        }
    }

    fn join(self, left: Condition, right: Condition) -> Condition {
        let left = Box::new(left);
        let right = Box::new(right);
        match self {
            Self::And => Condition::And { left, right },
            Self::Or => Condition::Or { left, right },
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::And => "and",
            Self::Or => "or",
        }
    }
}

fn default_node(group: bool) -> Condition {
    let value = Condition::Identifier {
        name: "terminalFocus".into(),
    };
    if group {
        Operator::Or.join(
            value.clone(),
            Condition::Not {
                node: Box::new(value),
            },
        )
    } else {
        value
    }
}

pub(super) fn add_root(state: &mut State, group: bool) {
    if let Some(when) = editor(state).filter(|when| when.valid) {
        let next = default_node(group);
        when.replace(Some(match when.value.clone() {
            Some(existing) => Operator::And.join(existing, next),
            None => next,
        }));
    }
}

fn path_key(prefix: &str, path: &[Branch]) -> String {
    let mut key = prefix.to_owned();
    for branch in path {
        key.push(match branch {
            Branch::Left => 'l',
            Branch::Right => 'r',
            Branch::Negated => 'n',
        });
    }
    key
}

fn child(path: &[Branch], branch: Branch) -> Path {
    let mut path = path.to_vec();
    path.push(branch);
    path
}

fn value_at<'a>(node: &'a mut Condition, path: &[Branch]) -> Option<&'a mut Condition> {
    let Some((branch, tail)) = path.split_first() else {
        return Some(node);
    };
    let child = match (node, branch) {
        (Condition::And { left, .. } | Condition::Or { left, .. }, Branch::Left) => left,
        (Condition::And { right, .. } | Condition::Or { right, .. }, Branch::Right) => right,
        (Condition::Not { node }, Branch::Negated) => node,
        _ => return None,
    };
    value_at(child, tail)
}

fn change(state: &mut State, path: &[Branch], edit: impl FnOnce(&mut Condition)) {
    let Some(when) = editor(state).filter(|when| when.valid) else {
        return;
    };
    let Some(mut root) = when.value.clone() else {
        return;
    };
    if let Some(node) = value_at(&mut root, path) {
        edit(node);
        when.replace(Some(root));
    }
}

fn remove(state: &mut State, path: &[Branch]) {
    if let Some((branch, parent)) = path.split_last() {
        change(state, parent, |node| match (&*node, branch) {
            (Condition::And { right, .. } | Condition::Or { right, .. }, Branch::Left) => {
                *node = *right.clone()
            }
            (Condition::And { left, .. } | Condition::Or { left, .. }, Branch::Right) => {
                *node = *left.clone()
            }
            _ => {}
        });
    } else if let Some(when) = editor(state).filter(|when| when.valid) {
        when.replace(None);
    }
}

fn negate(state: &mut State, path: &[Branch]) {
    change(state, path, |node| {
        *node = match &*node {
            Condition::Not { node } => *node.clone(),
            current => Condition::Not {
                node: Box::new(current.clone()),
            },
        };
    });
}

fn append(state: &mut State, path: &[Branch], group: bool) {
    change(state, path, |node| {
        if let Some(operator) = Operator::of(node) {
            *node = operator.join(node.clone(), default_node(group));
        }
    });
}

fn set_operator(node: Condition, from: Operator, to: Operator) -> Condition {
    if Operator::of(&node) != Some(from) {
        return node;
    }
    match node {
        Condition::And { left, right } | Condition::Or { left, right } => to.join(
            set_operator(*left, from, to),
            set_operator(*right, from, to),
        ),
        _ => node,
    }
}

#[derive(Clone, Copy)]
enum Choice {
    Variable,
    Operator,
}

pub(super) struct Picker {
    path: Path,
    choice: Choice,
    options: Vec<String>,
    selected: String,
    highlight: usize,
}

fn selection_key(path: &[Branch], choice: Choice) -> String {
    path_key(
        match choice {
            Choice::Variable => "when-variable-",
            Choice::Operator => "when-operator-",
        },
        path,
    )
}

fn open_picker(state: &mut State, path: Path, choice: Choice, selected: &str, tx: &mut Tx) {
    let Some(when) = editor(state).filter(|when| when.valid) else {
        return;
    };
    let labels = match choice {
        Choice::Variable => VARIABLES,
        Choice::Operator => &["and", "or"],
    };
    let mut options: Vec<_> = labels.iter().map(|label| (*label).to_owned()).collect();
    let highlight = options
        .iter()
        .position(|option| option == selected)
        .unwrap_or_else(|| {
            options.insert(0, selected.to_owned());
            0
        });
    when.picker = Some(Picker {
        path,
        choice,
        options,
        selected: selected.to_owned(),
        highlight,
    });
    tx.focus_when_shown(OPTIONS, true);
}

fn dismiss_picker(state: &mut State, tx: &mut Tx) -> Option<Picker> {
    let picker = editor(state)?.picker.take()?;
    tx.focus_when_shown(
        selection_key(&picker.path, picker.choice),
        tx.focus_visible(),
    );
    Some(picker)
}

fn select(state: &mut State, index: usize, tx: &mut Tx) {
    let Some(picker) = dismiss_picker(state, tx) else {
        return;
    };
    let Some(value) = picker.options.get(index) else {
        return;
    };
    change(state, &picker.path, |node| match picker.choice {
        Choice::Variable => {
            let identifier = match node {
                Condition::Not { node } => node.as_mut(),
                _ => node,
            };
            if let Condition::Identifier { name } = identifier {
                name.clone_from(value);
            }
        }
        Choice::Operator => {
            if let Some(from) = Operator::of(node) {
                let to = if value == "or" {
                    Operator::Or
                } else {
                    Operator::And
                };
                *node = set_operator(node.clone(), from, to);
            }
        }
    });
}

pub(super) fn picker_overlay(picker: &Picker, palette: Palette) -> [El; 2] {
    let mut menu = surface(palette)
        .key(OPTIONS)
        .role("listbox")
        .label(match picker.choice {
            Choice::Variable => "Condition variable",
            Choice::Operator => "Boolean operator",
        })
        .focusable(true)
        .anchored_fit(
            selection_key(&picker.path, picker.choice),
            Side::Bottom,
            Align::Start,
            4.0,
        )
        .w(220.0)
        .min_h(0.0)
        .max_h(288.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .gap(3.0)
        .padding(4.0)
        .active_descendant(format!("when-option-{}", picker.highlight))
        .on_key(|state: &mut State, event, tx| {
            let Some(picker) = editor(state).and_then(|when| when.picker.as_mut()) else {
                return false;
            };
            match event.key {
                Key::Up | Key::Down => {
                    let count = picker.options.len();
                    let step = if event.key == Key::Up { count - 1 } else { 1 };
                    picker.highlight = (picker.highlight + step) % count;
                    tx.scroll_into_view_in(OPTIONS, format!("when-option-{}", picker.highlight));
                }
                Key::Enter => {
                    let index = picker.highlight;
                    select(state, index, tx);
                }
                Key::Escape => {
                    dismiss_picker(state, tx);
                }
                _ => return false,
            }
            true
        });
    for (index, option) in picker.options.iter().enumerate() {
        menu = menu.child(
            row()
                .key(format!("when-option-{index}"))
                .role("option")
                .label(option)
                .focus_selected(option == &picker.selected)
                .h(28.0)
                .px(8.0)
                .w_full()
                .items_center()
                .radius(4.0)
                .fill(if index == picker.highlight {
                    palette.hover
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    if let Some(picker) = editor(state).and_then(|when| when.picker.as_mut()) {
                        picker.highlight = index;
                    }
                })
                .on_press_tx(move |state, tx| select(state, index, tx))
                .child(text(option).font(LABEL).color(palette.ink)),
        );
    }
    let dismiss = el()
        .key("keybinding-when-options-dismiss")
        .role("none")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_background_press_tx(|state, tx| {
            dismiss_picker(state, tx);
        });
    [dismiss, menu]
}

fn selector(path: &[Branch], choice: Choice, label: &str, enabled: bool, palette: Palette) -> El {
    let key = selection_key(path, choice);
    let path = path.to_vec();
    let selection = label.to_owned();
    action(
        row()
            .h(28.0)
            .px(8.0)
            .items_center()
            .gap(5.0)
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
        key,
        label,
        palette,
        enabled,
        move |state, tx| open_picker(state, path.clone(), choice, &selection, tx),
    )
    .role("combobox")
}

fn negate_button(
    path: &[Branch],
    label: String,
    negated: bool,
    enabled: bool,
    palette: Palette,
) -> El {
    let key = path_key("when-negate-", path);
    let path = path.to_vec();
    action(
        row()
            .h(28.0)
            .px(8.0)
            .items_center()
            .border(1.0, palette.line)
            .fill(if negated {
                palette.ink.alpha(0.12)
            } else {
                input_fill(palette)
            })
            .child(text("Not").font(LABEL).color(palette.ink)),
        key,
        label,
        palette,
        enabled,
        move |state, _| negate(state, &path),
    )
    .checked(negated)
}

fn remove_button(path: &[Branch], group: bool, enabled: bool, palette: Palette) -> El {
    if matches!(path.last(), Some(Branch::Negated)) {
        return el();
    }
    let key = path_key("when-remove-", path);
    let label = if path.is_empty() {
        "Clear all conditions"
    } else if group {
        "Remove group and its conditions"
    } else {
        "Remove condition"
    };
    let path = path.to_vec();
    action(
        el().size(28.0, 28.0)
            .center()
            .child(icon(&MINUS, 12.0).color(palette.muted).a11y_hidden()),
        key,
        label,
        palette,
        enabled,
        move |state, _| remove(state, &path),
    )
}

fn condition(name: &str, path: &[Branch], negated: bool, enabled: bool, palette: Palette) -> El {
    let mut node = row()
        .w_full()
        .items_center()
        .gap(8.0)
        .padding(8.0)
        .radius(6.0)
        .border(1.0, palette.line)
        .fill(palette.canvas.alpha(0.48))
        .child(negate_button(
            path,
            format!("Negate {name}"),
            negated,
            enabled,
            palette,
        ))
        .child(
            selector(path, Choice::Variable, name, enabled, palette)
                .grow(1.0)
                .min_w(0.0),
        );
    if !VARIABLES.contains(&name) {
        node = node.child(text("Unknown variable").font(CAPTION).color(palette.muted));
    }
    node.child(remove_button(path, false, enabled, palette))
}

fn collect_group<'a>(
    node: &'a Condition,
    operator: Operator,
    path: Path,
    out: &mut Vec<(&'a Condition, Path)>,
) {
    match node {
        Condition::And { left, right } | Condition::Or { left, right }
            if Operator::of(node) == Some(operator) =>
        {
            collect_group(left, operator, child(&path, Branch::Left), out);
            collect_group(right, operator, child(&path, Branch::Right), out);
        }
        _ => out.push((node, path)),
    }
}

pub(super) fn view(node: &Condition, path: Path, enabled: bool, palette: Palette) -> El {
    match node {
        Condition::Identifier { name } => condition(name, &path, false, enabled, palette),
        Condition::Not { node } => match node.as_ref() {
            Condition::Identifier { name } => condition(name, &path, true, enabled, palette),
            nested => frame(palette)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .items_center()
                        .child(negate_button(
                            &path,
                            "Negate group".into(),
                            true,
                            enabled,
                            palette,
                        ))
                        .child(el().grow(1.0))
                        .child(remove_button(&path, true, enabled, palette)),
                )
                .child(view(nested, child(&path, Branch::Negated), enabled, palette).ml(16.0)),
        },
        Condition::And { .. } | Condition::Or { .. } => {
            let operator = Operator::of(node).expect("boolean condition group");
            let add = |group, label, key| {
                let path = path.to_vec();
                add_button(
                    path_key(key, &path),
                    label,
                    palette,
                    enabled,
                    move |state, _| append(state, &path, group),
                )
            };
            let header = row()
                .w_full()
                .gap(8.0)
                .items_center()
                .child(
                    selector(&path, Choice::Operator, operator.label(), enabled, palette).w(144.0),
                )
                .child(add(false, "Condition", "when-add-condition-"))
                .child(add(true, "Group", "when-add-group-"))
                .child(el().grow(1.0))
                .child(remove_button(&path, true, enabled, palette));
            let mut members = Vec::new();
            collect_group(node, operator, path, &mut members);
            let mut items = col()
                .gap(8.0)
                .pl(10.0)
                .ml(6.0)
                .border_sides([0.0, 0.0, 0.0, 1.0], palette.line);
            for (member, path) in members {
                items = items.child(view(member, path, enabled, palette));
            }
            frame(palette).child(header).child(items)
        }
    }
}

fn frame(palette: Palette) -> El {
    col()
        .w_full()
        .gap(8.0)
        .padding(8.0)
        .radius(8.0)
        .border(1.0, palette.line)
}
