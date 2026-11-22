use rune::{El, Key, col, el, icon, row, text};

use t3_rune::providers::codex::ReasoningEffort;

use crate::connection::{ProviderModel, ReasoningLevels};

use super::super::{Palette, State};
use super::model_picker::{CHECK, Colors, DETAILS, LABEL};

pub(super) struct EffortPicker {
    levels: ReasoningLevels,
    current: Option<ReasoningEffort>,
}

impl EffortPicker {
    pub(super) fn for_model(
        model: &ProviderModel,
        selection: Option<ReasoningEffort>,
    ) -> Option<Self> {
        let levels = model.reasoning_levels()?;
        let current = selection
            .filter(|effort| levels.options.contains(effort))
            .or(levels.default);
        Some(Self { levels, current })
    }

    pub(super) fn current(&self) -> Option<ReasoningEffort> {
        self.current
    }

    pub(super) fn view(
        &self,
        project: &str,
        model: &str,
        width: f32,
        palette: Palette,
        query_key: &'static str,
    ) -> El {
        let color = Colors::new(palette.dark);
        let mut rows = col()
            .w_full()
            .gap(2.0)
            .role("radiogroup")
            .label("Reasoning")
            .focus_group();
        for (index, &effort) in self.levels.options.iter().enumerate() {
            let selected = self.current == Some(effort);
            let is_default = self.levels.default == Some(effort);
            let radio = key(effort);
            let previous = key(self.levels.options[index.saturating_sub(1)]);
            let next = key(self.levels.options[(index + 1).min(self.levels.options.len() - 1)]);
            let label = effort.label();
            let project = project.to_owned();
            let keyboard_project = project.clone();
            let mut option = row()
                .key(radio)
                .role("radio")
                .label(if is_default {
                    format!("{label}, Default")
                } else {
                    label.to_owned()
                })
                .checked(selected)
                .focus_selected(selected)
                .focusable(true)
                .focus_ring(1.0, -1.0, palette.accent)
                .w_full()
                .h(32.0)
                .px(8.0)
                .center_y()
                .gap(8.0)
                .radius(5.0)
                .hover(color.hover)
                .on_press_tx(move |state: &mut State, _| {
                    state.choose_codex_reasoning(&project, effort)
                })
                .on_key(move |state: &mut State, event, tx| {
                    match event.key {
                        Key::Up => tx.focus_when_shown(previous.clone(), true),
                        Key::Down => tx.focus_when_shown(next.clone(), true),
                        Key::Left => tx.focus_when_shown(query_key, true),
                        Key::Enter => {
                            if !tx.key_repeat() {
                                state.choose_codex_reasoning(&keyboard_project, effort);
                            }
                        }
                        _ => return false,
                    }
                    true
                })
                .child(text(label).font(LABEL).color(color.ink).grow(1.0));
            if selected {
                option = option.child(icon(&CHECK, 14.0).color(color.ink).a11y_hidden());
            } else if is_default {
                option = option.child(text("Default").font(DETAILS).color(color.muted));
            }
            rows = rows.child(option);
        }
        col()
            .key("draft-codex-reasoning")
            .w(width)
            .min_w(0.0)
            .px(8.0)
            .pt(10.0)
            .gap(12.25)
            .child(
                row()
                    .w_full()
                    .h(32.0)
                    .center_y()
                    .border_sides([0.0, 0.0, 1.0, 0.0], color.line)
                    .child(text("Reasoning").font(LABEL).color(color.muted).grow(1.0))
                    .child(text(model).font(DETAILS).color(color.muted)),
            )
            .child(rows)
            .child(el().grow(1.0))
    }
}

pub(super) fn key(effort: ReasoningEffort) -> String {
    format!("draft-codex-effort-{}", effort.as_str())
}

impl State {
    fn allows_codex_reasoning(&self, project: &str, effort: ReasoningEffort) -> bool {
        let Some(draft) = self.new_thread_drafts.get(project) else {
            return false;
        };
        let Some(workspace) = self.workspace.as_ref() else {
            return false;
        };
        let provider = crate::connection::DraftProvider::Codex;
        draft.workspace_mode == crate::connection::WorkspaceMode::Local
            && workspace.scratch_project.as_deref() != Some(project)
            && draft
                .model_policy
                .as_ref()
                .is_some_and(|policy| policy.allows(provider, ReasoningEffort::MODEL_ID))
            && workspace
                .providers
                .as_ref()
                .and_then(|providers| providers.get(provider.instance_id()))
                .filter(|metadata| {
                    metadata.driver == provider.instance_id()
                        && metadata.readiness() == crate::connection::ProviderReadiness::Ready
                })
                .and_then(|metadata| metadata.model(ReasoningEffort::MODEL_ID))
                .and_then(ProviderModel::reasoning_levels)
                .is_some_and(|levels| levels.options.contains(&effort))
    }

    pub(super) fn choose_draft_model(
        &mut self,
        project: &str,
        provider: crate::connection::DraftProvider,
        model: &str,
    ) {
        use crate::connection::{DraftProvider, ModelSelection};

        let remembered = (provider == DraftProvider::Codex && model == ReasoningEffort::MODEL_ID)
            .then(|| self.client_settings.sticky_codex_reasoning())
            .flatten()
            .filter(|effort| self.allows_codex_reasoning(project, *effort));
        let selection = remembered.map_or_else(
            || ModelSelection::plain(provider, model),
            ModelSelection::codex_reasoning,
        );
        self.set_draft_model_choice(project, selection);
    }

    fn choose_codex_reasoning(&mut self, project: &str, effort: ReasoningEffort) {
        if self.allows_codex_reasoning(project, effort) {
            self.set_draft_model_choice(
                project,
                crate::connection::ModelSelection::codex_reasoning(effort),
            );
        }
    }

    fn set_draft_model_choice(&mut self, project: &str, choice: crate::connection::ModelSelection) {
        let Some(draft) = self
            .new_thread_drafts
            .get_mut(project)
            .filter(|draft| draft.model_picker.is_some() && !draft.pending)
        else {
            return;
        };
        let effort = choice.reasoning_effort();
        draft.model_choice = Some(choice);
        if let Some(environment) = self.draft_environment.as_deref()
            && let Err(error) =
                self.client_settings
                    .remember_codex_reasoning(environment, project, effort)
        {
            draft.failure = Some(format!("Could not remember the reasoning effort. {error}"));
        }
    }

    pub(in crate::ui) fn clear_draft_reasoning(&mut self, project: &str) {
        if let Some(environment) = self.draft_environment.as_deref()
            && let Err(error) =
                self.client_settings
                    .remember_codex_reasoning(environment, project, None)
        {
            eprintln!("Could not clear the reasoning effort for the launched draft. {error}");
        }
    }
}
