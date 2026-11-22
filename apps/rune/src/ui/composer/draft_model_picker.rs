use std::borrow::Cow;

use rune::{
    Color, El, Key, KeyEvent, ScrollSpec, TextEdit, Tx, col, edit_access, el, icon, row, text,
    text_input,
};

use crate::connection::{
    DraftProvider, ModelSelection, ProviderMetadata, ProviderModel, ProviderReadiness,
    WorkspaceMode,
};

use t3_rune::providers::codex::{ReasoningEffort, ServiceTier};

use super::super::sidebar::provider_badge;
use super::super::{Draft, Palette, State, Thread, Workspace};
use super::codex_effort::{self, EffortPicker};
use super::model_favorites::{self, Favorites};
use super::model_picker::{self, CHECK, Colors, DETAILS, LABEL, SEARCH};
use super::saved_speed::{self, SavedSpeed};

const TRIGGER: &str = "draft-model-picker";
const QUERY: &str = "draft-model-search";
const EFFORT: &str = "draft-model-effort";
const PROVIDERS: [DraftProvider; 2] = [DraftProvider::Codex, DraftProvider::Claude];

#[derive(Clone)]
pub(super) enum Target {
    Draft(String),
    Saved(String),
}

impl Target {
    fn slot<'a>(&self, state: &'a mut State) -> Option<&'a mut Option<Picker>> {
        match self {
            Self::Draft(project) => {
                Some(&mut state.new_thread_drafts.get_mut(project)?.model_picker)
            }
            Self::Saved(thread) => Some(&mut state.conversation_mut(thread)?.saved_model_picker),
        }
    }

    fn picker<'a>(&self, state: &'a mut State) -> Option<&'a mut Picker> {
        self.slot(state)?.as_mut()
    }

    pub(super) fn choose_reasoning(&self, state: &mut State, effort: ReasoningEffort) {
        match self {
            Self::Draft(project) => state.choose_codex_reasoning(project, effort),
            Self::Saved(thread) => state.choose_saved_reasoning(thread, effort),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Tab {
    Favorites,
    Provider(DraftProvider),
}

impl Tab {
    fn key(self) -> &'static str {
        match self {
            Self::Favorites => "favorites",
            Self::Provider(provider) => provider.instance_id(),
        }
    }
}

pub(in crate::ui) struct Picker {
    tab: Tab,
    expanded: [bool; 2],
    query: TextEdit,
}

impl Picker {
    fn new(provider: DraftProvider, is_legacy: bool, favorites: bool) -> Self {
        let mut expanded = [false; 2];
        expanded[provider_index(provider)] = is_legacy;
        Self {
            tab: if favorites {
                Tab::Favorites
            } else {
                Tab::Provider(provider)
            },
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
            && selected.reasoning_effort().is_none_or(|effort| {
                self.model
                    .reasoning_levels()
                    .is_some_and(|levels| levels.options.contains(&effort))
            })
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
    codex_effort: Option<EffortPicker>,
    saved_speed: Option<SavedSpeed<'a>>,
    saved_service_tier: Option<ServiceTier>,
    favorites: &'a Favorites,
    permissions: Option<&'static str>,
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
        let codex_effort = selected
            .filter(|choice| {
                choice.provider == DraftProvider::Codex
                    && choice.model.slug == ReasoningEffort::MODEL_ID
            })
            .and_then(|choice| {
                EffortPicker::for_model(
                    choice.model,
                    draft
                        .selected_model()
                        .and_then(ModelSelection::reasoning_effort),
                )
            });
        Some(Self {
            selected,
            codex_effort,
            saved_speed: None,
            saved_service_tier: None,
            choices,
            favorites: &workspace.model_favorites,
            permissions: policy.full_access().then_some("Full access"),
        })
    }

    pub(super) fn for_saved(workspace: &'a Workspace, thread: &'a Thread) -> Option<Self> {
        let selection = thread.providers.codex_model()?;
        let metadata = workspace
            .providers
            .as_ref()?
            .get(DraftProvider::Codex.instance_id())?;
        if metadata.driver != "codex" || metadata.readiness() != ProviderReadiness::Ready {
            return None;
        }
        let model = metadata.model(selection.model())?;
        let selected = Choice {
            provider: DraftProvider::Codex,
            metadata,
            model,
        };
        let reported_tier = model.default_service_tier();
        let default_tier = reported_tier.and_then(|tier| ServiceTier::for_model(&model.slug, tier));
        let saved_service_tier = selection.service_tier().or(default_tier);
        let codex_effort = (default_tier.is_some() || reported_tier.is_none())
            .then(|| EffortPicker::for_model(model, selection.reasoning_effort()))
            .flatten();
        let saved_speed = codex_effort
            .as_ref()
            .and_then(EffortPicker::current)
            .and_then(|_| SavedSpeed::for_model(model, saved_service_tier));
        Some(Self {
            choices: metadata
                .models()
                .iter()
                .map(|model| Choice {
                    provider: DraftProvider::Codex,
                    metadata,
                    model,
                })
                .collect(),
            selected: Some(selected),
            codex_effort,
            saved_speed,
            saved_service_tier,
            favorites: &workspace.model_favorites,
            permissions: thread.providers.permissions_label(),
        })
    }

    pub(super) fn select_effort(&self, effort: ReasoningEffort) -> Option<ModelSelection> {
        self.codex_effort
            .as_ref()
            .filter(|picker| picker.supports(effort))?;
        let model = self.selected?.model;
        Some(ModelSelection::codex_with_reasoning(
            &model.slug,
            effort,
            self.saved_service_tier,
        ))
    }

    pub(super) fn select_speed(&self, tier: ServiceTier) -> Option<ModelSelection> {
        self.saved_speed.as_ref()?;
        let effort = self.codex_effort.as_ref()?.current()?;
        Some(ModelSelection::codex_with_reasoning(
            &self.selected?.model.slug,
            effort,
            Some(tier),
        ))
    }

    pub(super) fn select_model(
        &self,
        model: &str,
        remembered: Option<ModelSelection>,
    ) -> Option<ModelSelection> {
        let choice = self
            .choices
            .iter()
            .find(|choice| choice.model.slug == model)?;
        let selected = remembered.filter(|selection| {
            selection.reasoning_effort().is_some()
                && choice.matches(selection)
                && selection
                    .service_tier()
                    .is_none_or(|tier| choice.model.service_tier_choice(tier).is_some())
        });
        Some(selected.unwrap_or_else(|| ModelSelection::plain(DraftProvider::Codex, model)))
    }

    pub(super) fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    pub(super) fn full_access(&self) -> bool {
        self.permissions == Some("Full access")
    }

    pub(super) fn trigger(&self, target: &Target, open: bool, palette: Palette) -> El {
        let color = Colors::new(palette.dark);
        let target = target.clone();
        let initial = self.selected.unwrap_or(self.choices[0]);
        let provider = initial.provider;
        let favorites = self.choices.iter().any(|choice| {
            self.favorites
                .contains(choice.provider.instance_id(), &choice.model.slug)
        });
        let legacy = self.selected.is_some_and(|choice| choice.model.is_legacy);
        let saved = matches!(&target, Target::Saved(_));
        let name = self
            .selected
            .map_or(Cow::Borrowed("Choose a model"), |choice| {
                if saved {
                    choice.model.codex_short_name()
                } else {
                    Cow::Borrowed(
                        choice
                            .model
                            .short_name
                            .as_deref()
                            .unwrap_or(&choice.model.name),
                    )
                }
            });
        let claude = self
            .selected
            .is_some_and(|choice| choice.provider == DraftProvider::Claude);
        let effort = self
            .codex_effort
            .as_ref()
            .and_then(EffortPicker::current)
            .map(ReasoningEffort::label)
            .or_else(|| {
                saved
                    .then(|| initial.model.default_reasoning().map(|(_, label)| label))
                    .flatten()
            });
        let fast = self.saved_speed.as_ref().and_then(SavedSpeed::fast_label);
        let label = if let Some(effort) = effort {
            fast.map_or_else(
                || format!("{}, {effort}", initial.model.name),
                |speed| format!("{}, {effort} {speed}", initial.model.name),
            )
        } else if claude {
            format!("{}, High · 200k", initial.model.name)
        } else {
            self.selected
                .map_or_else(|| name.to_string(), |choice| choice.model.name.clone())
        };
        let width = if claude {
            183.0
        } else {
            let width = rune::text::measure(&name, &[], LABEL, None).0 + 42.0;
            let suffix = effort.map_or(0.0, |effort| {
                rune::text::measure(effort, &[], LABEL, None).0 + 4.0
            });
            let speed = if fast.is_some() { 18.0 } else { 0.0 };
            (width + suffix + speed).clamp(78.0, 183.0)
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
                if let Some(picker) = target.slot(state)
                    && picker.take().is_none()
                {
                    *picker = Some(Picker::new(provider, legacy, favorites));
                    tx.focus_when_shown(QUERY, false);
                }
            });
        if let Some(choice) = self.selected {
            trigger = trigger.child(choice.mark(palette, color, 16.0));
        }
        trigger = trigger.child(text(name.as_ref()).font(LABEL).color(color.ink).truncate());
        if let Some(effort) = effort {
            trigger = trigger.child(text(effort).font(LABEL).color(color.muted));
            if fast.is_some() {
                trigger = trigger.child(saved_speed::bolt(color.muted).ml(2.0));
            }
        } else if claude {
            trigger = trigger
                .child(text("High").font(LABEL).color(color.muted))
                .child(text("200k").font(DETAILS).color(color.ink));
        }
        trigger
    }

    pub(super) fn popup(
        &self,
        target: &Target,
        picker: &Picker,
        lane: f32,
        palette: Palette,
    ) -> El {
        let color = Colors::new(palette.dark);
        let edit_target = target.clone();
        let input_target = target.clone();
        let needle = picker.query.text.trim().to_lowercase();
        let searching = !needle.is_empty();
        let expanded = match picker.tab {
            Tab::Favorites => false,
            Tab::Provider(provider) => picker.expanded[provider_index(provider)],
        };
        let mut rows = col().w_full().gap(2.0);
        let mut results = 0;
        let mut legacy = false;
        let shelf = picker.tab == Tab::Favorites && !searching;
        let mut choices = self.choices.clone();
        if !searching && !shelf {
            choices.sort_by_key(|choice| {
                !self
                    .favorites
                    .contains(choice.provider.instance_id(), &choice.model.slug)
            });
        }
        for choice in choices {
            let favorite = self
                .favorites
                .contains(choice.provider.instance_id(), &choice.model.slug);
            if searching {
                if !choice.model.name.to_lowercase().contains(&needle)
                    && !choice.model.slug.to_lowercase().contains(&needle)
                {
                    continue;
                }
            } else if shelf {
                if !favorite {
                    continue;
                }
            } else {
                if Tab::Provider(choice.provider) != picker.tab {
                    continue;
                }
                let collapsed = choice.model.is_legacy && !favorite;
                legacy |= collapsed;
                if collapsed && !expanded {
                    continue;
                }
            }
            rows = rows.child(self.model_row(target, choice, shelf, palette, color));
            results += 1;
        }
        if legacy {
            let target = target.clone();
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
                        if let Some(picker) = target.picker(state)
                            && let Tab::Provider(provider) = picker.tab
                        {
                            let index = provider_index(provider);
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
            rows = rows.child(
                text(if shelf {
                    "No favorites yet"
                } else {
                    "No matching models"
                })
                .font(DETAILS)
                .color(color.muted),
            );
        }
        let mut list = col()
            .min_w(0.0)
            .min_h(0.0)
            .grow(1.0)
            .gap(12.25)
            .padding(10.0)
            .pl(13.0)
            .pr(12.0)
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
                                &mut edit_target.picker(state).expect("open model picker").query
                            }))
                            .on_input_key(move |state: &mut State, event, tx| {
                                escape(state, &input_target, event, tx)
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
        if let Some(error) = self.favorites.error(color) {
            list = list.child(error);
        }
        let width = model_picker::popup_width(lane);
        let columns = width >= 420.0;
        let details = if let Some(effort) = &self.codex_effort {
            let model = self.selected.expect("Codex reasoning model").model;
            let name = if matches!(target, Target::Saved(_)) {
                model.codex_short_name()
            } else {
                Cow::Borrowed(model.name.as_str())
            };
            effort.view(
                target,
                &name,
                if columns { 192.0 } else { width - 45.0 },
                palette,
                QUERY,
            )
        } else if let Some(choice) = self
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
        } else if matches!(target, Target::Saved(_))
            && let Some(choice) = self.selected
            && choice.model.default_reasoning().is_some()
        {
            codex_effort::reported_defaults(
                choice.model,
                if columns { 192.0 } else { width - 45.0 },
                palette,
            )
        } else {
            el().w(192.0).a11y_hidden()
        };
        let mut body = row().w_full().min_h(0.0).grow(1.0);
        if !searching {
            body = body.child(self.provider_tabs(target, picker, palette, color));
        }
        body = if columns {
            body.child(list)
                .child(details.border_sides([0.0, 0.0, 0.0, 1.0], color.line))
        } else {
            body.child(col().grow(1.0).min_h(0.0).child(list).child(details))
        };
        let speed = match target {
            Target::Saved(thread) => self
                .saved_speed
                .as_ref()
                .map(|speed| speed.view(thread, palette)),
            Target::Draft(_) => None,
        };
        let target = target.clone();
        let frame = if speed.is_some() {
            model_picker::popup_frame(
                "draft-model-popup",
                "Models",
                lane,
                palette,
                body.focus_group(),
            )
        } else {
            model_picker::popup_frame("draft-model-popup", "Models", lane, palette, body)
                .focus_group()
        }
        .on_key(move |state: &mut State, event, tx| escape(state, &target, event, tx));
        if self.permissions.is_some() || speed.is_some() {
            frame.child(model_picker::footer(self.permissions, speed, color))
        } else {
            frame
        }
    }

    fn provider_tabs(
        &self,
        target: &Target,
        picker: &Picker,
        palette: Palette,
        color: Colors,
    ) -> El {
        let mut side = col()
            .w(44.0)
            .h_full()
            .padding(4.0)
            .pt(5.25)
            .gap(4.0)
            .border_sides([0.0, 1.0, 0.0, 0.0], color.line)
            .child(provider_tab(
                target,
                Tab::Favorites,
                "Favorites",
                model_favorites::tab_icon(color),
                picker,
                palette,
            ))
            .child(el().w_full().h(1.0).fill(color.line).a11y_hidden());
        for provider in PROVIDERS {
            let Some(choice) = self
                .choices
                .iter()
                .find(|choice| choice.provider == provider)
            else {
                continue;
            };
            let (name, _) = provider_badge::identity(choice.metadata, palette.dark, color.ink);
            side = side.child(provider_tab(
                target,
                Tab::Provider(provider),
                &name,
                choice.mark(palette, color, 20.0),
                picker,
                palette,
            ));
        }
        side
    }

    fn model_row(
        &self,
        target: &Target,
        choice: Choice<'_>,
        shelf: bool,
        palette: Palette,
        color: Colors,
    ) -> El {
        let checked = self.selected.is_some_and(|selected| {
            choice.provider == selected.provider && choice.model.slug == selected.model.slug
        });
        let target = target.clone();
        let current_effort = self.codex_effort.as_ref().and_then(EffortPicker::current);
        let provider = choice.provider;
        let slug = choice.model.slug.clone();
        let legacy = choice.model.is_legacy;
        let codex_levels = (!matches!(&target, Target::Draft(_))
            || slug == ReasoningEffort::MODEL_ID)
            .then(|| choice.model.reasoning_levels())
            .flatten();
        let action = self
            .favorites
            .action(provider.instance_id(), &slug, shelf, palette, color);
        let mut line = row()
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
            .hover_group()
            .on_press_tx(move |state: &mut State, tx| {
                let selected = match &target {
                    Target::Draft(project) => {
                        if !checked {
                            state.choose_draft_model(project, provider, &slug);
                        }
                        state
                            .new_thread_drafts
                            .get(project)
                            .and_then(Draft::selected_model)
                            .and_then(ModelSelection::reasoning_effort)
                    }
                    Target::Saved(thread_id) => {
                        if checked {
                            current_effort
                        } else {
                            state.choose_saved_model(thread_id, &slug)
                        }
                    }
                };
                let Some(picker) = target.picker(state) else {
                    return;
                };
                if picker.tab != Tab::Favorites {
                    picker.tab = Tab::Provider(provider);
                }
                picker.expanded[provider_index(provider)] |= legacy;
                if provider == DraftProvider::Claude {
                    tx.focus_when_shown(EFFORT, tx.focus_visible());
                } else if let Some(levels) = &codex_levels {
                    let effort = selected
                        .filter(|value| levels.options.contains(value))
                        .or(levels.default)
                        .unwrap_or(levels.options[0]);
                    tx.focus_when_shown(codex_effort::key(effort), tx.focus_visible());
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
            line = line.child(icon(&CHECK, 12.0).color(color.ink).a11y_hidden());
        }
        line.child(action.mr(if shelf { 48.0 } else { 0.0 }))
    }
}

fn provider_tab(
    target: &Target,
    tab: Tab,
    label: &str,
    artwork: El,
    picker: &Picker,
    palette: Palette,
) -> El {
    let target = target.clone();
    let color = Colors::new(palette.dark);
    let selected = picker.tab == tab;
    el().key(format!("draft-model-provider-{}", tab.key()))
        .role("tab")
        .label(label)
        .checked(selected)
        .size(36.0, 36.0)
        .center()
        .radius(5.0)
        .focusable(true)
        .focus_ring(1.0, -1.0, palette.accent)
        .fill(if selected {
            color.control
        } else {
            Color::TRANSPARENT
        })
        .hover(color.hover)
        .on_press_tx(move |state: &mut State, tx| {
            if let Some(picker) = target.picker(state) {
                picker.tab = tab;
                tx.scroll_to_instant("draft-model-rows", 0.0, 0.0);
                tx.focus_when_shown(QUERY, tx.focus_visible());
            }
        })
        .child(artwork)
}

pub(super) fn dismissal(target: &Target, viewport: [f32; 2]) -> El {
    let target = target.clone();
    el().key("draft-model-dismiss")
        .absolute(-viewport[0], -viewport[1])
        .size(2.0 * viewport[0], 2.0 * viewport[1])
        .paint_order(2)
        .a11y_hidden()
        .on_background_press_tx(move |state: &mut State, tx| close(state, &target, tx))
}

fn escape(state: &mut State, target: &Target, event: &KeyEvent, tx: &mut Tx) -> bool {
    if event.key == Key::Escape {
        close(state, target, tx);
        true
    } else {
        false
    }
}

fn close(state: &mut State, target: &Target, tx: &mut Tx) {
    if let Some(picker) = target.slot(state) {
        *picker = None;
    }
    tx.focus_when_shown(TRIGGER, tx.focus_visible());
}
