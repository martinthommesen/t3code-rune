use rusqlite::{Transaction, named_params};
use serde_json::{Value, json};

use super::projection::{Failure, Read, View, parse_json};

const READ_RESULT_BUDGET: usize = 30_000;

pub(super) struct Page {
    pub(super) total: i64,
    pub(super) items: Vec<Item>,
    pub(super) has_more: bool,
    pub(super) has_earlier: bool,
}

pub(super) struct Item {
    position: i64,
    payload: Value,
    created_by: Option<String>,
    creation_source: Option<String>,
    sender_thread_id: Option<String>,
}

pub(super) fn page(tx: &Transaction<'_>, input: &Read) -> Result<Page, Failure> {
    let limit = input.limit();
    let newest = input.newest_first();
    let after = input.after_position.unwrap_or(-1);
    let before = input.before_position.unwrap_or(i64::MAX);
    let mut stmt = tx.prepare(include_str!("timeline.sql"))?;
    let mut rows = stmt.query(named_params! {
        ":thread":input.thread_id, ":item_id":input.item_id, ":activity":input.view==View::Activity,
        ":after":after, ":before":before, ":newest":newest, ":lookahead":limit as i64 + 1,
    })?;
    let Some(mut row) = rows.next()? else {
        unreachable!("timeline always returns its bounds")
    };
    let total = row.get(0)?;
    let first: Option<i64> = row.get(1)?;
    let last: Option<i64> = row.get(2)?;
    let mut items = Vec::new();
    loop {
        if let Some(position) = row.get::<_, Option<i64>>(3)? {
            items.push(Item {
                position,
                payload: parse_json(row.get(4)?)?,
                created_by: row.get(5)?,
                creation_source: row.get(6)?,
                sender_thread_id: row.get(7)?,
            });
        }
        let Some(next) = rows.next()? else {
            break;
        };
        row = next;
    }
    let extra = items.len() > limit;
    if extra {
        if newest {
            items.remove(0);
        } else {
            items.pop();
        }
    }
    let start = items
        .first()
        .map_or(after.saturating_add(1), |row| row.position);
    let has_more = if newest {
        last.is_some_and(|position| position >= before)
    } else {
        extra
    };
    Ok(Page {
        total,
        items,
        has_more,
        has_earlier: first.is_some_and(|position| position < start),
    })
}

pub(super) fn fit(envelope: &mut Value, page: Page, input: &Read) {
    let budget = READ_RESULT_BUDGET
        .saturating_sub(envelope.to_string().len())
        .max(READ_RESULT_BUDGET / 2);
    let newest = input.newest_first();
    let mut items = Vec::new();
    let mut bytes = 0;
    let thread = &input.thread_id;
    let mut rows = page.items;
    if newest {
        rows.reverse();
    }
    for row in &rows {
        let text = render(&row.payload);
        let offset = input.item_id.as_ref().and(input.text_offset).unwrap_or(0);
        let max_chars = input.max_chars();
        let mut item = row.json(thread, text.as_deref(), offset, max_chars);
        bytes += item.to_string().len() + usize::from(!items.is_empty());
        if bytes > budget {
            if items.is_empty() {
                if item["title"]
                    .as_str()
                    .is_some_and(|title| title.len() > budget / 4)
                {
                    let title = item["title"].as_str().unwrap();
                    let (title, _) = text_page(title, 0, budget / 6);
                    item["title"] = json!(title);
                }
                let mut low = 1;
                let mut high = max_chars;
                while low < high {
                    let middle = low + (high - low).div_ceil(2);
                    set_item_text(&mut item, text.as_deref(), offset, middle);
                    if item.to_string().len() <= budget {
                        low = middle;
                    } else {
                        high = middle - 1;
                    }
                }
                set_item_text(&mut item, text.as_deref(), offset, low);
                items.push(item);
            }
            break;
        }
        items.push(item);
    }
    if newest {
        items.reverse();
    }
    let kept_fewer = items.len() < rows.len();
    envelope["nextPosition"] = items
        .last()
        .map_or(Value::Null, |item| item["position"].clone());
    envelope["earlierPosition"] = if page.has_earlier || newest && kept_fewer {
        items
            .first()
            .map_or(Value::Null, |item| item["position"].clone())
    } else {
        Value::Null
    };
    envelope["hasMore"] = json!(page.has_more || !newest && kept_fewer);
    envelope["items"] = json!(items);
}

impl Item {
    fn json(&self, thread: &str, text: Option<&str>, offset: usize, max: usize) -> Value {
        let payload = &self.payload;
        let kind = payload["type"].as_str().unwrap_or_default();
        let mut output = json!({
            "position":self.position,"visibility":"local","sourceThreadId":thread,
            "itemId":payload["id"],"runId":payload["runId"],
            "messageId":if matches!(kind,"user_message"|"assistant_message"){payload["messageId"].clone()}else{Value::Null},
            "createdBy":self.created_by,"creationSource":self.creation_source,"type":kind,
            "status":payload["status"],"title":payload["title"],"updatedAt":payload["updatedAt"],
        });
        if kind == "subagent" {
            output["subagentId"] = payload["subagentId"].clone();
        }
        if let Some(sender) = &self.sender_thread_id {
            output["senderThreadId"] = json!(sender);
        }
        set_item_text(&mut output, text, offset, max);
        output
    }
}

fn set_item_text(output: &mut Value, text: Option<&str>, offset: usize, max_chars: usize) {
    let (text, next) = if let Some(text) = text {
        let (text, next) = text_page(text, offset, max_chars);
        (Some(text), next)
    } else {
        (None, None)
    };
    output["text"] = json!(text);
    output["textTruncated"] = json!(next.is_some());
    output["nextTextOffset"] = json!(next);
}

fn text_page(text: &str, offset: usize, max: usize) -> (&str, Option<usize>) {
    let mut units = 0_usize;
    let mut start = None;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            let start = *start.get_or_insert(byte);
            if units.saturating_add(ch.len_utf16()) > offset.saturating_add(max) && byte > start {
                return (&text[start..byte], Some(units));
            }
        }
        units += ch.len_utf16();
    }
    (&text[start.unwrap_or(text.len())..], None)
}

fn render(item: &Value) -> Option<String> {
    let field = |key: &str| item[key].as_str();
    let lines = |parts: &[Option<&str>]| {
        parts
            .iter()
            .filter_map(|part| *part)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    let kind = field("type")?;
    let text = match kind {
        "user_message" | "assistant_message" | "reasoning" => field("text")?.to_owned(),
        "proposed_plan" => field("markdown")?.to_owned(),
        "notification" => lines(&[field("summary"), field("detail")]),
        "todo_list" => {
            let mut rows = field("explanation")
                .map(ToOwned::to_owned)
                .into_iter()
                .collect::<Vec<_>>();
            if let Some(steps) = item["steps"].as_array() {
                rows.extend(steps.iter().filter_map(|v| {
                    Some(format!(
                        "[{}] {}",
                        v["status"].as_str()?,
                        v["text"].as_str()?
                    ))
                }));
            }
            rows.join("\n")
        }
        "user_input_request" => pretty(&item["questions"]),
        "file_change" => {
            let delta = item
                .get("additions")
                .or_else(|| item.get("deletions"))
                .map(|_| {
                    format!(
                        "+{} -{}",
                        item["additions"].as_i64().unwrap_or(0),
                        item["deletions"].as_i64().unwrap_or(0)
                    )
                });
            lines(&[
                field("fileName"),
                delta.as_deref(),
                field("diffStr").or_else(|| field("newStr")),
            ])
        }
        "command_execution" => lines(&[Some(&format!("$ {}", field("input")?)), field("output")]),
        "file_search" => pretty(&json!({"pattern":item["pattern"],"results":item["results"]})),
        "web_search" => pretty(&json!({"patterns":item["patterns"],"results":item["results"]})),
        "approval_request" => lines(&[
            field("prompt").or_else(|| field("requestKind")),
            field("reason"),
        ]),
        "checkpoint" => pretty(&item["files"]),
        "run_interrupt_request" | "run_interrupt_result" | "system_notice" => {
            field("message")?.to_owned()
        }
        "error" => item["failure"]["message"].as_str()?.to_owned(),
        "compaction" => field("summary")?.to_owned(),
        "handoff" => field("summary").map(ToOwned::to_owned).unwrap_or_else(|| {
            format!(
                "{} handoff to {}",
                field("strategy").unwrap_or_default(),
                field("toProviderInstanceId").unwrap_or_default()
            )
        }),
        "secret_request" => format!(
            "Asked the user for {}: {}.",
            field("label")?,
            field("secretStatus")?
        ),
        "subagent" => field("result")
            .or_else(|| field("progress"))
            .or_else(|| field("prompt"))?
            .to_owned(),
        "dynamic_tool" => pretty(
            &json!({"toolName":item["toolName"],"input":item["input"],"output":item["output"]}),
        ),
        _ => return None,
    };
    Some(text)
}
