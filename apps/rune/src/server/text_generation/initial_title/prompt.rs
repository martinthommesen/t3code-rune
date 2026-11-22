use serde::Deserialize;
use serde_json::{Value, json};

const TRUNCATED: &str = "\n[Content truncated]\n";
const BOOTSTRAP_PROMPT: &str = "[User attached one or more files without additional text. Respond using the conversation context and the attached files.]";

pub(super) fn attachment_only(message: &str) -> bool {
    let message = message
        .get(..11)
        .filter(|prefix| prefix.eq_ignore_ascii_case("ultrathink:"))
        .map_or(message, |_| message[11..].trim_start_matches(js_whitespace));
    message == BOOTSTRAP_PROMPT
}

pub(super) fn initial(message: &str, project: &str, instructions: &str) -> String {
    let project = project.trim_matches(js_whitespace);
    let instructions = instructions.trim_matches(js_whitespace);
    let length = if instructions.is_empty() {
        "- 3-8 words, fewer than 40 characters."
    } else {
        "- Follow the user's title instructions at the end of this prompt. Where they conflict with these rules, including on length and format, they win."
    };
    let avoid = if project.is_empty() {
        "- Avoid project names already visible in the UI, quotes, labels, filler, and trailing punctuation.".into()
    } else {
        format!(
            "- Never include the project name ({}); the UI already shows it next to the title. Avoid quotes, labels, filler, and trailing punctuation.",
            json!(project)
        )
    };
    let message = limit_message(message);
    let mut prompt = format!(
        r#"Generate a title that will help the user recognize this T3 Code thread weeks later.
Return JSON with keys title and needsRefinement.
Set needsRefinement to true only if the subject is still unknown, such as an unresolved link, "fix this", or an unexplained attachment. Otherwise set it to false.

Before answering, silently reduce the request to:
- Subject: What system, feature, or problem is this really about?
- Outcome: What does the user ultimately want to understand or change?
- Incidental instructions: What only describes how the agent should do the work?

Title the subject and outcome. Discard incidental instructions.

Editorial rules:
{length}
- Use a compact noun phrase or clear action phrase.
- Capture the umbrella goal when the request lists several symptoms or steps.
- When asked about thread behavior, never include the referenced titles' topics. "[Fix login] why didn't this thread start?" means "Thread Startup Failure". Use linked topics only for requests about that work.
- Name the product change, not the mock, plan, report, branch, or PR used to produce it.
- Models, subagents, tools, output formats, and monitoring instructions do not belong in the title unless they are themselves the topic.
- For reviews, name what is being reviewed and the relevant concern. Avoid generic titles such as "Review PR 123" when linked or attached context reveals the subject.
- For research, name the question domain rather than the requested research process.
- Do not claim the work is complete.
- Do not copy and truncate the user's message.
{avoid}
- Use attached images as primary context for UI issues.
- When a URL or attachment is the only source of the subject, use available tools to inspect it directly.
- Local git history is not evidence of what a linked PR or issue is about. Never title the thread after branch names, commit messages, or merged commits found in the checkout.
- If a linked PR or issue cannot be read, fall back to the user's stated action plus its number, such as "Take Over PR 8588". This is the one case where a PR or issue number belongs in the title.

User message:
{message}"#
    );
    if !instructions.is_empty() {
        let cutoff = prefix_end(instructions, 20_000);
        prompt.push_str("\n\nTitle instructions from the user:\n");
        prompt.push_str(&instructions[..cutoff]);
        if cutoff < instructions.len() {
            prompt.push_str("\n\n[truncated]");
        }
    }
    prompt
}

pub(super) fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"title": {"type": "string"}, "needsRefinement": {"type": "boolean"}},
        "required": ["title", "needsRefinement"],
        "additionalProperties": false
    })
}

#[derive(Deserialize)]
struct Output {
    title: String,
    #[serde(rename = "needsRefinement", default)]
    _needs_refinement: bool,
}

#[derive(Deserialize)]
struct WrappedTitle {
    title: String,
}

pub(super) fn generated(value: Value) -> Result<Option<String>, ()> {
    let output = serde_json::from_value::<Output>(value).map_err(|_| ())?;
    let wrapped = serde_json::from_str::<WrappedTitle>(&output.title).ok();
    let value = wrapped
        .as_ref()
        .map_or(output.title.as_str(), |title| &title.title);
    let first_line = value
        .trim_matches(js_whitespace)
        .split('\n')
        .next()
        .unwrap_or("");
    let first_line = first_line.trim_matches(js_whitespace);
    let unwrapped = ['\'', '"', '`'].into_iter().find_map(|quote| {
        let inner = first_line.strip_prefix(quote)?.strip_suffix(quote)?;
        (!inner.contains(quote) && !inner.contains(['\r', '\n', '\u{2028}', '\u{2029}']))
            .then_some(inner)
    });
    let normalized = unwrapped
        .unwrap_or(first_line)
        .trim_matches(js_whitespace)
        .split(js_whitespace)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.eq_ignore_ascii_case("title") {
        return Err(());
    }
    if normalized.is_empty() || normalized == "New thread" {
        return Ok(None);
    }
    if normalized.encode_utf16().count() > 120 {
        let cutoff = prefix_end(&normalized, 117);
        return Ok(Some(format!(
            "{}...",
            normalized[..cutoff].trim_end_matches(js_whitespace)
        )));
    }
    Ok(Some(normalized))
}

fn limit_message(message: &str) -> std::borrow::Cow<'_, str> {
    const BUDGET: usize = 8_000;
    if message.encode_utf16().count() <= BUDGET {
        return message.into();
    }
    let available = BUDGET - TRUNCATED.len();
    let head = prefix_end(message, available.div_ceil(2));
    let mut units = 0;
    let mut tail = message.len();
    for (offset, ch) in message.char_indices().rev() {
        units += ch.len_utf16();
        if units > available / 2 {
            break;
        }
        tail = offset;
    }
    format!("{}{TRUNCATED}{}", &message[..head], &message[tail..]).into()
}

fn prefix_end(value: &str, budget: usize) -> usize {
    let mut units = 0;
    for (offset, ch) in value.char_indices() {
        units += ch.len_utf16();
        if units > budget {
            return offset;
        }
    }
    value.len()
}

fn js_whitespace(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_a_valid_title_and_defers_the_placeholder() {
        assert_eq!(
            generated(
                json!({"title":" `Repair    Project Picker`\r\nignored", "needsRefinement": true})
            ),
            Ok(Some("Repair Project Picker".to_owned()))
        );
        assert_eq!(generated(json!({"title":"title"})), Err(()));
        assert_eq!(generated(json!({"title":"New thread"})), Ok(None));
        assert_eq!(
            generated(json!({"title":"{\"title\":\"Pick a Project\"}"})),
            Ok(Some("Pick a Project".to_owned()))
        );
        assert!(attachment_only(&format!("Ultrathink:\n{BOOTSTRAP_PROMPT}")));
    }

    #[test]
    fn truncates_the_prompt_and_title_without_splitting_surrogates() {
        let long = "🙂".repeat(4510);
        let curtailed = limit_message(&long);
        assert!(curtailed.contains(TRUNCATED));
        assert!(curtailed.encode_utf16().count() <= 8000);
        assert_eq!(
            generated(json!({"title":long})),
            Ok(Some(format!("{}...", "🙂".repeat(58))))
        );
    }
}
