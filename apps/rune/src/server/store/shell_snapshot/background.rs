use std::collections::HashSet;

use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

use super::decode_json;

const PENDING_ITEMS: &str = "
    SELECT item.payload_json, provider.provider_instance_id
    FROM orchestration_v2_projection_turn_items AS item
        INDEXED BY orchestration_v2_projection_turn_items_shell_pending_idx
    LEFT JOIN orchestration_v2_projection_provider_threads AS provider
        ON provider.provider_thread_id = item.provider_thread_id
    LEFT JOIN orchestration_v2_projection_runs AS run ON run.run_id = item.run_id
    WHERE item.thread_id = ?1 AND item.type IN ('command_execution', 'dynamic_tool', 'subagent')
      AND item.status NOT IN ('completed', 'interrupted', 'failed', 'cancelled')
      AND (item.run_id IS NULL OR run.status <> 'rolled_back')
    UNION ALL
    SELECT item.payload_json, NULL
    FROM orchestration_v2_projection_turn_items AS item
    WHERE item.run_id = ?2 AND item.type IN ('file_change', 'file_search', 'web_search', 'compaction')
      AND item.status IN ('running', 'waiting')";

pub(super) fn read(
    database: &Connection,
    thread_id: &str,
    latest_status: &str,
    active_run: Option<&str>,
    active_status: Option<&str>,
    thread: &Value,
    providers: &[Value],
) -> rusqlite::Result<(Vec<Value>, bool)> {
    let running = active_run.filter(|_| active_status == Some("running"));
    if active_run.is_some() && running.is_none() {
        return Ok((Vec::new(), false));
    }
    if active_run.is_none() && latest_status == "idle" {
        return Ok((
            pull_request_watch_tasks(&thread["pullRequests"]).collect(),
            false,
        ));
    }
    if active_run.is_none()
        && !matches!(
            latest_status,
            "cancelled" | "completed" | "failed" | "interrupted" | "waiting"
        )
    {
        return Ok((Vec::new(), false));
    }
    let items = database
        .prepare(PENDING_ITEMS)?
        .query_map(params![thread_id, running], |row| {
            Ok((decode_json(row, 0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(run) = running {
        return Ok((Vec::new(), blocked_on_wait(run, &items)));
    }

    let mut tasks = Map::new();
    let active_provider = &thread["activeProviderThreadId"];
    for provider in providers
        .iter()
        .filter(|provider| active_provider.is_null() || provider["id"] == *active_provider)
    {
        for task in provider["pendingBackgroundTasks"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(id) = task["taskId"]
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            tasks
                .entry(id.to_owned())
                .or_insert_with(|| provider_task(task, id, provider));
        }
    }
    for (item, instance) in items {
        if !matches!(
            item["status"].as_str(),
            Some("pending" | "running" | "waiting")
        ) || (item["type"] == "dynamic_tool" && item["input"]["persistent"] == true)
        {
            continue;
        }
        let Some(id) = item["nativeItemRef"]["nativeId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .or_else(|| item["id"].as_str())
        else {
            continue;
        };
        tasks
            .entry(id.to_owned())
            .or_insert_with(|| item_task(&item, id, instance));
    }
    for watch in pull_request_watch_tasks(&thread["pullRequests"]) {
        if let Some(id) = watch["taskId"].as_str() {
            tasks.insert(id.to_owned(), watch);
        }
    }
    let mut children = HashSet::new();
    Ok((
        tasks
            .into_iter()
            .map(|(_, task)| task)
            .filter(|task| {
                task["kind"] != "subagent"
                    || task["childThreadId"]
                        .as_str()
                        .is_none_or(|child| children.insert(child.to_owned()))
            })
            .collect(),
        false,
    ))
}

fn blocked_on_wait(run: &str, items: &[(Value, Option<String>)]) -> bool {
    let mut waits = false;
    for (item, _) in items.iter().filter(|(item, _)| {
        item["runId"] == run
            && matches!(item["status"].as_str(), Some("running" | "waiting"))
            && !(item["type"] == "subagent" && item["origin"] == "app_owned")
    }) {
        if item["type"] != "dynamic_tool" {
            return false;
        }
        let tool = item["toolName"].as_str().and_then(wait_tool);
        if tool != Some("t3_thread_wait")
            && !(tool == Some("delegate_task") && item["input"]["mode"] == "wait")
        {
            return false;
        }
        waits = true;
    }
    waits
}

fn wait_tool(tool: &str) -> Option<&str> {
    let tool = tool.trim();
    let (name, suffix_allowed) = if let Some(rest) = tool.strip_prefix("mcp__") {
        let (server, name) = rest.split_once("__")?;
        match server.to_ascii_lowercase().as_str() {
            "t3" | "t3-code" | "t3_code" | "t3code" | "t3-code-direct" | "t3_code_direct" => {
                (name, false)
            }
            _ => return None,
        }
    } else {
        let lower = tool.to_ascii_lowercase();
        let prefix = [
            "t3-code-direct",
            "t3_code_direct",
            "t3-code",
            "t3_code",
            "t3code",
            "t3 code",
        ]
        .into_iter()
        .find(|prefix| lower.starts_with(prefix));
        if let Some(prefix) = prefix {
            let rest = &tool[prefix.len()..];
            (
                rest.trim_start_matches(['-', '_', '.', ':', '/', ' ', '·']),
                lower.starts_with("t3-code-"),
            )
        } else {
            (tool, false)
        }
    };
    ["t3_thread_wait", "delegate_task"]
        .into_iter()
        .find(|candidate| {
            name == *candidate
                || (suffix_allowed
                    && name
                        .strip_suffix(candidate)
                        .is_some_and(|prefix| prefix.ends_with('_')))
        })
}

fn provider_task(task: &Value, id: &str, provider: &Value) -> Value {
    let kind = task["kind"]
        .as_str()
        .filter(|kind| {
            matches!(
                *kind,
                "subagent" | "command" | "monitor" | "background_task"
            )
        })
        .unwrap_or("background_task");
    let mut result = Map::new();
    result.insert("taskId".into(), id.into());
    result.insert("kind".into(), kind.into());
    let fields: &[&str] = if task["kind"] == kind {
        &[
            "command",
            "itemId",
            "sourceThreadId",
            "parentTaskId",
            "nativeItemId",
            "runId",
            "awaited",
        ]
    } else {
        &["parentTaskId"]
    };
    for key in fields {
        if let Some(value) = task.get(*key) {
            result.insert((*key).into(), value.clone());
        }
    }
    if let Some(description) = trimmed(&task["description"]) {
        result.insert("description".into(), description.into());
    }
    if kind == "subagent" {
        for (key, value) in [
            ("providerInstanceId", &provider["providerInstanceId"]),
            ("childThreadId", &task["childThreadId"]),
        ] {
            if !value.is_null() {
                result.insert(key.into(), value.clone());
            }
        }
    }
    result.into()
}

fn item_task(item: &Value, id: &str, instance: Option<String>) -> Value {
    let (kind, fallback) = match item["type"].as_str() {
        Some("subagent") => ("subagent", &item["prompt"]),
        Some("command_execution") => (
            if item["monitor"] == true {
                "monitor"
            } else {
                "command"
            },
            &item["input"],
        ),
        _ => ("background_task", &item["toolName"]),
    };
    let mut result = Map::new();
    result.insert("taskId".into(), id.into());
    result.insert("kind".into(), kind.into());
    if let Some(description) = trimmed(&item["title"]).or_else(|| trimmed(fallback)) {
        result.insert("description".into(), description.into());
    }
    if kind == "subagent" {
        if let Some(instance) = instance {
            result.insert("providerInstanceId".into(), instance.into());
        }
        if let Some(child) = item["childThreadId"].as_str() {
            result.insert("childThreadId".into(), child.into());
        }
    }
    result.into()
}

fn trimmed(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn pull_request_watch_tasks(links: &Value) -> impl Iterator<Item = Value> + '_ {
    links.as_array().into_iter().flatten().filter_map(|link| {
        let watch = link.get("watch")?.as_object()?;
        if link["source"] == "stack-dismissed" {
            return None;
        }
        let (host, repository, number) = (
            link["host"].as_str()?.to_lowercase(),
            link["repository"].as_str()?.to_lowercase(),
            link["number"].as_u64()?,
        );
        let mut task = json!({
            "taskId": format!("pull-request-watch:{host}/{repository}#{number}"),
            "kind": "monitor",
            "description": format!("Watching pull request #{number}"),
        });
        if watch.get("checksPending") != Some(&Value::Bool(false))
            || watch.get("until").and_then(Value::as_str) == Some("merged")
            || link["snapshot"]["state"] == "merged"
        {
            task["awaited"] = Value::Bool(true);
        }
        Some(task)
    })
}
