import json
import os
import sys

TEXT = 'x' * int(os.environ.get('FLOOD_TEXT_BYTES', '4096'))
BURST = int(os.environ.get('FLOOD_BURST', '100000'))


def send(message):
    print(json.dumps(message), flush=True)


def update(session, kind, **fields):
    send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
        'sessionId': session, 'update': dict(sessionUpdate=kind, **fields)}})


def flood(session):
    for n in range(BURST):
        update(session, 'agent_message_chunk', content={'type': 'text', 'text': TEXT})
        update(session, 'tool_call', toolCallId='call-%d' % n, title='tool %d %s' % (n, 'y' * 200), status='pending')


for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    request_id = message.get('id')
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {}}
    elif method == 'session/new':
        if os.environ.get('FLOOD_ON_NEW'):
            flood('up')
        result = {'sessionId': 'up'}
    elif method == 'session/prompt':
        flood('up')
        result = {'stopReason': 'end_turn'}
    else:
        continue
    send({'jsonrpc': '2.0', 'id': request_id, 'result': result})
