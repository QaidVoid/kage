import json
import os
import sys

# Keeps its conversation and working directory in HISTORY_FILE, so a fresh
# process can resume it, and refuses a resume in another directory.
STORE = os.environ['HISTORY_FILE']


def send(message):
    print(json.dumps(message), flush=True)


def update(text):
    send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
        'sessionId': 'upstream', 'update': {'sessionUpdate': 'agent_message_chunk',
        'content': {'type': 'text', 'text': text}}}})


def load():
    with open(STORE) as f:
        return json.load(f)


def save(store):
    with open(STORE, 'w') as f:
        json.dump(store, f)


for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    params = message.get('params', {})
    request_id = message.get('id')
    if method == 'initialize':
        result = {'protocolVersion': 1,
                  'agentCapabilities': {'sessionCapabilities': {'resume': {}}}}
    elif method == 'session/new':
        if os.path.exists(STORE):
            sys.exit(3)
        save({'cwd': params['cwd'], 'turns': []})
        result = {'sessionId': 'upstream'}
    elif method == 'session/resume':
        if (params['sessionId'] != 'upstream' or not os.path.exists(STORE)
                or load()['cwd'] != params['cwd']):
            send({'jsonrpc': '2.0', 'id': request_id,
                  'error': {'code': -32602, 'message': 'unknown session'}})
            continue
        result = {}
    elif method == 'session/prompt':
        if params['prompt'][0]['text'] == 'exit':
            sys.exit(0)
        store = load()
        store['turns'].append(params['prompt'][0]['text'])
        save(store)
        update('|'.join(store['turns']))
        result = {'stopReason': 'end_turn'}
    else:
        continue
    send({'jsonrpc': '2.0', 'id': request_id, 'result': result})
