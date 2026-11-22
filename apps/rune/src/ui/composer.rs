use rune::color::hex;
use rune::{
    Color, El, Icon, PillStyle, TextStyle, Tx, col, edit_access, el, icon, row, text, text_input,
};

use crate::connection::{DraftProvider, ProviderMetadata, ProviderModel, Thread};

use super::conversation::{Layout, MAX_LANE};
use super::draft_hero;
use super::project::clone_banner;
use super::worktree_setup;
use super::{Draft, Palette, Project, SMALL, State, Workspace, button, error_text};

mod codex_effort;
mod draft_model_picker;
mod model_favorites;
mod model_picker;
mod resume;

pub(super) use draft_model_picker::Picker as DraftModelPicker;
pub(super) use model_favorites::Favorites as ModelFavorites;

pub(super) fn terminal_pill(ink: Color, palette: Palette) -> PillStyle {
    PillStyle::new()
        .icon(&TERMINAL_EXCERPT)
        .bg(palette.line)
        .fg(ink)
}

const FONT: TextStyle = TextStyle::new(14.0, 400.0).lh(22.75);
const PROVIDER_UNAVAILABLE: &str = "Enable a provider in Settings to send a message";

static TERMINAL_EXCERPT: Icon = Icon {
    name: "terminal-excerpt",
    view: 24.0,
    body: r#"<path d="m4 6 6 6-6 6m9 0h7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static SEND_UP: Icon = Icon {
    name: "composer-send",
    view: 14.0,
    body: r#"<path d="M7 11.5V2.5M7 2.5L3 6.5M7 2.5L11 6.5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static QUEUE: Icon = Icon {
    name: "composer-queue",
    view: 24.0,
    body: r#"<path d="M11 12H3m13-6H3m13 12H3m15-9v6m-3-3h6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static CHECK: Icon = Icon {
    name: "composer-update-queue",
    view: 24.0,
    body: r#"<path d="m20 6-11 11-5-5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static STOP: Icon = Icon {
    name: "composer-stop",
    view: 12.0,
    body: r#"<rect x="2" y="2" width="8" height="8" rx="1.5" fill="currentColor"/>"#,
};

pub(super) fn provider_unavailable(workspace: &Workspace, requested: Option<&str>) -> bool {
    workspace.providers.as_ref().is_some_and(|providers| {
        !providers.iter().any(|(id, metadata)| {
            !metadata.native_saved_thread_turns
                && metadata.selectable(requested == Some(id.as_str()))
        })
    })
}

pub(super) fn thread_provider_unavailable(workspace: &Workspace, thread: &Thread) -> bool {
    if let Some(provider) = native_claude_provider(workspace, thread) {
        return claude_model(thread, provider).is_none();
    }
    provider_unavailable(workspace, thread.providers.current_instance())
}

pub(super) fn draft_provider_unavailable(
    workspace: &Workspace,
    draft: &Draft,
    project: &str,
) -> bool {
    let claude_default = draft
        .selected_model()
        .is_some_and(|model| model.instance_id() == DraftProvider::Claude.instance_id())
        && workspace.providers.as_ref().is_some_and(|providers| {
            providers
                .get(DraftProvider::Claude.instance_id())
                .is_some_and(|provider| provider.native_root_launch)
        });
    if draft.model_choice.is_some() || claude_default {
        return !draft_model_picker::Models::new(workspace, draft, project)
            .is_some_and(|models| models.has_selection());
    }
    provider_unavailable(workspace, None)
}

pub(super) fn draft_full_access(workspace: &Workspace, draft: &Draft, project: &str) -> bool {
    draft.workspace_mode == super::WorkspaceMode::Local
        && draft_model_picker::Models::new(workspace, draft, project)
            .is_some_and(|models| models.full_access() && models.has_selection())
}

fn native_claude_provider<'a>(
    workspace: &'a Workspace,
    thread: &Thread,
) -> Option<&'a ProviderMetadata> {
    let id = thread.providers.current_instance()?;
    workspace
        .providers
        .as_ref()?
        .get(id)
        .filter(|provider| provider.driver == "claudeAgent" && provider.native_saved_thread_turns)
}

fn claude_model<'a>(thread: &Thread, provider: &'a ProviderMetadata) -> Option<&'a ProviderModel> {
    if thread.worktree_path.is_some() || thread.archived {
        return None;
    }
    provider.model(thread.providers.plain_claude_model()?)
}

pub(super) fn thread(
    workspace: &Workspace,
    thread_id: &str,
    draft: &Draft,
    layout: Layout,
    palette: Palette,
) -> El {
    let thread = workspace.thread(thread_id);
    let target = Target::Thread {
        id: thread_id,
        label: thread.map_or_else(
            || "Message".into(),
            |thread| format!("Message {}", thread.title),
        ),
    };
    col()
        .key(format!("composer-{thread_id}"))
        .w_full()
        .items_center()
        .px(layout.timeline_gutter())
        .pb(20.0)
        .child(surface(target, draft, workspace, thread, layout, palette))
}

pub(super) fn draft(
    state: &State,
    workspace: &Workspace,
    project: &Project,
    draft: &Draft,
    layout: Layout,
    palette: Palette,
) -> El {
    let composer = surface(
        Target::Draft(&project.id),
        draft,
        workspace,
        None,
        layout,
        palette,
    );
    if let Some(launch) = &draft.worktree_launch {
        return col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(worktree_setup::draft_transcript(
                launch, workspace, layout, palette,
            ))
            .child(
                col()
                    .w_full()
                    .items_center()
                    .px(layout.timeline_gutter())
                    .pb(20.0)
                    .child(composer),
            );
    }
    draft_hero::view(state, workspace, project, draft, layout, palette, composer)
}

enum Target<'a> {
    Draft(&'a str),
    Thread { id: &'a str, label: String },
}

#[derive(Clone, Copy)]
struct Colors {
    surface: Color,
    outline: Color,
    ink: Color,
    muted: Color,
    disabled_send: Color,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let [surface, outline, ink, muted, disabled_send] = if dark {
            ["#0e0e0e", "#1a1a1a", "#f1f3f7", "#a3a3a3", "#2a2a2a"]
        } else {
            ["#ffffff", "#ebebeb", "#18181b", "#71717b", "#e3e3e3"]
        }
        .map(|value| hex(value).expect("static composer palette"));
        Self {
            surface,
            outline,
            ink,
            muted,
            disabled_send,
        }
    }
}

fn surface(
    target: Target<'_>,
    draft: &Draft,
    workspace: &Workspace,
    thread: Option<&Thread>,
    layout: Layout,
    palette: Palette,
) -> El {
    let colors = Colors::new(palette.dark);
    let unavailable = match &target {
        Target::Draft(project) => draft_provider_unavailable(workspace, draft, project),
        Target::Thread { .. } => thread.map_or_else(
            || provider_unavailable(workspace, None),
            |thread| thread_provider_unavailable(workspace, thread),
        ),
    };
    let starting_thread = matches!(&target, Target::Draft(_));
    let gutter = if starting_thread && draft.worktree_launch.is_none() {
        draft_hero::gutter(layout)
    } else {
        layout.timeline_gutter()
    };
    let width = (layout.pane_width - 2.0 * gutter).min(MAX_LANE);
    let picker = match &target {
        Target::Draft(project)
            if draft.workspace_mode == super::WorkspaceMode::Local
                && workspace.scratch_project.as_deref() != Some(*project) =>
        {
            draft_model_picker::Models::new(workspace, draft, project).map(|models| {
                let overlay = draft.model_picker.as_ref().map(|open| {
                    (
                        draft_model_picker::dismissal(project, layout.viewport),
                        models.popup(project, open, width, palette),
                    )
                });
                (models.trigger(project, overlay.is_some(), palette), overlay)
            })
        }
        Target::Draft(_) => None,
        Target::Thread { .. } => thread.and_then(|thread| {
            let model = claude_model(thread, native_claude_provider(workspace, thread)?)?;
            let target = model_picker::Target::Thread(thread.id.clone());
            let open = workspace
                .selection
                .as_ref()
                .and_then(|selection| match &selection.view {
                    super::ProjectView::Conversation(conversation) => {
                        conversation.model_picker.as_ref()
                    }
                    _ => None,
                });
            let overlay = open.map(|query| {
                (
                    model_picker::dismissal(target.clone(), layout.viewport),
                    model_picker::popup(target.clone(), model, query, width, palette),
                )
            });
            Some((
                model_picker::trigger(target, model, overlay.is_some(), palette),
                overlay,
            ))
        }),
    };
    let project_id = match &target {
        Target::Draft(id) => Some(*id),
        Target::Thread { id, .. } => workspace
            .thread(id)
            .map(|thread| thread.project_id.as_str()),
    };
    let clone = project_id.and_then(|id| workspace.pending_clone(id));
    let has_draft = !draft.edit.text.trim().is_empty();
    let editing_queue =
        matches!(&target, Target::Thread { id, .. } if workspace.editing_queued_message(id));
    let queue =
        matches!(&target, Target::Thread { id, .. } if workspace.queue_worktree_message(id));
    let stop = match &target {
        Target::Thread { id, .. } => workspace
            .stop_generation(id)
            .map(|pending| (id.to_string(), pending)),
        Target::Draft(_) => None,
    };
    let resume = match &target {
        Target::Thread { id, .. } if !has_draft && !editing_queue && stop.is_none() => {
            resume::button(
                workspace,
                id,
                draft,
                palette,
                unavailable || clone.is_some(),
            )
        }
        _ => None,
    };
    let resuming = match &target {
        Target::Thread { id, .. } => resume::pending(workspace, id),
        Target::Draft(_) => false,
    };
    let workspace_ready = !starting_thread
        || (!draft.refs.switching
            && (draft.workspace_mode != super::WorkspaceMode::Worktree
                || draft.base_ref.is_some()));
    let can_send = has_draft
        && !workspace.reconnecting
        && !draft.pending
        && !resuming
        && (editing_queue || (!unavailable && clone.is_none()))
        && workspace_ready;
    let editor_width = (width - 34.0).max(1.0);
    let (_, height) = rune::text::measure(&draft.edit.text, &[], FONT, Some(editor_width));
    let mut editor = text_input(&draft.edit, FONT)
        .w_full()
        .h(height.clamp(45.5, 200.0))
        .placeholder(
            if unavailable {
                PROVIDER_UNAVAILABLE
            } else {
                "Ask anything"
            },
            colors.muted,
        )
        .color(colors.ink)
        .selection_color(palette.selected)
        .pill_style(move |id| {
            if id.starts_with("terminal_") {
                terminal_pill(colors.ink, palette)
            } else {
                PillStyle::new()
            }
        })
        .disabled(draft.pending && draft.worktree_launch.is_none());
    let submit: fn(&mut State, &mut Tx);
    match target {
        Target::Draft(project) => {
            let id = project.to_owned();
            editor = editor
                .key("initial-prompt")
                .label("Message")
                .on_edit(edit_access(move |state: &mut State| {
                    &mut state.new_thread_drafts.entry(id.clone()).or_default().edit
                }));
            submit = State::start_thread;
        }
        Target::Thread { id, label } => {
            let id = id.to_owned();
            editor =
                editor
                    .key("prompt")
                    .label(label)
                    .on_edit(edit_access(move |state: &mut State| {
                        &mut state.drafts.entry(id.clone()).or_default().edit
                    }));
            submit = State::submit_thread_prompt;
        }
    }
    let editor = editor.on_submit(submit);
    let mut footer = row().w_full().px(8.0).pb(8.0).center_y().gap(8.0);
    if starting_thread && draft.worktree_launch.is_none() {
        footer = footer.child(
            button(
                "cancel-new-thread",
                "Cancel",
                palette,
                false,
                !draft.pending,
                |state, _| state.cancel_new_thread(),
            )
            .h(32.0),
        );
    }
    footer = footer.child(el().grow(1.0));
    let overlay = if let Some((trigger, overlay)) = picker {
        footer = footer.child(trigger.mr(if width >= 420.0 { 36.0 } else { 0.0 }));
        overlay
    } else {
        None
    };
    if unavailable && has_draft && !editing_queue {
        footer = footer.child(
            text("No provider available")
                .font(SMALL)
                .color(colors.muted)
                .role("status")
                .truncate(),
        );
    }
    let (submit_key, idle, pending) = if starting_thread {
        ("start-thread", "Start thread", "Starting…")
    } else if editing_queue {
        ("send", "Update queued message", "Updating…")
    } else if queue {
        ("send", "Queue message", "Queuing…")
    } else {
        ("send", "Send", "Sending…")
    };
    let label = if draft.pending { pending } else { idle };
    let (symbol, symbol_size) = if editing_queue {
        (&CHECK, 16.0)
    } else if queue {
        (&QUEUE, 16.0)
    } else {
        (&SEND_UP, 14.0)
    };
    let help = if editing_queue {
        label
    } else {
        clone
            .map(clone_banner::send_block_reason)
            .unwrap_or(if unavailable {
                PROVIDER_UNAVAILABLE
            } else {
                label
            })
    };
    let send = el()
        .key(submit_key)
        .role("button")
        .label(label)
        .help(help)
        .size(32.0, 32.0)
        .center()
        .radius(16.0)
        .fill(if can_send {
            palette.accent
        } else {
            colors.disabled_send
        })
        .disabled(!can_send)
        .focusable(can_send)
        .focus_ring(2.0, 1.0, palette.accent)
        .on_press_tx(submit)
        .child(
            icon(symbol, symbol_size)
                .color(if can_send {
                    palette.on_accent
                } else {
                    colors.muted
                })
                .a11y_hidden(),
        );
    if let Some(resume) = resume {
        footer = footer.child(resume);
    } else if has_draft || editing_queue || stop.is_none() {
        footer = footer.child(send);
    }
    if let Some((thread_id, pending)) = stop.filter(|(_, pending)| !has_draft || *pending) {
        footer = footer.child(stop_button(&thread_id, pending, 32.0, palette));
    }
    let mut card = col()
        .key("composer-surface")
        .w_full()
        .radius(16.0)
        .fill(colors.surface)
        .border(1.0, colors.outline)
        .border_layout()
        .child(col().w_full().px(16.0).pt(16.0).pb(8.0).child(editor));
    if !palette.dark {
        card = card.shadow(1.0, 2.0, Color::BLACK.alpha(0.05));
    }
    let mut form = col().w_full().min_w(0.0).max_w(MAX_LANE).gap(8.0);
    if !starting_thread && let Some(clone) = clone {
        form = form.child(clone_banner::view(workspace, clone, palette));
    }
    if let Some(error) = &draft.failure {
        form = form.child(error_text(error, palette));
    }
    card = card.child(footer);
    if let Some((dismissal, popup)) = overlay {
        card = card.child(dismissal).child(popup);
    }
    form.child(card)
}

pub(super) fn stop_button(thread_id: &str, pending: bool, size: f32, palette: Palette) -> El {
    let thread_id = thread_id.to_owned();
    let destructive =
        hex(if palette.dark { "#ff6467" } else { "#e7000b" }).expect("destructive color");
    el().key("stop-generation")
        .role("button")
        .label(if pending {
            "Stopping"
        } else {
            "Stop generation"
        })
        .help(if pending { "Stopping…" } else { "Interrupt" })
        .size(size, size)
        .center()
        .radius(size / 2.0)
        .fill(destructive.alpha(if pending { 0.58 } else { 0.90 }))
        .hover(destructive)
        .disabled(pending)
        .focusable(!pending)
        .on_press(move |state: &mut State| state.stop_generation(&thread_id))
        .child(icon(&STOP, 12.0).color(Color::WHITE).a11y_hidden())
}
