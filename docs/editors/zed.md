# zed

`kage rpc` is a spec-conformant **Agent Client Protocol** agent:
JSON-RPC 2.0 over stdio, one message per line (newline-delimited),
protocol version 1. Zed speaks ACP natively, so it drives kage the
same way it drives its other external agents - no shim, no wrapper.

## start the agent

```sh
kage rpc
```

Optional flags:

- `-m, --model <provider:model>` pins the model for the connection.
- `--system <text>` overrides the system-prompt role.

Credentials resolve as for the TUI and print mode (OS keyring,
`kage auth login`, or an API-key env var). With no provider
configured `kage rpc` prints a message and exits non-zero.

## configure zed

Add kage as an agent server in Zed's `settings.json`:

```json
{
  "agent_servers": {
    "kage": {
      "command": "kage",
      "args": ["rpc"]
    }
  }
}
```

Use an absolute `command` if `kage` is not on Zed's `PATH`; pass a
model with `"args": ["rpc", "-m", "anthropic:claude-sonnet-4-6"]`.
Pick kage from Zed's agent panel and prompt as usual. Tool calls
surface as Zed permission prompts (kage never auto-approves); the
agent's text and reasoning stream in as it works.

## what kage implements

Client to agent:

| method            | params -> result                                    |
| ----------------- | --------------------------------------------------- |
| `initialize`      | `{protocolVersion, clientCapabilities}` -> `{protocolVersion, agentCapabilities, agentInfo, authMethods}` |
| `session/new`     | `{cwd, mcpServers}` -> `{sessionId}`                 |
| `session/load`    | `{sessionId, cwd, mcpServers}` -> replays history as `session/update`, then `null` |
| `session/prompt`  | `{sessionId, prompt: ContentBlock[]}` -> `{stopReason}` |
| `session/cancel`  | notification `{sessionId}`                           |

Agent to client:

- `session/update` notification `{sessionId, update}` where `update`
  is tagged by `sessionUpdate`: `agent_message_chunk`,
  `agent_thought_chunk`, `tool_call`, `tool_call_update`.
- `session/request_permission` request `{sessionId, toolCall,
  options}` -> `{outcome}`; the client returns
  `{outcome: {outcome: "selected", optionId}}` or
  `{outcome: {outcome: "cancelled"}}`. kage blocks the tool until the
  client answers and never auto-approves.

kage advertises `agentCapabilities.loadSession: true`, empty
`authMethods`, and `agentInfo {name: "kage", version}`. It does not
request `fs`/`terminal` client capabilities: kage runs its own tools
in-process and gates them through `session/request_permission`.

## hand-driven smoke test

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}' \
  | kage rpc
```

Two framed JSON-RPC results come back, one per line: the agent's
capabilities, then a fresh `sessionId`.
