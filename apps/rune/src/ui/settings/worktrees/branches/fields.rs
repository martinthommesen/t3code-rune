use rune::{El, TextEdit, TextStyle, Tx, col, edit_access, row, text, text_input};
use uuid::Uuid;

use crate::connection::BranchTextField;

use super::super::super::{CONTROL, DESCRIPTION, Page, Palette, RESET, State, icon_button};
use super::workspace::{self, BranchTextView};

mod textarea;

const FIELD_TEXT: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);

struct Labels {
    row: &'static str,
    input: &'static str,
    reset: &'static str,
    title: &'static str,
    description: &'static str,
    placeholder: &'static str,
    environment_reset: &'static str,
    project_reset: &'static str,
}

impl Labels {
    fn for_field(field: BranchTextField) -> Self {
        match field {
            BranchTextField::Prefix => Self {
                row: "branch-prefix-row",
                input: "branch-prefix",
                reset: "reset-branch-prefix",
                title: "Branch prefix",
                description: "For example, t3 or t3/ produces t3/add-search. Leave empty for no prefix.",
                placeholder: "No prefix",
                environment_reset: "Reset branch prefix to default",
                project_reset: "Reset Branch prefix to default",
            },
            BranchTextField::Instructions => Self {
                row: "branch-instructions-row",
                input: "branch-instructions",
                reset: "reset-branch-instructions",
                title: "Branch naming instructions",
                description: "Appended to the naming prompt. The model returns the complete branch name; no prefix or suffix is added.",
                placeholder: "Use julius/ followed by the issue ID and a short description.",
                environment_reset: "Reset branch naming instructions to default",
                project_reset: "Reset Branch naming instructions to default",
            },
        }
    }
}

#[derive(Default)]
pub(in crate::ui::settings) struct FieldsPanel {
    editors: [Editor; 2],
}

#[derive(Default)]
struct Editor {
    project_id: Option<String>,
    saved: Option<String>,
    draft: TextEdit,
    edited: bool,
    pending: Option<Uuid>,
    resize: textarea::Resize,
}

impl FieldsPanel {
    pub(in crate::ui::settings) fn settle(&mut self, operation: Uuid, saved: bool) {
        for editor in &mut self.editors {
            if editor.pending == Some(operation) {
                editor.pending = None;
                if !saved {
                    editor.replace_draft(editor.saved.clone().unwrap_or_default());
                }
            }
        }
    }

    fn editor(&mut self, field: BranchTextField) -> &mut Editor {
        &mut self.editors[field.index()]
    }
}

impl Editor {
    fn sync(&mut self, setting: Option<&BranchTextView<'_>>) -> bool {
        let project_id = setting.and_then(|setting| setting.project_id);
        let saved = setting.map(|setting| setting.value);
        if self.project_id.as_deref() == project_id && self.saved.as_deref() == saved {
            return false;
        }
        if self.project_id.as_deref() != project_id || saved.is_none() {
            self.pending = None;
        }
        self.project_id = project_id.map(str::to_owned);
        self.saved = saved.map(str::to_owned);
        self.replace_draft(saved.unwrap_or_default().to_owned());
        true
    }

    fn replace_draft(&mut self, value: String) {
        self.draft.replace_text(&value);
        self.edited = false;
        self.resize = textarea::Resize::default();
    }

    fn pending(&mut self, operation: Uuid, value: String) {
        self.pending = Some(operation);
        self.replace_draft(value);
    }
}

pub(in crate::ui) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let instructions = BranchTextField::Instructions;
    if !state.settings.open
        || state.settings.page != Page::Worktrees
        || state
            .settings
            .workspace
            .branch_naming_view(state)
            .is_none_or(|naming| naming.mode != instructions.mode())
    {
        state.settings.branch_fields.editor(instructions).resize = textarea::Resize::default();
    }
    let mut changed = false;
    for field in BranchTextField::ALL {
        changed |= poll_field(state, tx, field);
    }
    changed
}

fn poll_field(state: &mut State, tx: &mut Tx, field: BranchTextField) -> bool {
    let setting = state.settings.workspace.branch_text_view(state, field);
    let editor = state.settings.branch_fields.editor(field);
    let changed = editor.sync(setting.as_ref());
    let labels = Labels::for_field(field);
    if !editor.edited
        || !setting.is_some_and(|setting| setting.writable)
        || tx.focused() == Some(labels.input)
        || (tx.focused() == Some(labels.reset) && !tx.focus_visible())
    {
        return changed;
    }

    editor.edited = false;
    let value = editor.draft.text.trim_matches(|character: char| {
        character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
    });
    if editor.saved.as_deref() == Some(value) {
        return changed;
    }
    let value = value.to_owned();
    if let Some(operation) = workspace::save_branch_text(state, field, Some(value.clone())) {
        state
            .settings
            .branch_fields
            .editor(field)
            .pending(operation, value);
    }
    true
}

pub(super) fn view(
    state: &State,
    palette: Palette,
    width: f32,
    field: BranchTextField,
) -> Option<El> {
    let setting = state.settings.workspace.branch_text_view(state, field)?;
    let labels = Labels::for_field(field);
    let editor = &state.settings.branch_fields.editors[field.index()];
    let mut input = text_input(&editor.draft, FIELD_TEXT)
        .key(labels.input)
        .label(labels.title)
        .placeholder(labels.placeholder, palette.muted)
        .color(palette.ink)
        .px(11.0)
        .radius(8.0)
        .border(1.0, palette.line)
        .focus_ring(1.0, 1.0, palette.accent);
    input = match field {
        BranchTextField::Prefix => input.single_line().h(30.0),
        BranchTextField::Instructions => input.py(4.0),
    };
    if setting.writable {
        input = input
            .on_edit(edit_access(move |state: &mut State| {
                &mut state.settings.branch_fields.editor(field).draft
            }))
            .on_input_change(move |state: &mut State, _| {
                state.settings.branch_fields.editor(field).edited = true;
            })
            .on_submit(move |state: &mut State, _| {
                if field == BranchTextField::Instructions {
                    let editor = state.settings.branch_fields.editor(field);
                    editor.draft.insert("\n");
                    editor.edited = true;
                }
            });
    } else {
        input = input.disabled(true).read_only(true);
        if let Some(organization) = setting.managed_by {
            input = input.help(format!("Managed by {organization}."));
        }
    }

    let mut title = row()
        .gap(6.0)
        .items_center()
        .child(text(labels.title).font(CONTROL).color(palette.ink));
    let project = setting.project_id.is_some();
    let may_reset = setting.reset_value.is_some();
    if project {
        let source = if may_reset {
            "Overridden for this project".to_owned()
        } else {
            state.workspace.as_ref().map_or_else(
                || "Inherited from environment".to_owned(),
                |workspace| format!("Inherited from {}", workspace.label),
            )
        };
        title = title.child(workspace::inheritance_marker(
            source,
            Palette {
                muted: if may_reset {
                    palette.accent
                } else {
                    palette.muted
                },
                ..palette
            },
        ));
    } else {
        title = title.child(rune::el().size(20.0, 20.0).a11y_hidden());
    }

    if setting.writable && may_reset {
        title = title.child(
            icon_button(
                labels.reset,
                if project {
                    labels.project_reset
                } else {
                    labels.environment_reset
                },
                &RESET,
                12.0,
                20.0,
                palette,
                reset_handler(field),
            )
            .help(if project {
                "Reset to inherited value"
            } else {
                "Reset to default"
            }),
        );
    }

    let heading = col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
        text(labels.description)
            .font(DESCRIPTION)
            .color(palette.muted)
            .wrap(),
    );
    let row = match field {
        BranchTextField::Prefix if width >= 560.0 => row()
            .items_center()
            .gap(32.0)
            .child(heading)
            .child(input.w(232.0)),
        BranchTextField::Prefix => col().gap(12.0).child(heading).child(input.w_full()),
        BranchTextField::Instructions => col().gap(12.0).child(heading).child(textarea::view(
            state,
            input,
            palette,
            setting.writable,
        )),
    };
    Some(
        row.key(labels.row)
            .focusable(true)
            .tab_index(-1)
            .no_focus_ring()
            .w_full()
            .px(16.0)
            .pt(12.0)
            .pb(if field == BranchTextField::Instructions {
                20.0
            } else {
                12.0
            }),
    )
}

fn reset_handler(field: BranchTextField) -> fn(&mut State, &mut Tx) {
    match field {
        BranchTextField::Prefix => |state, tx| reset(state, tx, BranchTextField::Prefix),
        BranchTextField::Instructions => {
            |state, tx| reset(state, tx, BranchTextField::Instructions)
        }
    }
}

fn reset(state: &mut State, tx: &mut Tx, field: BranchTextField) {
    let Some(value) = state
        .settings
        .workspace
        .branch_text_view(state, field)
        .and_then(|setting| setting.reset_value)
        .map(str::to_owned)
    else {
        return;
    };
    if let Some(operation) = workspace::save_branch_text(state, field, None) {
        state
            .settings
            .branch_fields
            .editor(field)
            .pending(operation, value);
        tx.focus(Labels::for_field(field).row, tx.focus_visible());
    }
}
