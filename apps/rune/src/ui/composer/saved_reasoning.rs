use rune::El;
use uuid::Uuid;

use t3_rune::providers::codex::{ReasoningEffort, ServiceTier};

use crate::connection::{ModelSelection, Thread};

use super::super::{Conversation, Palette, ProjectView, State, Workspace};
use super::Layout;
use super::draft_model_picker::{self, Models, Target};

pub(super) fn controls(
    workspace: &Workspace,
    thread: &Thread,
    layout: Layout,
    width: f32,
    palette: Palette,
) -> Option<(El, Option<(El, El)>)> {
    let conversation = available(workspace, &thread.id)?;
    let models = Models::for_saved(workspace, thread)?;
    let target = Target::Saved(thread.id.clone());
    let overlay = conversation.saved_model_picker.as_ref().map(|open| {
        (
            draft_model_picker::dismissal(&target, layout.viewport),
            models.popup(&target, open, width, palette),
        )
    });
    Some((models.trigger(&target, overlay.is_some(), palette), overlay))
}

fn available<'a>(workspace: &'a Workspace, thread_id: &str) -> Option<&'a Conversation> {
    let ProjectView::Conversation(conversation) = &workspace.selection.as_ref()?.view else {
        return None;
    };
    (!workspace.reconnecting
        && conversation.id == thread_id
        && conversation.saved_reasoning_available
        && conversation.queued_edit.is_none())
    .then_some(conversation)
}

impl State {
    pub(in crate::ui) fn choose_saved_reasoning(
        &mut self,
        thread_id: &str,
        effort: ReasoningEffort,
    ) {
        self.edit_saved_options(thread_id, |models| models.select_effort(effort));
    }

    pub(super) fn choose_saved_speed(&mut self, thread_id: &str, tier: ServiceTier) {
        self.edit_saved_options(thread_id, |models| models.select_speed(tier));
    }

    pub(super) fn choose_saved_model(
        &mut self,
        thread_id: &str,
        model: &str,
    ) -> Option<ReasoningEffort> {
        let remembered = self.client_settings.saved_codex_options(model).cloned();
        self.edit_saved_model(thread_id, |models| models.select_model(model, remembered))?
            .reasoning_effort()
    }

    fn edit_saved_options(
        &mut self,
        thread_id: &str,
        pick: impl FnOnce(&Models<'_>) -> Option<ModelSelection>,
    ) {
        if let Some(selection) = self.edit_saved_model(thread_id, pick)
            && let Err(error) = self.client_settings.remember_saved_codex_options(selection)
        {
            self.project_notice = Some(format!("Could not remember model options. {error}"));
        }
    }

    fn edit_saved_model(
        &mut self,
        thread_id: &str,
        pick: impl FnOnce(&Models<'_>) -> Option<ModelSelection>,
    ) -> Option<ModelSelection> {
        let workspace = self.workspace.as_ref()?;
        available(workspace, thread_id)?
            .saved_model_picker
            .as_ref()?;
        let thread = workspace.thread(thread_id)?;
        let selection = pick(&Models::for_saved(workspace, thread)?)?;
        let draft = self
            .drafts
            .get_mut(thread_id)
            .filter(|draft| !draft.pending && draft.pending_saved_reasoning.is_none())?;
        let operation = self
            .connection
            .as_ref()?
            .set_saved_codex_model(thread_id.to_owned(), selection.clone())?;
        draft.pending_saved_reasoning = Some(operation);
        Some(selection)
    }

    pub(in crate::ui) fn settle_saved_reasoning(
        &mut self,
        thread_id: &str,
        operation: Uuid,
        result: Result<(), String>,
    ) {
        let Some(draft) = self
            .drafts
            .get_mut(thread_id)
            .filter(|draft| draft.pending_saved_reasoning == Some(operation))
        else {
            return;
        };
        draft.pending_saved_reasoning = None;
        if let Err(message) = result {
            self.project_notice = Some(format!("Could not apply model selection. {message}"));
        }
    }
}
