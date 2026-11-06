import json
import sys


def send(message):
    print(json.dumps(message), flush=True)


def update(text):
    send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
        'sessionId': 'upstream', 'update': {'sessionUpdate': 'agent_message_chunk',
        'content': {'type': 'text', 'text': text}}}})


history = []
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    params = message.get('params', {})
    request_id = message.get('id')
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {}}
    elif method == 'session/new':
        update('created')
        result = {'sessionId': 'upstream'}
    elif method == 'session/prompt':
        assert params['sessionId'] == 'upstream'
        text = params['prompt'][0]['text']
        if text == 'wait':
            update('waiting')
            cancel = json.loads(sys.stdin.readline())
            assert cancel['method'] == 'session/cancel'
            assert cancel['params']['sessionId'] == 'upstream'
            result = {'stopReason': 'cancelled'}
        else:
            history.append(text)
            update('|'.join(history))
            result = {'stopReason': 'end_turn'}
    else:
        continue
    send({'jsonrpc': '2.0', 'id': request_id, 'result': result})
