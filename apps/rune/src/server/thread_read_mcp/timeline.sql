WITH visible AS MATERIALIZED (
    SELECT item.turn_item_id, item.type,
           ROW_NUMBER() OVER (ORDER BY item.ordinal, item.turn_item_id) - 1 AS position
    FROM orchestration_v2_projection_turn_items AS item
    LEFT JOIN orchestration_v2_projection_runs AS run
      ON run.thread_id=item.thread_id AND run.run_id=item.run_id
    WHERE item.thread_id=:thread
      AND run.status IS NOT 'rolled_back'
      AND NOT (COALESCE(run.status, '') IN ('queued','cancelled') AND item.type='user_message'
               AND json_extract(item.payload_json,'$.inputIntent') IS 'queued_turn')
      AND NOT (item.type='run_interrupt_result'
               AND EXISTS (
                   SELECT 1 FROM orchestration_v2_projection_run_attempts AS attempt
                   WHERE attempt.thread_id=item.thread_id AND attempt.run_id=item.run_id
                     AND attempt.root_node_id=item.node_id AND attempt.status='superseded'
               )
               AND NOT EXISTS (
                   SELECT 1 FROM orchestration_v2_projection_turn_items AS interrupt
                   WHERE interrupt.thread_id=item.thread_id AND interrupt.run_id=item.run_id
                     AND interrupt.type='run_interrupt_request'
               ))
), matching AS MATERIALIZED (
    SELECT turn_item_id, position FROM visible
    WHERE CASE WHEN :item_id IS NOT NULL THEN turn_item_id=:item_id
               ELSE :activity OR type IN ('user_message','assistant_message','proposed_plan') END
), page AS (
    SELECT turn_item_id, position FROM matching
    WHERE :item_id IS NOT NULL OR (position>:after AND position<:before)
    ORDER BY CASE WHEN :newest THEN -position ELSE position END
    LIMIT :lookahead
), bounds AS (
    SELECT (SELECT count(*) FROM visible) AS total, min(position) AS first, max(position) AS last
    FROM matching
)
SELECT bounds.total, bounds.first, bounds.last, page.position, item.payload_json,
       json_extract(message.payload_json, '$.createdBy'),
       json_extract(message.payload_json, '$.creationSource'),
       json_extract(message.payload_json, '$.senderThreadId')
FROM bounds
LEFT JOIN page ON 1=1
LEFT JOIN orchestration_v2_projection_turn_items AS item
  ON item.thread_id=:thread AND item.turn_item_id=page.turn_item_id
LEFT JOIN orchestration_v2_projection_messages AS message
  ON message.thread_id=:thread AND message.message_id=json_extract(item.payload_json,'$.messageId')
 AND item.type IN ('user_message','assistant_message')
ORDER BY page.position
