use std::path::Path;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::super::store::open_readonly;
use super::credentials::ProviderScope;
use super::history;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Read {
    pub(super) environment_id: Option<String>,
    pub(super) thread_id: String,
    pub(super) item_id: Option<String>,
    pub(super) text_offset: Option<usize>,
    #[serde(default)]
    pub(super) view: View,
    pub(super) after_position: Option<i64>,
    pub(super) before_position: Option<i64>,
    limit: Option<usize>,
    run_limit: Option<usize>,
    max_chars_per_item: Option<usize>,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(super) enum View {
    #[default]
    Messages,
    Activity,
}

impl Read {
    pub(super) fn valid(&self) -> bool {
        !self.thread_id.is_empty()
            && self.after_position.is_none_or(|value| value >= -1)
            && self.before_position.is_none_or(|value| value >= 0)
            && self.limit != Some(0)
            && self.run_limit != Some(0)
            && self.max_chars_per_item != Some(0)
    }

    pub(super) fn newest_first(&self) -> bool {
        self.before_position.is_some() || self.after_position.is_none()
    }

    pub(super) fn limit(&self) -> usize {
        self.limit.unwrap_or(10).min(100)
    }

    pub(super) fn max_chars(&self) -> usize {
        self.max_chars_per_item.unwrap_or(500).min(20_000)
    }

    fn run_limit(&self) -> i64 {
        self.run_limit.unwrap_or(10).min(50) as i64
    }
}

pub(super) enum Failure {
    MissingTarget(String),
    Unsupported,
    Database,
}

impl From<rusqlite::Error> for Failure {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

pub(super) fn parse_json(raw: String) -> Result<Value, Failure> {
    serde_json::from_str(&raw).map_err(|_| Failure::Database)
}

pub(super) fn read(
    path: &Path,
    environment: &str,
    scope: &ProviderScope,
    input: &Read,
) -> Result<Value, Failure> {
    let mut db = open_readonly(path)?;
    let tx = db.transaction()?;
    let caller = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM orchestration_v2_projection_threads
          WHERE thread_id=?1 AND deleted_at IS NULL AND provider_instance_id=?2)",
        [&scope.thread_id, &scope.provider_instance_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !caller {
        return Err(Failure::Database);
    }
    let thread=tx.query_row(
        "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id=?1 AND deleted_at IS NULL",
        [&input.thread_id],|row|row.get::<_,String>(0),
    ).optional()?.ok_or_else(||Failure::MissingTarget(input.thread_id.clone())).and_then(parse_json)?;
    if !thread["lineage"]["parentThreadId"].is_null()
        || !thread["forkedFrom"].is_null()
        || !thread["delegatedFrom"].is_null()
    {
        return Err(Failure::Unsupported);
    }
    let page = history::page(&tx, input)?;
    let (run_count, latest, active, runs) = run_page(&tx, input)?;
    let pending=tx.query_row(
        "SELECT count(*) FROM orchestration_v2_projection_runtime_requests WHERE thread_id=?1 AND status='pending'",
        [&input.thread_id],|row|row.get::<_,i64>(0),
    )?;
    let title = thread["title"].as_str().unwrap_or_default();
    let settled = thread["settledOverride"] == "settled";
    let snoozed = thread["snoozedUntil"]
        .as_str()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .is_some_and(|until| until > OffsetDateTime::now_utc());
    let status = active["status"]
        .as_str()
        .or_else(|| latest["status"].as_str())
        .unwrap_or("idle");
    let mut detail = json!({
        "threadId":input.thread_id,"projectId":thread["projectId"],"title":title,
        "link":link(environment,&input.thread_id,title),
        "createdBy":thread["createdBy"],"creationSource":thread["creationSource"],
        "status":status,"latestRunId":latest["id"],"activeRunId":active["id"],
        "providerInstanceId":thread["modelSelection"]["instanceId"],"model":thread["modelSelection"]["model"],
        "runtimeMode":thread["runtimeMode"],"interactionMode":thread["interactionMode"],
        "linkedPullRequest":thread["linkedPullRequest"],"titleRegeneration":thread["titleRegeneration"],
        "lastTitleRegeneration":thread["lastTitleRegeneration"],"branch":thread["branch"],"worktreePath":thread["worktreePath"],
        "parentThreadId":thread["lineage"]["parentThreadId"],"relationshipToParent":thread["lineage"]["relationshipToParent"],
        "runCount":run_count,"itemCount":page.total,"pendingRequestCount":pending,
        "archived":!thread["archivedAt"].is_null(),"settled":settled,
        "settledAt":if settled {thread["settledAt"].clone()}else{Value::Null},
        "snoozed":snoozed,"snoozedUntil":if snoozed {thread["snoozedUntil"].clone()}else{Value::Null},
        "pinned":!thread["pinnedAt"].is_null(),"blockedOnThreadWait":false,"pendingBackgroundTasks":[],
        "createdAt":thread["createdAt"],"updatedAt":thread["updatedAt"],
    });
    let first_page = input.after_position.is_none()
        && input.item_id.is_none()
        && input
            .before_position
            .is_none_or(|before| before >= page.total);
    pull_requests(&mut detail, &thread["pullRequests"], first_page);
    let mut output = json!({"thread":detail,"recentRuns":runs,"items":[],"nextPosition":null,"hasMore":false,"earlierPosition":null});
    history::fit(&mut output, page, input);
    tx.commit()?;
    Ok(output)
}

type RunPage = (i64, Value, Value, Vec<Value>);

fn run_page(tx: &Transaction<'_>, input: &Read) -> Result<RunPage, Failure> {
    let id = &input.thread_id;
    let count = tx.query_row(
        "SELECT count(*) FROM orchestration_v2_projection_runs WHERE thread_id=?1",
        [id],
        |row| row.get(0),
    )?;
    let active = tx
        .query_row(
            "SELECT payload_json FROM orchestration_v2_projection_runs
         WHERE thread_id=?1 AND status IN ('preparing','starting','running','waiting')
         ORDER BY ordinal DESC LIMIT 1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(parse_json)
        .transpose()?
        .unwrap_or(Value::Null);
    let mut statement=tx.prepare(
        "SELECT run.payload_json,(
            SELECT json_extract(turn.payload_json,'$.model') FROM orchestration_v2_projection_provider_turns AS turn
            WHERE turn.thread_id=run.thread_id
              AND json_extract(turn.payload_json,'$.runAttemptId')=json_extract(run.payload_json,'$.activeAttemptId')
            ORDER BY turn.ordinal DESC LIMIT 1)
         FROM orchestration_v2_projection_runs AS run WHERE run.thread_id=?1 ORDER BY run.ordinal DESC LIMIT ?2"
    )?;
    let mut rows = statement.query(params![id, input.run_limit()])?;
    let mut runs = Vec::new();
    let mut latest = Value::Null;
    while let Some(row) = rows.next()? {
        let run = parse_json(row.get(0)?)?;
        let model: Option<String> = row.get(1)?;
        runs.push(json!({"runId":run["id"],"ordinal":run["ordinal"],"status":run["status"],
            "providerInstanceId":run["modelSelection"]["instanceId"],"model":run["modelSelection"]["model"],
            "executedModel":model,"requestedAt":run["requestedAt"],"startedAt":run["startedAt"],"completedAt":run["completedAt"]}));
        if latest.is_null() {
            latest = run;
        }
    }
    Ok((count, latest, active, runs))
}

fn link(environment: &str, thread: &str, label: &str) -> String {
    let label = label.replace(['[', ']', '\\', '\r', '\n'], " ");
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let href = format!("t3-thread://v2/{environment}/{thread}");
    if label.is_empty() {
        return format!("<{href}>");
    }
    let destination = if href.contains(['(', ')']) {
        format!("<{href}>")
    } else {
        href
    };
    format!("[{label}]({destination})")
}

fn pull_requests(detail: &mut Value, links: &Value, include_links: bool) {
    let links = links.as_array().map_or(&[][..], |links| links.as_slice());
    let visible = links
        .iter()
        .filter(|link| link["removedAt"].is_null())
        .collect::<Vec<_>>();
    detail["pullRequestCount"] = json!(visible.len());
    if !include_links {
        return;
    }
    let mut open = visible
        .into_iter()
        .filter(|link| {
            !matches!(
                link["snapshot"]["state"].as_str(),
                Some("merged" | "closed")
            )
        })
        .collect::<Vec<_>>();
    detail["openPullRequestCount"] = json!(open.len());
    open.sort_by(|a, b| {
        a["watch"]
            .is_null()
            .cmp(&b["watch"].is_null())
            .then_with(|| {
                b["snapshot"]["updatedAt"]
                    .as_str()
                    .cmp(&a["snapshot"]["updatedAt"].as_str())
            })
    });
    detail["pullRequests"]=json!(open.into_iter().take(5).map(|link|json!({
        "host":link["host"],"repository":link["repository"],"number":link["number"],"url":link["url"],"source":link["source"],
        "state":link["snapshot"]["state"],"watching":!link["watch"].is_null(),"watchStartedAt":link["watch"]["startedAt"]
    })).collect::<Vec<_>>());
}
