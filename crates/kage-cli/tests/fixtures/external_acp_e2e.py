import json
import os
import sys

# A resumable agent for the end to end test. Prompt "wait" blocks until the
# turn is cancelled and prompt "ask" requests a permission, answering with the
# option it was given. Any other prompt joins every earlier one, so a process
# started after a restart shows whether the conversation was restored.
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


def next_message():
    return json.loads(sys.stdin.readline())


for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    params = message.get('params', {})
    request_id = message.get('id')
    if method == 'initialize':
        result = {'protocolVersion': 1,
                  'agentCapabilities': {'sessionCapabilities': {'resume': {}}}}
    elif method == 'session/new':
        save({'cwd': params['cwd'], 'turns': []})
        result = {'sessionId': 'upstream'}
    elif method == 'session/resume':
        if params['sessionId'] != 'upstream' or load()['cwd'] != params['cwd']:
            send({'jsonrpc': '2.0', 'id': request_id,
                  'error': {'code': -32602, 'message': 'unknown session'}})
            continue
        result = {}
    elif method == 'session/prompt':
        text = params['prompt'][0]['text']
        if text == 'wait':
            update('waiting')
            while next_message().get('method') != 'session/cancel':
                pass
            result = {'stopReason': 'cancelled'}
        elif text == 'ask':
            send({'jsonrpc': '2.0', 'id': 'ask-1', 'method': 'session/request_permission',
                  'params': {'sessionId': 'upstream',
                             'toolCall': {'toolCallId': 'tool-1', 'title': 'run it'},
                             'options': [
                                 {'optionId': 'allow', 'name': 'Allow', 'kind': 'allow_once'},
                                 {'optionId': 'deny', 'name': 'Deny', 'kind': 'reject_once'}]}})
            answer = next_message()
            assert answer['id'] == 'ask-1', answer
            update('asked:' + str(answer['result']['outcome'].get('optionId')))
            result = {'stopReason': 'end_turn'}
        else:
            store = load()
            store['turns'].append(text)
            save(store)
            update('|'.join(store['turns']))
            result = {'stopReason': 'end_turn'}
    else:
        continue
    send({'jsonrpc': '2.0', 'id': request_id, 'result': result})
