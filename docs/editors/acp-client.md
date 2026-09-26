# acp client

The other direction: drive **another** ACP agent *from* kage. kage
becomes the ACP client and the upstream agent (any program that
speaks ACP over stdio, including another `kage rpc`) is used as a
model. This is the inverse of
[zed](./zed) / [neovim](./neovim), where kage is the agent being
driven.

## configure an agent

Declare agents under `[acp.agents.<name>]` in
`~/.config/kage/config.toml`. Each entry is the launch command:

```toml
[acp.agents.upstream]
command = "kage"
args = ["rpc", "-m", "anthropic:claude-sonnet-4-6"]

[acp.agents.upstream.env]
ANTHROPIC_API_KEY = "sk-..."
```

Then select it as the model:

```sh
kage -m acp:upstream -p "explain this file"
```

`acp:<name>` resolves to the configured agent. kage spawns it, speaks
ACP, forwards your turn as `session/prompt`, and streams its reply
back. kage ships **no presets**. You declare the command yourself,
the same way you would an MCP server or an LSP.

## configure from a plugin

Plugins can declare agents at runtime. Plugins *configure* and core
spawns. Naming a command for the host to spawn is process execution,
so `kage.acp.add_agent` requires the `exec` capability
(`[plugins.capabilities] your-plugin = ["exec"]`, then request it at
load):

```lua
kage.request_capabilities({ "exec" })
kage.acp.add_agent({
  name = "reviewer",
  command = "my-agent",
  args = { "--acp" },
  env = { MY_AGENT_MODE = "review" },
})
```

Static `[acp.agents.*]` config wins on a name clash.

## tool permissions

The upstream agent runs its **own** tool loop. When it wants to run a
tool it asks kage (`session/request_permission`). kage **never
auto-approves** another agent's tools. Decide with a plugin policy:

```lua
kage.on_acp_permission(function(req)
  -- req carries the upstream tool call; return a boolean.
  -- This is policy, not UI: it must NOT open a dialog.
  return false
end)
```

Return `true` to allow, `false` (or no handler at all) to deny. A
handler that errors or returns a non-boolean also denies.

## v1 limitations

- Only the upstream's assistant **text and thinking** are surfaced.
  Its own `tool_call` / `plan` / mode updates are not relayed into
  kage's loop (`supports_tool_use` is `false`).
- A `session/update` of a kind kage does not know is ignored, so an
  upstream that speaks a newer ACP revision keeps streaming.
- kage advertises **no** `fs` / `terminal` client capabilities, so a
  conformant upstream will not ask kage to read/write files or open
  terminals on its behalf. It uses its own.
- `kage.on_acp_permission` is a synchronous policy callback. An
  interactive "ask the human" prompt for an upstream tool is not
  available in v1. The handler must decide programmatically.
- Every turn starts a **fresh agent process and session**, and only
  the latest user message is forwarded. The upstream has no memory
  of the conversation beyond that message.
- The upstream's stop reason is collapsed to a normal end: a
  `max_tokens` cutoff or a refusal is not distinguished, and token
  usage is not reported for ACP turns.
