WITH compaction_messages AS NOT MATERIALIZED (
    SELECT message_id
    FROM orchestration_v2_projection_messages
    WHERE role = 'user'
      AND lower(trim(json_extract(payload_json, '$.text'), char(
          9, 10, 11, 12, 13, 32, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197,
          8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288, 65279
      ))) = '/compact'
      AND coalesce(json_array_length(payload_json, '$.attachments'), 0) = 0
      AND (
          json_extract(payload_json, '$.idleCompaction') IS NOT NULL
          OR (
              json_extract(payload_json, '$.createdBy') = 'agent'
              AND json_extract(payload_json, '$.creationSource') = 'server'
          )
      )
), displayed_runs AS NOT MATERIALIZED (
    SELECT run.*
    FROM orchestration_v2_projection_runs AS run
    WHERE NOT (
        run.status <> 'failed'
        AND EXISTS (
            SELECT 1 FROM compaction_messages AS message
            WHERE message.message_id = json_extract(run.payload_json, '$.userMessageId')
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_turn_items AS compaction
            WHERE compaction.thread_id = run.thread_id AND compaction.run_id = run.run_id
              AND compaction.type = 'compaction' AND compaction.status = 'failed'
        )
    )
)
SELECT thread.thread_id, thread.project_id, thread.title, thread.archived_at,
       CASE
           WHEN presented.status IS NULL THEN 'idle'
           WHEN presented.status IN ('preparing', 'queued', 'starting', 'running', 'waiting',
                                     'completed', 'interrupted', 'failed', 'cancelled', 'rolled_back')
               THEN presented.status
           ELSE 'failed'
       END,
       json_extract(thread.payload_json, '$.worktreePath'), thread.updated_at,
       (
           SELECT message.updated_at FROM orchestration_v2_projection_messages AS message
           WHERE message.thread_id = thread.thread_id AND message.role = 'user'
             AND NOT EXISTS (
                 SELECT 1 FROM compaction_messages AS compaction
                 WHERE compaction.message_id = message.message_id
             )
           ORDER BY message.updated_at DESC, message.message_id DESC LIMIT 1
       ),
       presented.requested_at, json_extract(presented.payload_json, '$.startedAt'), presented.completed_at,
       json_extract(thread.payload_json, '$.providerInstanceId'),
       json_extract(thread.payload_json, '$.modelSelection'),
       json_extract(thread.payload_json, '$.lastVisitedAt')
FROM orchestration_v2_projection_threads AS thread
JOIN projection_projects AS project ON project.project_id = thread.project_id
LEFT JOIN orchestration_v2_projection_runs AS presented ON presented.run_id = (
    SELECT candidate.run_id
    FROM displayed_runs AS candidate
    WHERE candidate.thread_id = thread.thread_id
      AND NOT (candidate.status = 'queued' AND json_extract(candidate.payload_json, '$.queueHeld') IS 1)
      AND NOT (
          candidate.status = 'cancelled'
          AND json_extract(candidate.payload_json, '$.startedAt') IS NULL
          AND candidate.completed_at IS NOT NULL
          AND EXISTS (
              SELECT 1 FROM displayed_runs AS earlier
              WHERE earlier.thread_id = candidate.thread_id AND earlier.ordinal < candidate.ordinal
                AND earlier.status <> 'queued'
                AND (
                    earlier.completed_at IS NULL
                    OR julianday(earlier.completed_at) >= julianday(candidate.completed_at)
                )
          )
      )
    ORDER BY candidate.ordinal DESC, candidate.run_id DESC LIMIT 1
)
WHERE thread.deleted_at IS NULL AND project.deleted_at IS NULL
  AND json_extract(thread.payload_json, '$.lineage.relationshipToParent') IS NOT 'subagent'
  AND json_extract(thread.payload_json, '$.lineage.sideChat') IS NOT 1
