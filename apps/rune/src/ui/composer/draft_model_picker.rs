use rune::{
    Color, El, Key, KeyEvent, ScrollSpec, TextEdit, Tx, col, edit_access, el, icon, row, text,
    text_input,
};

use crate::connection::{
    DraftProvider, ModelSelection, ProviderMetadata, ProviderModel, ProviderReadiness,
    WorkspaceMode,
};

use super::super::sidebar::provider_badge;
use super::super::{Draft, Palette, State, Workspace};
use super::model_picker::{self, CHECK, Colors, DETAILS, LABEL, SEARCH};

const TRIGGER: &str = "draft-model-picker";
const QUERY: &str = "draft-model-search";
const EFFORT: &str = "draft-model-effort";
const PROVIDERS: [DraftProvider; 2] = [DraftProvider::Codex, DraftProvider::Claude];

pub(in crate::ui) struct Picker {
    provider: DraftProvider,
    expanded: [bool; 2],
    query: TextEdit,
}

impl Picker {
    fn new(provider: DraftProvider, is_legacy: bool) -> Self {
        let mut expanded = [false; 2];
        expanded[provider_index(provider)] = is_legacy;
        Self {
            provider,
            expanded,
            query: TextEdit::default(),
        }
    }
}

fn provider_index(provider: DraftProvider) -> usize {
    match provider {
        DraftProvider::Codex => 0,
        DraftProvider::Claude => 1,
    }
}

#[derive(Clone, Copy)]
struct Choice<'a> {
    provider: DraftProvider,
    metadata: &'a ProviderMetadata,
    model: &'a ProviderModel,
}

impl Choice<'_> {
    fn matches(self, selected: &ModelSelection) -> bool {
        self.provider.instance_id() == selected.instance_id()
            && self.model.slug == selected.model()
            && (self.provider != DraftProvider::Claude || selected.plain_model().is_some())
    }

    fn mark(self, palette: Palette, color: Colors, size: f32) -> El {
        match self.provider {
            DraftProvider::Codex => {
                provider_badge::identity(self.metadata, palette.dark, color.ink).1
            }
            DraftProvider::Claude => model_picker::claude(size),
        }
    }
}

pub(super) struct Models<'a> {
    choices: Vec<Choice<'a>>,
    selected: Option<Choice<'a>>,
    full_access: bool,
}

impl<'a> Models<'a> {
    pub(super) fn new(workspace: &'a Workspace, draft: &Draft, project: &str) -> Option<Self> {
        let claude_root = draft.workspace_mode == WorkspaceMode::Local
            && workspace.scratch_project.as_deref() != Some(project);
        let policy = draft.model_policy.as_ref()?;
        let providers = workspace.providers.as_ref()?;
        let choices: Vec<_> = PROVIDERS
            .into_iter()
            .filter_map(|provider| {
                let meta = providers.get(provider.instance_id())?;
                (meta.driver == provider.instance_id()
                    && meta.readiness() == ProviderReadiness::Ready
                    && (provider != DraftProvider::Claude
                        || (claude_root && meta.native_root_launch)))
                    .then_some((provider, meta))
            })
            .flat_map(|(provider, metadata)| {
                metadata
                    .models()
                    .iter()
                    .filter(move |model| policy.allows(provider, &model.slug))
                    .map(move |model| Choice {
                        provider,
                        metadata,
                        model,
                    })
            })
            .collect();
        if choices.is_empty() {
            return None;
        }
        let selected = if let Some(model) = draft.selected_model() {
            choices.iter().find(|choice| choice.matches(model))
        } else {
            let mut codex = choices
                .iter()
                .filter(|choice| choice.provider == DraftProvider::Codex);
            let slug = crate::server::preferred_codex_model(
                codex
                    .clone()
                    .map(|choice| (choice.model.slug.as_str(), choice.model.is_default)),
            );
            codex.find(|choice| slug == Some(choice.model.slug.as_str()))
        }
        .copied();
        Some(Self {
            selected,
            choices,
            full_access: policy.full_access(),
        })
    }

    pub(super) fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    pub(super) fn full_access(&self) -> bool {
        self.full_access
    }

    pub(super) fn trigger(&self, project: &str, open: bool, palette: Palette) -> El {
        let color = Colors::new(palette.dark);
        let project = project.to_owned();
        let initial = self.selected.unwrap_or(self.choices[0]);
        let provider = initial.provider;
        let legacy = self.selected.is_some_and(|choice| choice.model.is_legacy);
        let name = self.selected.map_or("Choose a model", |choice| {
            choice
                .model
                .short_name
                .as_deref()
                .unwrap_or(&choice.model.name)
        });
        let claude = self
            .selected
            .is_some_and(|choice| choice.provider == DraftProvider::Claude);
        let label = if claude {
            format!("{}, High · 200k", initial.model.name)
        } else {
            self.selected
                .map_or_else(|| name.to_owned(), |choice| choice.model.name.clone())
        };
        let width = if claude {
            183.0
        } else {
            (rune::text::measure(name, &[], LABEL, None).0 + 42.0).clamp(78.0, 183.0)
        };
        let mut trigger = row()
            .key(TRIGGER)
            .role("button")
            .label(label)
            .help("Models")
            .expanded(open)
            .focusable(true)
            .focus_ring(1.0, -1.0, palette.accent)
            .w(width)
            .shrink()
            .h(28.0)
            .px(8.0)
            .center_y()
            .gap(4.0)
            .radius(9.0)
            .fill(color.control)
            .hover(color.hover)
            .on_press_tx(move |state: &mut State, tx| {
                if let Some(draft) = state.new_thread_drafts.get_mut(&project)
                    && draft.model_picker.take().is_none()
                {
                    draft.model_picker = Some(Picker::new(provider, legacy));
                    tx.focus_when_shown(QUERY, false);
                }
            });
        if let Some(choice) = self.selected {
            trigger = trigger.child(choice.mark(palette, color, 16.0));
        }
        trigger = trigger.child(text(name).font(LABEL).color(color.ink).truncate());
        if claude {
            trigger = trigger
                .child(text("High").font(LABEL).color(color.muted))
                .child(text("200k").font(DETAILS).color(color.ink));
        }
        trigger
    }

    pub(super) fn popup(&self, project: &str, picker: &Picker, lane: f32, palette: Palette) -> El {
        let color = Colors::new(palette.dark);
        let edit_project = project.to_owned();
        let input_project = project.to_owned();
        let needle = picker.query.text.trim().to_lowercase();
        let searching = !needle.is_empty();
        let expanded = picker.expanded[provider_index(picker.provider)];
        let mut rows = col().w_full().gap(2.0);
        let mut results = 0;
        let mut legacy = false;
        for &choice in &self.choices {
            if searching {
                if !choice.model.name.to_lowercase().contains(&needle)
                    && !choice.model.slug.to_lowercase().contains(&needle)
                {
                    continue;
                }
            } else {
                if choice.provider != picker.provider {
                    continue;
                }
                legacy |= choice.model.is_legacy;
                if choice.model.is_legacy && !expanded {
                    continue;
                }
            }
            rows = rows.child(self.model_row(project, choice, palette, color));
            results += 1;
        }
        if legacy {
            let project = project.to_owned();
            rows = rows.child(
                row()
                    .w_full()
                    .key("draft-model-legacy")
                    .role("option")
                    .label(if expanded { "Show less" } else { "Show more" })
                    .px(8.0)
                    .center_y()
                    .h(28.0)
                    .radius(5.0)
                    .focusable(true)
                    .focus_ring(1.0, -1.0, palette.accent)
                    .hover(color.hover)
                    .on_press_tx(move |state: &mut State, _| {
                        if let Some(picker) = picker_mut(state, &project) {
                            let index = provider_index(picker.provider);
                            picker.expanded[index] = !picker.expanded[index];
                        }
                    })
                    .child(
                        text(if expanded { "Show less" } else { "Show more" })
                            .font(LABEL)
                            .color(color.muted),
                    ),
            );
        } else if results == 0 {
            rows = rows.child(text("No matching models").font(DETAILS).color(color.muted));
        }
        let list = col()
            .min_w(0.0)
            .min_h(0.0)
            .grow(1.0)
            .gap(9.0)
            .padding(10.0)
            .child(
                row()
                    .h(32.0)
                    .center_y()
                    .gap(6.0)
                    .border_sides([0.0, 0.0, 1.0, 0.0], palette.accent)
                    .child(icon(&SEARCH, 14.0).color(color.muted).a11y_hidden())
                    .child(
                        text_input(&picker.query, LABEL)
                            .key(QUERY)
                            .label("Search models")
                            .w_full()
                            .h(27.0)
                            .color(color.ink)
                            .single_line()
                            .placeholder("Search models...", color.muted)
                            .on_edit(edit_access(move |state: &mut State| {
                                &mut picker_mut(state, &edit_project)
                                    .expect("open model picker")
                                    .query
                            }))
                            .on_input_key(move |state: &mut State, event, tx| {
                                escape(state, &input_project, event, tx)
                            }),
                    ),
            )
            .child(
                col()
                    .key("draft-model-rows")
                    .w_full()
                    .min_h(0.0)
                    .grow(1.0)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .child(rows),
            );
        let width = model_picker::popup_width(lane);
        let columns = width >= 420.0;
        let details = if let Some(choice) = self
            .selected
            .filter(|choice| choice.provider == DraftProvider::Claude)
        {
            model_picker::claude_details(
                choice.model,
                if columns { 192.0 } else { width - 45.0 },
                palette,
                EFFORT,
                QUERY,
            )
        } else {
            el().w(192.0).a11y_hidden()
        };
        let mut body = row().w_full().min_h(0.0).grow(1.0);
        if !searching {
            body = body.child(self.provider_tabs(project, picker, palette, color));
        }
        body = if columns {
            body.child(list)
                .child(details.border_sides([0.0, 0.0, 0.0, 1.0], color.line))
        } else {
            body.child(col().grow(1.0).min_h(0.0).child(list).child(details))
        };
        let project = project.to_owned();
        let frame = model_picker::popup_frame("draft-model-popup", "Models", lane, palette, body)
            .on_key(move |state: &mut State, event, tx| escape(state, &project, event, tx));
        if self.full_access {
            frame.child(model_picker::permissions_footer("Full access", color))
        } else {
            frame
        }
    }

    fn provider_tabs(&self, project: &str, picker: &Picker, palette: Palette, color: Colors) -> El {
        let mut side = col()
            .w(44.0)
            .h_full()
            .padding(4.0)
            .gap(4.0)
            .border_sides([0.0, 1.0, 0.0, 0.0], color.line);
        for provider in PROVIDERS {
            let Some(choice) = self
                .choices
                .iter()
                .find(|choice| choice.provider == provider)
            else {
                continue;
            };
            let (name, _) = provider_badge::identity(choice.metadata, palette.dark, color.ink);
            let project = project.to_owned();
            side = side.child(
                el().key(format!("draft-model-provider-{}", provider.instance_id()))
                    .role("tab")
                    .label(name)
                    .checked(picker.provider == provider)
                    .size(36.0, 36.0)
                    .center()
                    .radius(5.0)
                    .focusable(true)
                    .focus_ring(1.0, -1.0, palette.accent)
                    .fill(if picker.provider == provider {
                        color.control
                    } else {
                        Color::TRANSPARENT
                    })
                    .hover(color.hover)
                    .on_press_tx(move |state: &mut State, tx| {
                        if let Some(picker) = picker_mut(state, &project) {
                            picker.provider = provider;
                            tx.scroll_to_instant("draft-model-rows", 0.0, 0.0);
                            tx.focus_when_shown(QUERY, tx.focus_visible());
                        }
                    })
                    .child(choice.mark(palette, color, 20.0)),
            );
        }
        side
    }

    fn model_row(&self, project: &str, choice: Choice<'_>, palette: Palette, color: Colors) -> El {
        let checked = self.selected.is_some_and(|selected| {
            choice.provider == selected.provider && choice.model.slug == selected.model.slug
        });
        let project = project.to_owned();
        let provider = choice.provider;
        let slug = choice.model.slug.clone();
        let legacy = choice.model.is_legacy;
        let line = row()
            .key(format!("draft-model-{}-{}", provider.instance_id(), slug))
            .role("option")
            .label(&choice.model.name)
            .checked(checked)
            .w_full()
            .h(32.0)
            .px(8.0)
            .gap(8.0)
            .center_y()
            .radius(5.0)
            .focusable(true)
            .focus_ring(1.0, -1.0, palette.accent)
            .hover(color.hover)
            .on_press_tx(move |state: &mut State, tx| {
                if let Some(draft) = state.new_thread_drafts.get_mut(&project)
                    && let Some(picker) = &mut draft.model_picker
                {
                    if !checked {
                        draft.model_choice = Some(ModelSelection::plain(provider, &slug));
                    }
                    picker.provider = provider;
                    picker.expanded[provider_index(provider)] |= legacy;
                    if provider == DraftProvider::Claude {
                        tx.focus_when_shown(EFFORT, tx.focus_visible());
                    }
                }
            })
            .child(choice.mark(palette, color, 16.0))
            .child(
                text(&choice.model.name)
                    .font(LABEL)
                    .color(color.ink)
                    .truncate()
                    .grow(1.0),
            );
        if checked {
            line.child(icon(&CHECK, 12.0).color(color.ink).a11y_hidden())
        } else {
            line
        }
    }
}

fn picker_mut<'a>(state: &'a mut State, project: &str) -> Option<&'a mut Picker> {
    state
        .new_thread_drafts
        .get_mut(project)?
        .model_picker
        .as_mut()
}

pub(super) fn dismissal(project: &str, viewport: [f32; 2]) -> El {
    let project = project.to_owned();
    el().key("draft-model-dismiss")
        .absolute(-viewport[0], -viewport[1])
        .size(2.0 * viewport[0], 2.0 * viewport[1])
        .paint_order(2)
        .a11y_hidden()
        .on_background_press_tx(move |state: &mut State, tx| close(state, &project, tx))
}

fn escape(state: &mut State, project: &str, event: &KeyEvent, tx: &mut Tx) -> bool {
    if event.key == Key::Escape {
        close(state, project, tx);
        true
    } else {
        false
    }
}

fn close(state: &mut State, project: &str, tx: &mut Tx) {
    if let Some(draft) = state.new_thread_drafts.get_mut(project) {
        draft.model_picker = None;
    }
    tx.focus_when_shown(TRIGGER, tx.focus_visible());
}
