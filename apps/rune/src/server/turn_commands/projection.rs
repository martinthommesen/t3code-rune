use rusqlite::{Transaction, params};
use serde_json::Value;

pub(super) fn apply(
    transaction: &Transaction<'_>,
    thread_id: &str,
    event_type: &str,
    occurred_at: &str,
    payload: &Value,
) -> rusqlite::Result<()> {
    let encoded = payload.to_string();
    match event_type {
        "thread.metadata-updated" | "thread.model-selection-updated" => {
            transaction.execute(
                "UPDATE orchestration_v2_projection_threads
                     SET updated_at = json_extract(?1, '$.updatedAt'), payload_json = ?1
                     WHERE thread_id = ?2",
                params![encoded, thread_id],
            )?;
        }
        "thread.archived" | "thread.unarchived" => {
            transaction.execute(
                "UPDATE orchestration_v2_projection_threads
                 SET archived_at = json_extract(?1, '$.archivedAt'),
                     updated_at = json_extract(?1, '$.updatedAt'), payload_json = ?1
                 WHERE thread_id = ?2",
                params![encoded, thread_id],
            )?;
        }
        "thread.deleted" => {
            transaction.execute(
                "UPDATE orchestration_v2_projection_threads
                 SET deleted_at = json_extract(?1, '$.deletedAt'),
                     updated_at = json_extract(?1, '$.updatedAt'), payload_json = ?1
                 WHERE thread_id = ?2",
                params![encoded, thread_id],
            )?;
        }
        "provider-session.attached" | "provider-session.updated" => {
            transaction
                .prepare_cached(PROVIDER_SESSION)?
                .execute(params![encoded, thread_id])?;
            if event_type == "provider-session.attached" {
                transaction
                    .prepare_cached(
                        "INSERT OR IGNORE INTO orchestration_v2_projection_provider_session_bindings
                             (provider_session_id, thread_id)
                         VALUES (json_extract(?1, '$.id'), ?2)",
                    )?
                    .execute(params![encoded, thread_id])?;
            }
        }
        "provider-turn.updated" => {
            transaction
                .prepare_cached(PROVIDER_TURN)?
                .execute(params![encoded, thread_id])?;
        }
        "runtime-request.updated" => {
            transaction
                .prepare_cached(RUNTIME_REQUEST)?
                .execute(params![encoded, thread_id])?;
        }
        "checkpoint.captured" => {
            transaction
                .prepare_cached(CHECKPOINT)?
                .execute([&encoded])?;
        }
        "context-handoff.updated" => {
            transaction
                .prepare_cached(CONTEXT_HANDOFF)?
                .execute([&encoded])?;
        }
        "provider-thread.updated" => {
            transaction
                .prepare_cached(PROVIDER_THREAD)?
                .execute([&encoded])?;
            transaction
                .prepare_cached(ACTIVE_PROVIDER_THREAD)?
                .execute([&encoded])?;
        }
        "run.created" | "run.updated" => {
            transaction.prepare_cached(RUN)?.execute([&encoded])?;
        }
        "run-attempt.created" | "run-attempt.updated" => {
            transaction
                .prepare_cached(RUN_ATTEMPT)?
                .execute(params![encoded, thread_id])?;
        }
        "node.updated" => {
            transaction.prepare_cached(NODE)?.execute([&encoded])?;
        }
        "plan.updated" => {
            transaction.prepare_cached(PLAN)?.execute([&encoded])?;
        }
        "checkpoint-scope.created" => {
            transaction
                .prepare_cached(CHECKPOINT_SCOPE)?
                .execute([&encoded])?;
        }
        "message.updated" => {
            let message = payload["text"]
                .as_str()
                .ok_or(rusqlite::Error::InvalidQuery)?;
            transaction
                .prepare_cached(MESSAGE)?
                .execute(params![encoded, readable_search_text(message)])?;
        }
        "turn-item.updated" => {
            let inline_output = payload["type"] == "dynamic_tool" && is_inline_output(&encoded);
            transaction
                .prepare_cached(TURN_ITEM)?
                .execute(params![encoded, inline_output])?;
        }
        _ => return Err(rusqlite::Error::InvalidQuery),
    }
    transaction
        .prepare_cached(THREAD_TOUCH)?
        .execute(params![thread_id, occurred_at])?;
    Ok(())
}

const THREAD_TOUCH: &str = "
    UPDATE orchestration_v2_projection_threads
    SET updated_at = ?2, payload_json = json_set(payload_json, '$.updatedAt', ?2)
    WHERE thread_id = ?1
";

const PLAN: &str = "
    INSERT INTO orchestration_v2_projection_plans (
        plan_id, thread_id, run_id, node_id, kind, status, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'),
        json_extract(?1, '$.runId'), json_extract(?1, '$.nodeId'),
        json_extract(?1, '$.kind'), json_extract(?1, '$.status'), ?1
    ) ON CONFLICT(plan_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = excluded.run_id,
        node_id = excluded.node_id,
        kind = excluded.kind,
        status = excluded.status,
        payload_json = excluded.payload_json
";

const CONTEXT_HANDOFF: &str = "
    INSERT INTO orchestration_v2_projection_context_handoffs (
        context_handoff_id, thread_id, target_run_id, to_provider_thread_id,
        strategy, status, updated_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'),
        json_extract(?1, '$.targetRunId'), json_extract(?1, '$.toProviderThreadId'),
        json_extract(?1, '$.strategy'), json_extract(?1, '$.status'),
        json_extract(?1, '$.updatedAt'), ?1
    ) ON CONFLICT(context_handoff_id) DO UPDATE SET
        thread_id = excluded.thread_id, target_run_id = excluded.target_run_id,
        to_provider_thread_id = excluded.to_provider_thread_id, strategy = excluded.strategy,
        status = excluded.status, updated_at = excluded.updated_at, payload_json = excluded.payload_json
";

const PROVIDER_SESSION: &str = "
    INSERT INTO orchestration_v2_projection_provider_sessions (
        provider_session_id, thread_id, provider, driver, provider_instance_id, status, updated_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), ?2, json_extract(?1, '$.providerInstanceId'),
        json_extract(?1, '$.driver'), json_extract(?1, '$.providerInstanceId'),
        json_extract(?1, '$.status'), json_extract(?1, '$.updatedAt'), ?1
    )
    ON CONFLICT(provider_session_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        provider = excluded.provider,
        driver = excluded.driver,
        provider_instance_id = excluded.provider_instance_id,
        status = excluded.status,
        updated_at = excluded.updated_at,
        payload_json = excluded.payload_json
";

const PROVIDER_TURN: &str = "
    INSERT INTO orchestration_v2_projection_provider_turns (
        provider_turn_id, thread_id, provider_thread_id, node_id, run_attempt_id, ordinal,
        status, started_at, completed_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), ?2, json_extract(?1, '$.providerThreadId'),
        json_extract(?1, '$.nodeId'), json_extract(?1, '$.runAttemptId'),
        json_extract(?1, '$.ordinal'), json_extract(?1, '$.status'),
        json_extract(?1, '$.startedAt'), json_extract(?1, '$.completedAt'), ?1
    )
    ON CONFLICT(provider_turn_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        provider_thread_id = excluded.provider_thread_id,
        node_id = excluded.node_id,
        run_attempt_id = excluded.run_attempt_id,
        ordinal = excluded.ordinal,
        status = excluded.status,
        started_at = excluded.started_at,
        completed_at = excluded.completed_at,
        payload_json = CASE
            WHEN json_type(excluded.payload_json, '$.tokenUsage') IS NULL
                AND json_type(orchestration_v2_projection_provider_turns.payload_json, '$.tokenUsage') IS NOT NULL
            THEN json_set(excluded.payload_json, '$.tokenUsage',
                json_extract(orchestration_v2_projection_provider_turns.payload_json, '$.tokenUsage'))
            ELSE excluded.payload_json
        END
";

const RUNTIME_REQUEST: &str = "
    INSERT INTO orchestration_v2_projection_runtime_requests (
        runtime_request_id, thread_id, node_id, provider_turn_id, kind, status,
        created_at, resolved_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), ?2, json_extract(?1, '$.nodeId'),
        json_extract(?1, '$.providerTurnId'), json_extract(?1, '$.kind'),
        json_extract(?1, '$.status'), json_extract(?1, '$.createdAt'),
        json_extract(?1, '$.resolvedAt'), ?1
    )
    ON CONFLICT(runtime_request_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        node_id = excluded.node_id,
        provider_turn_id = excluded.provider_turn_id,
        kind = excluded.kind,
        status = excluded.status,
        created_at = excluded.created_at,
        resolved_at = excluded.resolved_at,
        payload_json = excluded.payload_json
";

const CHECKPOINT: &str = "
    INSERT INTO orchestration_v2_projection_checkpoints (
        checkpoint_id, thread_id, scope_id, run_id, node_id, ordinal_within_scope,
        app_run_ordinal, status, captured_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'),
        json_extract(?1, '$.scopeId'), json_extract(?1, '$.runId'),
        json_extract(?1, '$.nodeId'), json_extract(?1, '$.ordinalWithinScope'),
        json_extract(?1, '$.appRunOrdinal'), json_extract(?1, '$.status'),
        json_extract(?1, '$.capturedAt'), ?1
    )
    ON CONFLICT(checkpoint_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        scope_id = excluded.scope_id,
        run_id = excluded.run_id,
        node_id = excluded.node_id,
        ordinal_within_scope = excluded.ordinal_within_scope,
        app_run_ordinal = excluded.app_run_ordinal,
        status = excluded.status,
        captured_at = excluded.captured_at,
        payload_json = excluded.payload_json
";

const PROVIDER_THREAD: &str = "
    INSERT INTO orchestration_v2_projection_provider_threads (
        provider_thread_id, thread_id, owner_node_id, provider, driver, provider_instance_id,
        provider_session_id, status, first_run_ordinal, last_run_ordinal, updated_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.appThreadId'), json_extract(?1, '$.ownerNodeId'),
        json_extract(?1, '$.providerInstanceId'), json_extract(?1, '$.driver'),
        json_extract(?1, '$.providerInstanceId'), json_extract(?1, '$.providerSessionId'),
        json_extract(?1, '$.status'), json_extract(?1, '$.firstRunOrdinal'),
        json_extract(?1, '$.lastRunOrdinal'), json_extract(?1, '$.updatedAt'),
        json_insert(
            ?1,
            '$.pendingBackgroundTasks', json('[]'),
            '$.contextUsage', NULL,
            '$.nativeMetadata', NULL,
            '$.goal', NULL
        )
    )
    ON CONFLICT(provider_thread_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        owner_node_id = excluded.owner_node_id,
        provider = excluded.provider,
        driver = excluded.driver,
        provider_instance_id = excluded.provider_instance_id,
        provider_session_id = excluded.provider_session_id,
        status = excluded.status,
        first_run_ordinal = excluded.first_run_ordinal,
        last_run_ordinal = excluded.last_run_ordinal,
        updated_at = excluded.updated_at,
        payload_json = excluded.payload_json
";

const ACTIVE_PROVIDER_THREAD: &str = "
    UPDATE orchestration_v2_projection_threads
    SET active_provider_thread_id = json_extract(?1, '$.id'),
        updated_at = json_extract(?1, '$.updatedAt'),
        payload_json = json_set(
            payload_json,
            '$.activeProviderThreadId', json_extract(?1, '$.id'),
            '$.updatedAt', json_extract(?1, '$.updatedAt')
        )
    WHERE thread_id = json_extract(?1, '$.appThreadId')
        AND NOT (
            json_extract(?1, '$.status') = 'not_loaded'
            AND json_extract(?1, '$.firstRunOrdinal') IS NULL
            AND json_extract(?1, '$.nativeThreadRef') IS NULL
            AND json_extract(?1, '$.providerSessionId') IS NULL
        )
";

const RUN: &str = "
    INSERT INTO orchestration_v2_projection_runs (
        run_id, thread_id, ordinal, provider, provider_instance_id, provider_thread_id,
        status, requested_at, completed_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'), json_extract(?1, '$.ordinal'),
        json_extract(?1, '$.providerInstanceId'), json_extract(?1, '$.providerInstanceId'),
        json_extract(?1, '$.providerThreadId'), json_extract(?1, '$.status'),
        json_extract(?1, '$.requestedAt'), json_extract(?1, '$.completedAt'), ?1
    )
    ON CONFLICT(run_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        ordinal = excluded.ordinal,
        provider = excluded.provider,
        provider_instance_id = excluded.provider_instance_id,
        provider_thread_id = excluded.provider_thread_id,
        status = excluded.status,
        requested_at = excluded.requested_at,
        completed_at = excluded.completed_at,
        payload_json = (
            WITH delegation(payload) AS (SELECT CASE
                WHEN json_type(excluded.payload_json, '$.delegatedCompletion') IS NULL
                    AND json_type(orchestration_v2_projection_runs.payload_json, '$.delegatedCompletion') IS NOT NULL
                THEN json_set(excluded.payload_json, '$.delegatedCompletion',
                    json_extract(orchestration_v2_projection_runs.payload_json, '$.delegatedCompletion'))
                ELSE excluded.payload_json END),
            background(payload) AS (SELECT CASE
                WHEN json_type(excluded.payload_json, '$.restartCancelledBackgroundWork') IS NULL
                    AND json_type(orchestration_v2_projection_runs.payload_json, '$.restartCancelledBackgroundWork') IS NOT NULL
                THEN json_set(payload, '$.restartCancelledBackgroundWork',
                    json_extract(orchestration_v2_projection_runs.payload_json, '$.restartCancelledBackgroundWork'))
                ELSE payload END FROM delegation)
            SELECT CASE
                WHEN json_type(orchestration_v2_projection_runs.payload_json, '$.output') IS NOT NULL
                THEN json_set(payload, '$.output',
                    json_extract(orchestration_v2_projection_runs.payload_json, '$.output'))
                ELSE payload END
            FROM background
        )
";

const RUN_ATTEMPT: &str = "
    INSERT INTO orchestration_v2_projection_run_attempts (
        attempt_id, thread_id, run_id, attempt_ordinal, root_node_id, provider,
        provider_instance_id, provider_thread_id, provider_turn_id, status, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), ?2, json_extract(?1, '$.runId'),
        json_extract(?1, '$.attemptOrdinal'), json_extract(?1, '$.rootNodeId'),
        json_extract(?1, '$.providerInstanceId'), json_extract(?1, '$.providerInstanceId'),
        json_extract(?1, '$.providerThreadId'), json_extract(?1, '$.providerTurnId'),
        json_extract(?1, '$.status'), ?1
    )
    ON CONFLICT(attempt_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = excluded.run_id,
        attempt_ordinal = excluded.attempt_ordinal,
        root_node_id = excluded.root_node_id,
        provider = excluded.provider,
        provider_instance_id = excluded.provider_instance_id,
        provider_thread_id = excluded.provider_thread_id,
        provider_turn_id = excluded.provider_turn_id,
        status = excluded.status,
        payload_json = excluded.payload_json
";

const NODE: &str = "
    INSERT INTO orchestration_v2_projection_nodes (
        node_id, thread_id, run_id, parent_node_id, root_node_id, kind, status,
        provider_thread_id, provider_turn_id, started_at, completed_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'), json_extract(?1, '$.runId'),
        json_extract(?1, '$.parentNodeId'), json_extract(?1, '$.rootNodeId'),
        json_extract(?1, '$.kind'), json_extract(?1, '$.status'),
        json_extract(?1, '$.providerThreadId'), json_extract(?1, '$.providerTurnId'),
        json_extract(?1, '$.startedAt'), json_extract(?1, '$.completedAt'), ?1
    )
    ON CONFLICT(node_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = excluded.run_id,
        parent_node_id = excluded.parent_node_id,
        root_node_id = excluded.root_node_id,
        kind = excluded.kind,
        status = excluded.status,
        provider_thread_id = excluded.provider_thread_id,
        provider_turn_id = excluded.provider_turn_id,
        started_at = excluded.started_at,
        completed_at = excluded.completed_at,
        payload_json = excluded.payload_json
";

const CHECKPOINT_SCOPE: &str = "
    INSERT INTO orchestration_v2_projection_checkpoint_scopes (
        scope_id, thread_id, run_id, node_id, provider_thread_id, kind, ordinal_within_parent,
        advances_app_run_count, created_at, payload_json
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'), json_extract(?1, '$.runId'),
        json_extract(?1, '$.nodeId'), json_extract(?1, '$.providerThreadId'),
        json_extract(?1, '$.kind'), json_extract(?1, '$.ordinalWithinParent'),
        json_extract(?1, '$.advancesAppRunCount'), json_extract(?1, '$.createdAt'), ?1
    )
    ON CONFLICT(scope_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = excluded.run_id,
        node_id = excluded.node_id,
        provider_thread_id = excluded.provider_thread_id,
        kind = excluded.kind,
        ordinal_within_parent = excluded.ordinal_within_parent,
        advances_app_run_count = excluded.advances_app_run_count,
        created_at = excluded.created_at,
        payload_json = excluded.payload_json
";

const MESSAGE: &str = "
    INSERT INTO orchestration_v2_projection_messages (
        message_id, thread_id, run_id, node_id, role, streaming,
        created_at, updated_at, payload_json, search_text
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'), json_extract(?1, '$.runId'),
        json_extract(?1, '$.nodeId'), json_extract(?1, '$.role'), json_extract(?1, '$.streaming'),
        json_extract(?1, '$.createdAt'), json_extract(?1, '$.updatedAt'), ?1, ?2
    )
    ON CONFLICT(message_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = excluded.run_id,
        node_id = excluded.node_id,
        role = excluded.role,
        streaming = excluded.streaming,
        created_at = excluded.created_at,
        updated_at = excluded.updated_at,
        payload_json = excluded.payload_json,
        search_text = excluded.search_text
";

const TURN_ITEM: &str = "
    INSERT INTO orchestration_v2_projection_turn_items (
        turn_item_id, thread_id, run_id, node_id, provider_thread_id, provider_turn_id,
        parent_item_id, ordinal, type, status, updated_at, payload_json, inline_work_output
    ) VALUES (
        json_extract(?1, '$.id'), json_extract(?1, '$.threadId'), json_extract(?1, '$.runId'),
        json_extract(?1, '$.nodeId'), json_extract(?1, '$.providerThreadId'),
        json_extract(?1, '$.providerTurnId'), json_extract(?1, '$.parentItemId'),
        json_extract(?1, '$.ordinal'), json_extract(?1, '$.type'), json_extract(?1, '$.status'),
        json_extract(?1, '$.updatedAt'), ?1, ?2
    )
    ON CONFLICT(turn_item_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = excluded.run_id,
        node_id = excluded.node_id,
        provider_thread_id = excluded.provider_thread_id,
        provider_turn_id = excluded.provider_turn_id,
        parent_item_id = excluded.parent_item_id,
        ordinal = excluded.ordinal,
        type = excluded.type,
        status = excluded.status,
        updated_at = excluded.updated_at,
        payload_json = excluded.payload_json,
        inline_work_output = excluded.inline_work_output
";

fn is_inline_output(encoded: &str) -> bool {
    let lowercase = encoded.to_ascii_lowercase();
    [
        "htmlrender",
        "t3mcpapp",
        "\"viewedimagepath\"",
        "sentmessage",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
        || lowercase
            .split_once("\"toolname\":\"")
            .is_some_and(|(_, value)| {
                [
                    ("delegate", &["task\""][..]),
                    ("send", &["to", "user\""][..]),
                ]
                .iter()
                .any(|(prefix, suffixes)| {
                    value.match_indices(prefix).any(|(index, prefix)| {
                        suffixes
                            .iter()
                            .try_fold(&value[index + prefix.len()..], |rest, suffix| {
                                let separator = rest.chars().next()?;
                                if separator.len_utf16() != 1 {
                                    return None;
                                }
                                rest[separator.len_utf8()..].strip_prefix(suffix)
                            })
                            .is_some()
                    })
                })
            })
}

fn readable_search_text(text: &str) -> Option<String> {
    if !text.contains(r"\_") && !text.contains(r"\*") {
        return None;
    }
    let mut rewritten = String::new();
    let mut position = 0;
    let mut changed = false;
    while let Some(open) = text[position..].find('[') {
        let start = position + open + 1;
        rewritten.push_str(&text[position..start]);
        position = start;
        let Some(close) = text[start..].find([']', '\n']) else {
            break;
        };
        let label = &text[start..start + close];
        let Some(destination) = text[start + close..].strip_prefix("](") else {
            continue;
        };
        let Some(end) = destination
            .find(|c: char| c == ')' || c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
        else {
            continue;
        };
        if destination.as_bytes()[end] != b')' {
            continue;
        }
        let mut characters = label.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\\' && matches!(characters.peek(), Some('*' | '_')) {
                changed = true;
            } else {
                rewritten.push(character);
            }
        }
        position = start + close + "](".len() + end + ")".len();
        rewritten.push_str(&text[start + close..position]);
    }
    changed.then(|| {
        rewritten.push_str(&text[position..]);
        rewritten
    })
}
