use rune::El;
use uuid::Uuid;

use t3_rune::providers::codex::ReasoningEffort;

use crate::connection::{DraftProvider, ModelSelection, Thread};

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
        self.edit_saved_model(thread_id, |models| {
            models
                .supports_effort(effort)
                .then(|| ModelSelection::codex_reasoning(effort))
        });
    }

    pub(in crate::ui) fn choose_saved_model(&mut self, thread_id: &str, model: &str) {
        self.edit_saved_model(thread_id, |models| {
            models
                .contains_model(model)
                .then(|| ModelSelection::plain(DraftProvider::Codex, model))
        });
    }

    fn edit_saved_model(
        &mut self,
        thread_id: &str,
        pick: impl FnOnce(&Models<'_>) -> Option<ModelSelection>,
    ) {
        let Some(workspace) = self.workspace.as_ref() else {
            return;
        };
        if !available(workspace, thread_id)
            .is_some_and(|conversation| conversation.saved_model_picker.is_some())
        {
            return;
        }
        let Some(selection) = workspace
            .thread(thread_id)
            .and_then(|thread| Models::for_saved(workspace, thread))
            .and_then(|models| pick(&models))
        else {
            return;
        };
        let Some(draft) = self
            .drafts
            .get_mut(thread_id)
            .filter(|draft| !draft.pending && draft.pending_saved_reasoning.is_none())
        else {
            return;
        };
        draft.pending_saved_reasoning = self.connection.as_ref().and_then(|connection| {
            connection.set_saved_codex_model(thread_id.to_owned(), selection)
        });
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
