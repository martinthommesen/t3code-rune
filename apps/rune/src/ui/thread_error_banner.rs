use std::collections::HashSet;

use rune::color::{hex, oklch};
use rune::{Color, El, Icon, Key, TextStyle, col, el, icon, row, text};
use serde_json::Value;

use super::conversation::Layout;
use super::{ConversationEntry, Palette, State, Thread, Transcript};

const WORKSPACE_PREPARATION_FAILED: &str = "workspace_preparation_failed";
const CHATGPT_USAGE_LIMIT: &str = "subscription_sharing_usage_limit_exceeded";
const CHATGPT_WORKSPACE_USAGE_LIMIT: &str =
    "subscription_sharing_workplace_individual_usage_limit_exceeded";
const MESSAGE: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const DISCLOSURE: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);

static ALERT: Icon = Icon {
    name: "circle-alert",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 8v4m0 4h.01"/></g>"#,
};

static DISMISS: Icon = Icon {
    name: "x",
    view: 24.0,
    body: r#"<path d="m18 6-12 12M6 6l12 12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Dismissal {
    environment: String,
    thread: String,
    source_id: String,
    completed_at: Option<String>,
    message: String,
}

#[derive(Default)]
pub(super) struct Session {
    dismissed: HashSet<Dismissal>,
    expanded: Option<Dismissal>,
}

impl Session {
    pub(super) fn reset_expansion(&mut self) {
        self.expanded = None;
    }

    fn dismiss(&mut self, error: Dismissal) {
        if self.expanded.as_ref() == Some(&error) {
            self.reset_expansion();
        }
        self.dismissed.insert(error);
    }

    fn toggle_expansion(&mut self, error: &Dismissal) {
        self.expanded = (self.expanded.as_ref() != Some(error)).then(|| error.clone());
    }
}

fn current_failure(
    environment: &str,
    thread: &Thread,
    transcript: &Transcript,
) -> Option<Dismissal> {
    if thread.status != "failed"
        || thread.error.class.as_deref() == Some("usage_limit")
        || matches!(
            thread.error.code.as_deref(),
            Some(CHATGPT_USAGE_LIMIT | CHATGPT_WORKSPACE_USAGE_LIMIT)
        )
    {
        return None;
    }
    let message = thread
        .error
        .message
        .as_deref()
        .filter(|message| !message.is_empty())?;
    let run_id = thread.latest_run_id()?;
    let (source_id, completed_at) =
        if thread.error.code.as_deref() == Some(WORKSPACE_PREPARATION_FAILED) {
            let Transcript::Loaded(entries) = transcript else {
                return None;
            };
            let failure = entries.iter().rev().find_map(|entry| {
                let ConversationEntry::WorkspaceFailure(failure) = entry else {
                    return None;
                };
                (failure.thread_id.as_deref() == Some(thread.id.as_str())
                    && failure.run_id.as_deref() == Some(run_id)
                    && failure.message() == Some(message))
                .then_some(failure)
            })?;
            (failure.id.as_str(), failure.completed_at.as_deref())
        } else {
            (run_id, thread.latest_run_completed_at())
        };
    Some(Dismissal {
        environment: environment.to_owned(),
        thread: thread.id.clone(),
        source_id: source_id.to_owned(),
        completed_at: completed_at.map(str::to_owned),
        message: message.to_owned(),
    })
}

fn compact_message(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|data| {
            data["error"]["message"]
                .as_str()
                .filter(|message| !message.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| raw.to_owned())
}

pub(super) fn view(
    state: &State,
    thread: Option<&Thread>,
    transcript: &Transcript,
    layout: Layout,
    palette: Palette,
) -> Option<El> {
    let error = current_failure(state.draft_environment.as_deref()?, thread?, transcript)?;
    let session = &state.thread_error_banner;
    if session.dismissed.contains(&error) {
        return None;
    }
    let collapsed = compact_message(&error.message);
    let expandable = collapsed != error.message
        || error.message.encode_utf16().count() > 180
        || error.message.contains('\n');
    let expanded = session.expanded.as_ref() == Some(&error);
    let message = if expanded { &error.message } else { &collapsed };
    let (foreground, red, glass, muted, hover, blur) = if palette.dark {
        (
            oklch(0.704, 0.191, 22.216),
            hex("#fb414a").expect("static error color"),
            "#0c0404",
            hex("#a3a3a3").expect("static muted color"),
            Color::WHITE.alpha(0.04),
            16.0,
        )
    } else {
        (
            oklch(0.505, 0.213, 27.518),
            oklch(0.637, 0.237, 25.331),
            "#fdf3f4",
            hex("#71717b").expect("static muted color"),
            hex("#f4f4f5").expect("static hover color"),
            12.0,
        )
    };
    let compact = layout.viewport[0] < 640.0;
    let control_top = if compact { 4.0 } else { 0.0 };
    let mut content = col()
        .min_w(0.0)
        .grow(1.0)
        .mt(if compact { 4.0 } else { 2.0 })
        .child(
            text(message)
                .key("thread-error-banner-text")
                .font(MESSAGE)
                .color(foreground.alpha(0.8))
                .wrap_anywhere()
                .max_lines(if expandable && !expanded { 3 } else { 0 })
                .selectable(true),
        );
    if expandable {
        let disclosure_error = error.clone();
        let keyboard_error = disclosure_error.clone();
        content = content.child(
            el().key("thread-error-banner-expand")
                .role("button")
                .checked(expanded)
                .focusable(true)
                .focus_ring(2.0, 1.0, palette.accent)
                .mt(4.0)
                .on_press(move |state: &mut State| {
                    state
                        .thread_error_banner
                        .toggle_expansion(&disclosure_error);
                })
                .on_key(move |state: &mut State, event, tx| {
                    if event.key != Key::Enter {
                        return false;
                    }
                    if !tx.key_repeat() {
                        state.thread_error_banner.toggle_expansion(&keyboard_error);
                    }
                    true
                })
                .child(
                    text(if expanded {
                        "Show less"
                    } else {
                        "Show full error"
                    })
                    .font(DISCLOSURE)
                    .color(muted),
                ),
        );
    }
    let keyboard_error = error.clone();
    let dismiss = el()
        .key("dismiss-thread-error")
        .role("button")
        .label("Dismiss error")
        .size(24.0, 24.0)
        .min_w(24.0)
        .mt(control_top)
        .center()
        .radius(8.0)
        .hover(hover)
        .focusable(true)
        .focus_ring(2.0, 1.0, palette.accent)
        .on_press(move |state: &mut State| state.thread_error_banner.dismiss(error.clone()))
        .on_key(move |state: &mut State, event, tx| {
            if event.key != Key::Enter {
                return false;
            }
            if !tx.key_repeat() {
                state.thread_error_banner.dismiss(keyboard_error.clone());
            }
            true
        })
        .child(icon(&DISMISS, 14.0).color(muted).a11y_hidden());
    let banner = row()
        .key("thread-error-banner")
        .role("alert")
        .max_w((layout.pane_width - 32.0).clamp(0.0, 768.0))
        .radius(12.0)
        .border(1.0, red.alpha(0.32))
        .border_layout()
        .px(14.0)
        .py(12.0)
        .gap(8.0)
        .items_start()
        .fill(hex(glass).expect("combined sRGB glass tint").alpha(0.808))
        .backdrop_blur(blur)
        .child(
            icon(&ALERT, 16.0)
                .color(red)
                .min_w(16.0)
                .mt(if compact { 6.0 } else { 4.0 })
                .a11y_hidden(),
        )
        .child(content)
        .child(dismiss);
    Some(
        row()
            .pin(Some(0.0), Some(layout.header_height + 12.0), None, None)
            .w(layout.pane_width)
            .center()
            .child(banner),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn thread() -> Thread {
        serde_json::from_value(json!({
            "id":"thread",
            "title":"Example",
            "projectId":"project",
            "status":"failed",
            "latestRunId":"run",
            "latestRunCompletedAt":"2026-01-23T12:00:00Z",
            "lastError":"Setup script failed.",
            "lastErrorCode":WORKSPACE_PREPARATION_FAILED,
        }))
        .unwrap()
    }

    fn failure(thread_id: &str, run_id: &str, completed_at: &str, retryable: bool) -> Transcript {
        let mut failure: crate::connection::WorkspaceFailure = serde_json::from_value(json!({
            "id":"failed-item",
            "status":"failed",
            "threadId":thread_id,
            "runId":run_id,
            "completedAt":completed_at,
            "failure":{"message":"Setup script failed."},
        }))
        .unwrap();
        failure.retryable = retryable;
        Transcript::Loaded(vec![ConversationEntry::WorkspaceFailure(Box::new(failure))])
    }

    #[test]
    fn the_banner_filters_older_and_inherited_failures_and_reopens_for_a_new_failure() {
        let mut thread = thread();
        let before = "2026-01-23T12:00:00Z";
        let after = "2026-01-23T12:01:00Z";
        let mut session = Session::default();
        let original = failure("thread", "run", before, true);
        let current = |thread: &Thread, transcript: &Transcript| {
            current_failure("environment", thread, transcript)
        };
        let error = current(&thread, &original).unwrap();
        assert_eq!(error.source_id, "failed-item");
        session.dismiss(error);
        assert!(
            session
                .dismissed
                .contains(&current(&thread, &original).unwrap())
        );
        assert!(
            !session
                .dismissed
                .contains(&current_failure("other", &thread, &original).unwrap())
        );
        assert!(current(&thread, &failure("other", "run", before, true)).is_none());
        assert!(current(&thread, &failure("thread", "older-run", before, true)).is_none());
        assert!(current(&thread, &failure("thread", "run", before, false)).is_some());
        let later = current(&thread, &failure("thread", "run", after, true)).unwrap();
        assert!(!session.dismissed.contains(&later));
        thread.status = "preparing".into();
        assert!(current(&thread, &original).is_none());
        thread.status = "failed".into();
        thread.error.class = Some("usage_limit".into());
        assert!(current(&thread, &original).is_none());
    }

    #[test]
    fn the_compact_banner_extracts_nonempty_nested_messages() {
        let raw = r#"{"error":{"message":"Provider rejected the run","code":"invalid_input"}}"#;
        assert_eq!(compact_message(raw), "Provider rejected the run");
        let empty = r#"{"error":{"message":" "}}"#;
        assert_eq!(compact_message(empty), empty);
        assert_eq!(compact_message("Setup failed"), "Setup failed");
    }
}
