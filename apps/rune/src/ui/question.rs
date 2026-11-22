use std::collections::{BTreeMap, HashMap};

use rune::color::{hex, oklch};
use rune::{
    Color, El, Icon, Key, KeyEvent, ScrollSpec, TextEdit, TextStyle, Tx, col, edit_access, el,
    icon, row, text, text_input,
};
use uuid::Uuid;

use crate::connection::{
    Answer, PendingQuestion, Question, QuestionOption, QuestionResponse, RuntimeResponseOutcome,
};

use super::conversation::{Layout, MAX_LANE};
use super::{Palette, ProjectView, State, Transcript, error_text};

const SMALL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const KEY: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);
const PROMPT: TextStyle = TextStyle::new(14.0, 400.0).lh(22.75);
const LABEL: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const DESCRIPTION: TextStyle = TextStyle::new(11.0, 400.0).lh(16.0);

static CHEVRON_UP: Icon = Icon {
    name: "question-toggle",
    view: 24.0,
    body: r#"<path d="m6 15 6-6 6 6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static CHEVRON_DOWN: Icon = Icon {
    name: "question-expand",
    view: 24.0,
    body: r#"<path d="m6 9 6 6 6-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(super) struct Questions {
    visible: Vec<PendingQuestion>,
    can_respond: bool,
    drafts: HashMap<String, QuestionDraft>,
}

#[derive(Default)]
struct QuestionDraft {
    index: usize,
    collapsed: bool,
    answers: HashMap<String, DraftAnswer>,
    submission: Submission,
}

struct DraftAnswer {
    edit: TextEdit,
    choices: Vec<String>,
}

#[derive(Default)]
enum Submission {
    #[default]
    Editable,
    Sending(Uuid),
    AwaitingProjection,
    Unconfirmed(QuestionResponse),
    Rejected(String),
}

impl DraftAnswer {
    fn new(initial_answer: &str) -> Self {
        Self {
            edit: TextEdit::new(initial_answer),
            choices: Vec::new(),
        }
    }

    fn resolve(&self, question: &Question) -> Option<Answer> {
        let custom = if question.has_literal_answer() {
            self.edit.text.as_str()
        } else {
            self.edit.text.trim()
        };
        if question.allows_custom_answer() && !custom.is_empty() {
            return Some(Answer::Text(custom.to_owned()));
        }
        if question.allows_empty_answer() {
            return Some(Answer::Text(String::new()));
        }
        let mut choices = self.choices.iter().filter(|value| {
            question
                .options
                .iter()
                .any(|option| option.value() == *value)
        });
        if let Some(first) = choices.next() {
            return Some(if question.multi_select {
                Answer::Choices(std::iter::once(first).chain(choices).cloned().collect())
            } else {
                Answer::Text(first.clone())
            });
        }
        question
            .has_literal_answer()
            .then(|| Answer::Text(String::new()))
    }
}

impl QuestionDraft {
    fn editable(&self) -> bool {
        matches!(
            self.submission,
            Submission::Editable | Submission::Rejected(_)
        )
    }

    fn answers(&self, request: &PendingQuestion) -> Option<BTreeMap<String, Answer>> {
        request
            .questions
            .iter()
            .map(|question| {
                Some((
                    question.id.clone(),
                    self.answers.get(&question.id)?.resolve(question)?,
                ))
            })
            .collect()
    }
}

impl Questions {
    pub(super) fn replace(&mut self, visible: Vec<PendingQuestion>, can_respond: bool) {
        self.drafts
            .retain(|id, _| visible.iter().any(|request| &request.id == id));
        for request in &visible {
            let draft = self.drafts.entry(request.id.clone()).or_default();
            draft.index = draft.index.min(request.questions.len().saturating_sub(1));
            for question in &request.questions {
                draft.answers.entry(question.id.clone()).or_insert_with(|| {
                    DraftAnswer::new(question.initial_answer.as_deref().unwrap_or_default())
                });
            }
        }
        self.visible = visible;
        self.can_respond = can_respond;
    }

    pub(super) fn pause(&mut self) {
        self.can_respond = false;
    }

    pub(super) fn finish(&mut self, response: QuestionResponse, outcome: RuntimeResponseOutcome) {
        let Some(draft) = self.drafts.get_mut(&response.request_id) else {
            return;
        };
        if !matches!(draft.submission, Submission::Sending(id) if id == response.command_id) {
            return;
        }
        draft.submission = match outcome {
            RuntimeResponseOutcome::Accepted => Submission::AwaitingProjection,
            RuntimeResponseOutcome::Rejected(message) => Submission::Rejected(message),
            RuntimeResponseOutcome::Unconfirmed => Submission::Unconfirmed(response),
        };
    }

    pub(super) fn has_active_question(&self) -> bool {
        self.current().is_some()
    }

    fn current(&self) -> Option<(&PendingQuestion, &QuestionDraft, &Question)> {
        let request = self.visible.first()?;
        let draft = self.drafts.get(&request.id)?;
        Some((request, draft, request.questions.get(draft.index)?))
    }

    fn current_mut(&mut self) -> Option<(&PendingQuestion, &mut QuestionDraft)> {
        let request = self.visible.first()?;
        Some((request, self.drafts.get_mut(&request.id)?))
    }
}

pub(super) struct AutoAdvance {
    thread_id: String,
    request_id: String,
    due: f64,
}

impl State {
    fn active_question_thread(&self) -> Option<&str> {
        let workspace = self.workspace.as_ref()?;
        match &workspace.selection.as_ref()?.view {
            ProjectView::Conversation(conversation)
                if matches!(conversation.transcript, Transcript::Loaded(_)) =>
            {
                Some(&conversation.id)
            }
            _ => None,
        }
    }

    fn can_answer_question(&self, thread_id: &str) -> bool {
        self.active_question_thread() == Some(thread_id)
            && self.connection.is_some()
            && self.workspace.as_ref().is_some_and(|workspace| {
                workspace.prompt_writes
                    && !workspace.reconnecting
                    && !workspace
                        .thread(thread_id)
                        .is_some_and(|thread| thread.archived)
            })
            && self
                .questions
                .get(thread_id)
                .is_some_and(|questions| questions.can_respond)
    }

    fn select_question_option(
        &mut self,
        thread_id: &str,
        request_id: &str,
        index: usize,
        tx: &mut Tx,
    ) -> bool {
        if !self.can_answer_question(thread_id) {
            return false;
        }
        let Some((request, draft)) = self
            .questions
            .get_mut(thread_id)
            .and_then(Questions::current_mut)
        else {
            return false;
        };
        let Some(question) = request.questions.get(draft.index) else {
            return false;
        };
        let Some(option) = question.options.get(index) else {
            return false;
        };
        if draft.collapsed || !request.live || request.id != request_id || !draft.editable() {
            return false;
        }
        let answer = draft
            .answers
            .get_mut(&question.id)
            .expect("draft for current question");
        let displaced = answer.edit.text.trim().to_owned();
        if !displaced.is_empty() {
            answer.edit = TextEdit::new("");
            let edit = &mut self.drafts.entry(thread_id.to_owned()).or_default().edit;
            let old = edit.text.trim_end();
            let text = if old.is_empty() {
                displaced
            } else {
                format!("{old}\n\n{displaced}")
            };
            *edit = TextEdit::new(text);
        }
        let value = option.value();
        if question.multi_select {
            if let Some(index) = answer.choices.iter().position(|choice| choice == value) {
                answer.choices.remove(index);
            } else {
                answer.choices.push(value.to_owned());
            }
        } else {
            answer.choices = vec![value.to_owned()];
            let due = tx.clock() + 0.2;
            self.question_auto_advance = Some(AutoAdvance {
                thread_id: thread_id.to_owned(),
                request_id: request_id.to_owned(),
                due,
            });
            tx.wake_at(due);
        }
        draft.submission = Submission::Editable;
        true
    }

    fn advance_question(&mut self, thread_id: &str, request_id: &str) -> bool {
        if !self.can_answer_question(thread_id) {
            return false;
        }
        let Some((request, draft)) = self
            .questions
            .get_mut(thread_id)
            .and_then(Questions::current_mut)
        else {
            return false;
        };
        if !request.live || request.id != request_id {
            return false;
        }
        self.question_auto_advance = None;
        let response = match &draft.submission {
            Submission::Unconfirmed(response) => response.clone(),
            Submission::Editable | Submission::Rejected(_) => {
                let question = &request.questions[draft.index];
                if draft
                    .answers
                    .get(&question.id)
                    .and_then(|answer| answer.resolve(question))
                    .is_none()
                {
                    return false;
                }
                if draft.index + 1 < request.questions.len() {
                    draft.index += 1;
                    draft.collapsed = false;
                    return true;
                }
                let Some(answers) = draft.answers(request) else {
                    return false;
                };
                QuestionResponse {
                    thread_id: thread_id.to_owned(),
                    request_id: request_id.to_owned(),
                    command_id: Uuid::new_v4(),
                    answers,
                }
            }
            Submission::Sending(_) | Submission::AwaitingProjection => return false,
        };
        draft.submission = Submission::Sending(response.command_id);
        if let Some(connection) = &self.connection {
            connection.respond_to_question(response);
        }
        true
    }

    fn edit_question(&mut self, thread_id: &str, request_id: &str, question_id: &str) {
        self.question_auto_advance = None;
        if let Some((request, draft)) = self
            .questions
            .get_mut(thread_id)
            .and_then(Questions::current_mut)
            && request.id == request_id
            && draft.editable()
            && let Some(answer) = draft.answers.get_mut(question_id)
            && !answer.edit.text.trim().is_empty()
        {
            answer.choices.clear();
            draft.submission = Submission::Editable;
        }
    }

    pub(super) fn poll_question(&mut self, clock: f64) -> bool {
        if !self
            .question_auto_advance
            .as_ref()
            .is_some_and(|timer| clock >= timer.due)
        {
            return false;
        }
        let Some(timer) = self.question_auto_advance.take() else {
            return false;
        };
        self.advance_question(&timer.thread_id, &timer.request_id)
    }

    pub(super) fn question_shortcut(&mut self, event: &KeyEvent, tx: &mut Tx) -> bool {
        let Key::Text(digit) = &event.key else {
            return false;
        };
        if digit.len() != 1
            || event.mods.cmd
            || event.mods.ctrl
            || event.mods.alt
            || tx.key_repeat()
            || self.add_project.is_some()
            || tx.focused().is_some_and(|key| {
                matches!(
                    key,
                    "question-answer" | "prompt" | "thread-search" | "initial-prompt"
                ) || key.starts_with("terminal-")
            })
        {
            return false;
        }
        let Some(index) = digit
            .parse::<usize>()
            .ok()
            .filter(|digit| (1..=9).contains(digit))
        else {
            return false;
        };
        let Some(thread_id) = self.active_question_thread().map(str::to_owned) else {
            return false;
        };
        let Some(request_id) = self
            .questions
            .get(&thread_id)
            .and_then(Questions::current)
            .map(|(request, _, _)| request.id.clone())
        else {
            return false;
        };
        self.select_question_option(&thread_id, &request_id, index - 1, tx)
    }
}

#[derive(Clone, Copy)]
struct Colors {
    surface: Color,
    border: Color,
    ink: Color,
    muted: Color,
    key: Color,
    radio: Color,
    primary: Color,
    disabled_primary: Color,
    disabled_primary_ink: Color,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let gray = |lightness| oklch(lightness, 0.0, 0.0);
        let (surface, border, ink, muted, radio, primary) = if dark {
            (
                hex("#0e0e0e").expect("static question palette"),
                gray(0.218),
                gray(0.97),
                gray(0.708),
                Color::WHITE.alpha(0.0352),
                oklch(0.571, 0.21, 264.0),
            )
        } else {
            (
                Color::WHITE,
                gray(0.94),
                oklch(0.21, 0.006, 285.885),
                oklch(0.552, 0.016, 285.938),
                gray(0.992),
                oklch(0.488, 0.217, 264.0),
            )
        };
        Self {
            surface,
            border,
            ink,
            muted,
            key: ink.alpha(0.07),
            radio,
            primary,
            disabled_primary: hex(if dark { "#26499f" } else { "#6d8de6" })
                .expect("static question palette"),
            disabled_primary_ink: if dark { gray(0.731) } else { Color::WHITE },
        }
    }
}

pub(super) fn view(state: &State, thread_id: &str, layout: Layout, palette: Palette) -> Option<El> {
    let questions = state.questions.get(thread_id)?;
    let (request, draft, question) = questions.current()?;
    let answer = draft.answers.get(&question.id)?;
    let can_respond = request.live && state.can_answer_question(thread_id);
    let editable = draft.editable() && can_respond;
    let colors = Colors::new(palette.dark);
    let drawer = drawer(
        thread_id, request, draft, question, answer, editable, colors,
    );
    let width = (layout.pane_width - 2.0 * layout.timeline_gutter()).min(MAX_LANE);
    let surface = surface(
        thread_id,
        request,
        draft,
        question,
        answer,
        can_respond,
        width,
        palette,
        state
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.stop_generation(thread_id)),
    );
    let form = col()
        .w_full()
        .min_w(0.0)
        .max_w(MAX_LANE)
        .key(format!("question-composer-{thread_id}"))
        .child(col().w_full().px(22.0).child(drawer))
        .child(surface);
    Some(
        col()
            .w_full()
            .items_center()
            .px(layout.timeline_gutter())
            .pb(20.0)
            .child(form),
    )
}

fn drawer(
    thread_id: &str,
    request: &PendingQuestion,
    draft: &QuestionDraft,
    question: &Question,
    answer: &DraftAnswer,
    editable: bool,
    colors: Colors,
) -> El {
    let label = if draft.collapsed {
        "Show the question and its options"
    } else {
        "Hide the question and its options"
    };
    let thread = thread_id.to_owned();
    let request_id = request.id.clone();
    let mut header = row()
        .key(format!("question-toggle-{}", request.id))
        .w_full()
        .h(36.0)
        .px(16.0)
        .gap(6.0)
        .center_y()
        .radius(12.0)
        .hover(colors.key)
        .role("button")
        .label(label)
        .help(label)
        .expanded(!draft.collapsed)
        .focusable(true)
        .focus_ring(1.0, -1.0, colors.primary)
        .on_press(move |state: &mut State| {
            if let Some((request, draft)) = state
                .questions
                .get_mut(&thread)
                .and_then(Questions::current_mut)
                && request.id == request_id
            {
                draft.collapsed = !draft.collapsed;
            }
        })
        .child(
            text(&question.header)
                .font(SMALL)
                .color(colors.muted)
                .truncate(),
        );
    header = if draft.collapsed {
        header
            .child(text("·").font(SMALL).color(colors.muted))
            .child(
                text(&question.question)
                    .font(SMALL)
                    .color(colors.ink.alpha(0.8))
                    .grow(1.0)
                    .truncate(),
            )
    } else {
        header.child(el().grow(1.0))
    };
    if request.questions.len() > 1 {
        header = header.child(
            text(format!("{}/{}", draft.index + 1, request.questions.len()))
                .font(SMALL)
                .color(colors.muted),
        );
    }
    let chevron = if draft.collapsed {
        &CHEVRON_UP
    } else {
        &CHEVRON_DOWN
    };
    let mut card = col()
        .key(format!("question-drawer-{}", request.id))
        .w_full()
        .role("group")
        .label(&question.header)
        .padding(4.0)
        .corners([16.0, 16.0, 0.0, 0.0])
        .fill(colors.surface)
        .border_sides([1.0, 1.0, 0.0, 1.0], colors.border)
        .child(header.child(icon(chevron, 14.0).color(colors.muted).a11y_hidden()));
    if draft.collapsed {
        return card;
    }
    let mut content = col().w_full().pl(16.0).pr(16.0).pt(2.0).pb(4.0).child(
        text(&question.question)
            .key("active-question-text")
            .font(PROMPT)
            .color(colors.ink.alpha(0.85))
            .pl(6.0)
            .wrap_anywhere()
            .selectable(true),
    );
    if !question.options.is_empty() {
        let choice_group = ChoiceGroup {
            thread_id,
            request_id: &request.id,
            question,
            answer,
            enabled: editable,
            colors,
        };
        content = content.child(
            col().w_full().pt(6.0).children(
                question
                    .options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| choice_group.option(index, option)),
            ),
        );
    }
    card = card.child(
        col()
            .key("question-scroll")
            .w_full()
            .max_h(286.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(content),
    );
    card
}

struct ChoiceGroup<'a> {
    thread_id: &'a str,
    request_id: &'a str,
    question: &'a Question,
    answer: &'a DraftAnswer,
    enabled: bool,
    colors: Colors,
}

impl ChoiceGroup<'_> {
    fn option(&self, index: usize, option: &QuestionOption) -> El {
        let colors = self.colors;
        let selected = self
            .answer
            .choices
            .iter()
            .any(|choice| choice == option.value());
        let indicator = el()
            .size(16.0, 16.0)
            .radius(if self.question.multi_select { 4.0 } else { 8.0 })
            .fill(if selected {
                colors.primary
            } else {
                colors.radio
            })
            .border(
                1.0,
                if selected {
                    colors.primary
                } else {
                    colors.ink.alpha(0.55)
                },
            )
            .center()
            .a11y_hidden();
        let indicator = if selected {
            indicator.child(el().size(6.0, 6.0).radius(3.0).fill(Color::WHITE))
        } else {
            indicator
        };
        let thread_id = self.thread_id.to_owned();
        let request_id = self.request_id.to_owned();
        let mut option_text = col().min_w(0.0).grow(1.0).gap(2.0).child(
            text(&option.label)
                .font(LABEL)
                .color(colors.ink.alpha(if selected { 1.0 } else { 0.7 }))
                .wrap_anywhere(),
        );
        if !option.description.is_empty() && option.description != option.label {
            option_text = option_text.child(
                text(&option.description)
                    .font(DESCRIPTION)
                    .color(colors.ink.alpha(0.8))
                    .wrap_anywhere(),
            );
        }
        let mut option_row = row()
            .key(format!("question-option-{index}"))
            .w_full()
            .py(6.0)
            .pl(6.0)
            .gap(10.0)
            .radius(6.0)
            .hover(colors.key)
            .role("button")
            .label(&option.label)
            .checked(selected)
            .disabled(!self.enabled)
            .focusable(self.enabled)
            .focus_ring(1.0, -1.0, colors.primary)
            .on_press_tx(move |state: &mut State, tx| {
                state.select_question_option(&thread_id, &request_id, index, tx);
            })
            .child(row().h(20.0).center_y().child(indicator))
            .child(option_text);
        if index < 9 {
            let keycap = el()
                .size(20.0, 20.0)
                .radius(4.0)
                .fill(colors.key)
                .center()
                .child(text(format!("{}", index + 1)).font(KEY).color(colors.muted));
            option_row =
                option_row.child(el().size(24.0, 20.0).center().a11y_hidden().child(keycap));
        }
        if let Some(preview) = &option.preview {
            col().w_full().child(option_row).child(
                text(preview)
                    .font(SMALL)
                    .color(colors.ink.alpha(0.7))
                    .pl(32.0)
                    .pb(6.0)
                    .wrap_anywhere()
                    .selectable(true),
            )
        } else {
            option_row
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn surface(
    thread_id: &str,
    request: &PendingQuestion,
    draft: &QuestionDraft,
    question: &Question,
    answer: &DraftAnswer,
    can_respond: bool,
    width: f32,
    palette: Palette,
    stop: Option<bool>,
) -> El {
    let colors = Colors::new(palette.dark);
    let (thread, request_id, question_id) = (
        thread_id.to_owned(),
        request.id.clone(),
        question.id.clone(),
    );
    let retrying = matches!(draft.submission, Submission::Unconfirmed(_));
    let sending = matches!(
        draft.submission,
        Submission::Sending(_) | Submission::AwaitingProjection
    );
    let last = draft.index + 1 == request.questions.len();
    let can_submit = can_respond
        && (retrying
            || draft.editable()
                && if last {
                    draft.answers(request).is_some()
                } else {
                    answer.resolve(question).is_some()
                });
    let editor = if question.allows_custom_answer() {
        let placeholder = if question.options.is_empty() {
            "Type your answer"
        } else {
            "Type your own answer, or leave this blank to use the selected option"
        };
        let (_, height) = rune::text::measure(
            &answer.edit.text,
            &[],
            PROMPT,
            Some((width - 34.0).max(1.0)),
        );
        let height = height.clamp(45.5, 200.0);
        if can_respond && draft.editable() {
            let (edit_thread, edit_request, edit_question) =
                (thread.clone(), request_id.clone(), question_id.clone());
            let (change_thread, change_request, change_question) =
                (thread.clone(), request_id.clone(), question_id);
            let (send_thread, send_request) = (thread.clone(), request_id.clone());
            text_input(&answer.edit, PROMPT)
                .key("question-answer")
                .w_full()
                .h(height)
                .placeholder(placeholder, colors.muted)
                .color(colors.ink)
                .label("Answer to the question")
                .selection_color(palette.selected)
                .on_edit(edit_access(move |state: &mut State| {
                    &mut state
                        .questions
                        .entry(edit_thread.clone())
                        .or_default()
                        .drafts
                        .entry(edit_request.clone())
                        .or_default()
                        .answers
                        .entry(edit_question.clone())
                        .or_insert_with(|| DraftAnswer::new(""))
                        .edit
                }))
                .on_input_change(move |state: &mut State, _| {
                    state.edit_question(&change_thread, &change_request, &change_question)
                })
                .on_submit(move |state: &mut State, _| {
                    state.advance_question(&send_thread, &send_request);
                })
        } else {
            let value = if answer.edit.text.is_empty() {
                placeholder
            } else {
                &answer.edit.text
            };
            col()
                .h(height)
                .child(text(value).font(PROMPT).color(colors.muted).wrap_anywhere())
        }
    } else {
        col().h(45.5).child(
            text("Choose an option above")
                .font(PROMPT)
                .color(colors.muted),
        )
    };
    let label = if retrying {
        "Retry"
    } else if sending {
        "Submitting…"
    } else if last {
        "Submit"
    } else {
        "Next"
    };
    let mut footer = row()
        .w_full()
        .px(8.0)
        .py(8.0)
        .gap(8.0)
        .center_y()
        .child(el().grow(1.0));
    if draft.index > 0 {
        let (prev_thread, prev_request) = (thread.clone(), request_id.clone());
        footer = footer.child(
            el().key("question-previous")
                .h(28.0)
                .px(14.0)
                .center()
                .radius(14.0)
                .role("button")
                .label("Previous question")
                .hover(colors.key)
                .focusable(can_respond && draft.editable())
                .disabled(!can_respond || !draft.editable())
                .focus_ring(2.0, 1.0, colors.primary)
                .on_press(move |state: &mut State| {
                    state.question_auto_advance = None;
                    if state.can_answer_question(&prev_thread)
                        && let Some((request, draft)) = state
                            .questions
                            .get_mut(&prev_thread)
                            .and_then(Questions::current_mut)
                        && request.id == prev_request
                        && draft.editable()
                    {
                        draft.index = draft.index.saturating_sub(1);
                        draft.collapsed = false;
                    }
                })
                .child(text("Previous").font(LABEL).color(colors.ink)),
        );
    }
    let submit = el()
        .key("question-submit")
        .h(28.0)
        .px(12.0)
        .center()
        .radius(14.0)
        .fill(if can_submit {
            colors.primary
        } else {
            colors.disabled_primary
        })
        .role("button")
        .label(if label == "Submit" {
            "Submit answer"
        } else {
            label
        })
        .disabled(!can_submit)
        .focusable(can_submit)
        .focus_ring(2.0, 1.0, colors.primary)
        .on_press(move |state: &mut State| {
            state.advance_question(&thread, &request_id);
        })
        .child(text(label).font(LABEL).color(if can_submit {
            Color::WHITE
        } else {
            colors.disabled_primary_ink
        }));
    let mut card = col()
        .key("question-surface")
        .w_full()
        .radius(16.0)
        .fill(colors.surface)
        .border(1.0, colors.border)
        .border_layout()
        .child(col().w_full().px(16.0).pt(16.0).pb(8.0).child(editor));
    let error = match &draft.submission {
        Submission::Unconfirmed(_) => {
            Some("The answer could not be confirmed. Retry to send the same answer.")
        }
        Submission::Rejected(message) => Some(message.as_str()),
        _ if !request.live => {
            Some("Provider process is gone. Interrupt or restart the run to respond.")
        }
        _ if !can_respond => Some("This session cannot respond to requests."),
        _ => None,
    };
    if let Some(error) = error {
        card = card.child(col().w_full().px(16.0).child(error_text(error, palette)));
    }
    if !palette.dark {
        card = card.shadow(1.0, 2.0, Color::BLACK.alpha(0.05));
    }
    let footer = footer.child(submit);
    card.child(match stop {
        Some(pending) => footer.child(super::composer::stop_button(
            thread_id, pending, 28.0, palette,
        )),
        None => footer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kept_question_drafts_encode_the_provider_values_and_preserve_failed_submission() {
        let question: Question = serde_json::from_value(json!({
            "id": "preference", "header": "Fixture", "question": "Which ones?",
            "options": [
                {"label": "First", "description": "First", "value": "a"},
                {"label": "Second", "description": "Second", "value": "b"}
            ], "multiSelect": true
        }))
        .unwrap();
        let request = PendingQuestion {
            id: "request".into(),
            questions: vec![question],
            live: true,
        };
        let mut threads = Questions::default();
        threads.replace(vec![request.clone()], true);
        let (_, draft) = threads.current_mut().unwrap();
        let answer = draft.answers.get_mut("preference").unwrap();
        answer.choices = vec!["b".into(), "deleted-option".into(), "a".into()];
        assert_eq!(
            answer.resolve(&request.questions[0]),
            Some(Answer::Choices(vec!["b".into(), "a".into()]))
        );
        answer.edit = TextEdit::new("  custom answer  ");
        let answers = draft.answers(&request).unwrap();
        assert_eq!(answers["preference"], Answer::Text("custom answer".into()));
        let response = QuestionResponse {
            thread_id: "thread".into(),
            request_id: request.id.clone(),
            command_id: Uuid::new_v4(),
            answers,
        };
        draft.submission = Submission::Sending(response.command_id);
        threads.finish(response.clone(), RuntimeResponseOutcome::Unconfirmed);
        threads.pause();
        threads.replace(vec![request], true);
        let (_, draft, _) = threads.current().unwrap();
        assert!(
            matches!(&draft.submission, Submission::Unconfirmed(retry) if retry.command_id == response.command_id && retry.answers == response.answers)
        );
    }
}
