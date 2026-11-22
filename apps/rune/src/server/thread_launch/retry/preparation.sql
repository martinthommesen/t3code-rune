SELECT project.workspace_root, thread.payload_json, run.payload_json,
       attempt.payload_json, node.payload_json, preparation.payload_json, failure.payload_json,
       message.payload_json
FROM orchestration_v2_projection_runs AS run
JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id=run.thread_id
JOIN projection_projects AS project ON project.project_id=thread.project_id
JOIN orchestration_v2_projection_run_attempts AS attempt
  ON attempt.run_id=run.run_id AND attempt.thread_id=run.thread_id
  AND attempt.attempt_id=json_extract(run.payload_json,'$.activeAttemptId')
JOIN orchestration_v2_projection_nodes AS node
  ON node.run_id=run.run_id AND node.thread_id=run.thread_id
  AND node.node_id=json_extract(run.payload_json,'$.rootNodeId')
  AND node.node_id=attempt.root_node_id
JOIN orchestration_v2_projection_provider_threads AS provider
  ON provider.provider_thread_id=run.provider_thread_id AND provider.thread_id=run.thread_id
JOIN orchestration_v2_projection_turn_items AS preparation
  ON preparation.run_id=run.run_id AND preparation.thread_id=run.thread_id
  AND preparation.type='command_execution'
  AND json_extract(preparation.payload_json,'$.input')='Preparing workspace'
JOIN orchestration_v2_projection_messages AS message
  ON message.run_id=run.run_id AND message.thread_id=run.thread_id
  AND message.message_id=json_extract(run.payload_json,'$.userMessageId')
LEFT JOIN orchestration_v2_projection_turn_items AS failure
  ON failure.run_id=run.run_id AND failure.thread_id=run.thread_id
  AND failure.type='error' AND failure.status='failed'
  AND json_extract(failure.payload_json,'$.failure.code')='workspace_preparation_failed'
WHERE run.thread_id=?1 AND run.run_id=?2 AND run.status=?3
  AND run.ordinal=1 AND run.provider_instance_id='codex'
  AND attempt.attempt_ordinal=1 AND node.kind='root_turn'
  AND provider.driver='codex' AND provider.status='not_loaded'
  AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
  AND project.deleted_at IS NULL
  AND json_extract(thread.payload_json,'$.worktreePath') IS NULL
  AND json_extract(run.payload_json,'$.workspacePreparation.type')='worktree'
  AND message.role='user'
  AND COALESCE(json_array_length(message.payload_json,'$.attachments'),0)=0
  AND json_extract(message.payload_json,'$.context') IS NULL
ORDER BY preparation.ordinal, failure.ordinal
LIMIT 1
