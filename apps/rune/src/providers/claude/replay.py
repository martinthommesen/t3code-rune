#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path


def send(frame):
    print(json.dumps(frame), flush=True)


def frames():
    for line in Path(os.environ['T3_CLAUDE_REPLAY_RECORDING']).read_text().splitlines():
        record = json.loads(line)
        if 'frame' in record:
            yield record


def replay():
    expected = []
    for record in frames():
        frame = record['frame']
        match record['type'], frame['type']:
            case 'expect_outbound', 'query.open':
                expected.append((frame['options'], []))
            case 'expect_outbound', 'prompt.offer':
                expected[-1][1].append((frame['message'], []))
            case 'emit_inbound', _:
                expected[-1][1][-1][1].append(frame)
    session_argument = next(
        arg for arg in sys.argv[1:] if arg.startswith(('--session-id=', '--resume='))
    )
    flag, session = session_argument.split('=', 1)
    key = 'sessionId' if flag == '--session-id' else 'resume'
    options, turns = next((options, turns) for options, turns in expected if options.get(key) == session)
    assert '--model=' + options['model'] in sys.argv
    approval = os.environ.get('T3_CLAUDE_REPLAY_APPROVAL')
    if approval:
        assert '--permission-mode=default' in sys.argv
        assert '--permission-prompt-tool=stdio' in sys.argv
    else:
        assert '--permission-mode=' + options['permissionMode'] in sys.argv
        assert '--allow-dangerously-skip-permissions' in sys.argv
    assert os.environ['CLAUDE_CODE_ENTRYPOINT'] == 't3-code'

    with Path('frames.ndjson').open('a') as log:
        log.write(json.dumps({'argv': sys.argv[1:]}) + '\n')
        log.flush()
        for line in sys.stdin:
            frame = json.loads(line)
            log.write(line)
            log.flush()
            if frame['type'] == 'control_request':
                assert frame['request']['subtype'] == 'initialize'
                send({'type': 'control_response', 'response': {
                    'subtype': 'success', 'request_id': frame['request_id'], 'response': {},
                }})
                continue
            expected_message, output = turns.pop(0)
            assert frame == expected_message, (frame, expected_message)
            if approval:
                send({'type': 'control_request', 'request_id': approval, 'request': {
                    'subtype': 'can_use_tool', 'tool_name': 'Read',
                    'input': {'file_path': '/fixture/no-data.txt'}, 'tool_use_id': 'toolu_fixture',
                }})
                reply = sys.stdin.readline()
                log.write(reply)
                log.flush()
                assert json.loads(reply)['response']['request_id'] == approval
                approval = None
            for incoming in output:
                send(incoming)
        assert not turns
        log.write(json.dumps({'eof': flag}) + '\n')


replay()
