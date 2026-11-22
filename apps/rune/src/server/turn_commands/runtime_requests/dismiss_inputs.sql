WITH pending(id, node_id, payload_json) AS (
    SELECT runtime_request_id, node_id, payload_json
    FROM orchestration_v2_projection_runtime_requests
    WHERE thread_id = ?1 AND provider_turn_id = ?2 AND kind = 'user_input' AND status = 'pending'
      AND json_extract(payload_json, '$.responseCapability.type') IS NOT 'message'
)
SELECT 'runtime-request.updated', node_id, payload_json FROM pending
UNION ALL
SELECT 'node.updated', node.node_id, node.payload_json
FROM orchestration_v2_projection_nodes AS node
JOIN pending ON pending.node_id = node.node_id
WHERE node.thread_id = ?1
UNION ALL
SELECT 'turn-item.updated', item.node_id, item.payload_json
FROM orchestration_v2_projection_turn_items AS item
JOIN pending ON pending.id = json_extract(item.payload_json, '$.requestId')
WHERE item.thread_id = ?1 AND item.provider_turn_id = ?2 AND item.type = 'user_input_request'
