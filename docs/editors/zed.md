# zed

`kage rpc` speaks JSON-RPC 2.0 over stdio with LSP-style
`Content-Length` framing, the same transport Zed already uses for
language servers and external agents. Zed spawns the process, sends
requests on stdin, and reads responses and progress notifications on
stdout.

## start the server

```sh
kage rpc
```

Optional flags:

- `-m, --model <provider:model>` pins the model for the connection
  (default: the first authed provider's default model).
- `--system <text>` overrides the system-prompt role.

Credentials are resolved exactly as for the TUI and print mode: the OS
keyring, `kage auth login`, or an API-key environment variable. If no
provider is configured the process prints a message and exits with a
non-zero code, so a wrapper can surface that to the user.

## configure zed

Add `kage rpc` as a custom agent server in Zed's `settings.json`:

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

Use an absolute `command` path if `kage` is not on Zed's `PATH`. Pass
a model with `"args": ["rpc", "-m", "anthropic:claude-sonnet-4-6"]`.

## protocol

Every message is `Content-Length: <bytes>\r\n\r\n<json>`.

Client to server (requests carry an `id`; omit `id` for a
notification):

| method               | params                                  |
| -------------------- | --------------------------------------- |
| `initialize`         | `{}` (optional `protocol_version`)      |
| `prompt`             | `{ prompt, model?, session? }`          |
| `cancel`             | none (cancels the in-flight prompt)     |
| `permission/respond` | `{ id, allow, reason? }`                |
| `session/load`       | `{ id }`                                |
| `session/list`       | none                                    |

`initialize` replies with `{ name, version, protocol, model,
methods }`. `prompt` streams progress and replies once the turn
finishes.

Server to client notifications (no `id`):

- `event` carries one agent-loop event in `params`. The event
  alphabet is identical to `kage -p --json`: `message_start`,
  `text_delta`, `thinking_delta`, `tool_call_start`,
  `tool_call_args_delta`, `tool_call_end`, `message_end`,
  `compaction`, `error`.
- `permission/request` carries `{ id, name, input }` for a tool the
  agent wants to run. The agent blocks until the editor answers with
  a `permission/respond` whose `id` matches. Permissions are never
  auto-approved; a cancelled run resolves the prompt as denied.

## minimal hand-driven session

```sh
req='{"jsonrpc":"2.0","id":1,"method":"initialize"}'
printf 'Content-Length: %d\r\n\r\n%s' "${#req}" "$req" | kage rpc
```

The reply is a single framed JSON-RPC result describing the server.
Send a `prompt` the same way to drive a full turn; answer any
`permission/request` notification with a framed `permission/respond`.
