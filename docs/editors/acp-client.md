# acp client

The other direction: drive **another** ACP agent *from* kage. kage
becomes the ACP client and the upstream agent
(`claude-code-acp`, `goose`, `gemini` in ACP mode, or another
`kage rpc`) is used as a model. This is the inverse of
[zed](./zed) / [neovim](./neovim), where kage is the agent being
driven.

## configure an agent

Declare agents under `[acp.agents.<name>]` in
`~/.config/kage/config.toml`. Each entry is the launch command:

```toml
[acp.agents.claude-code]
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]

[acp.agents.claude-code.env]
ANTHROPIC_API_KEY = "sk-..."
```

Then select it as the model:

```sh
kage -m acp:claude-code -p "explain this file"
```

`acp:<name>` resolves to the configured agent; kage spawns it, speaks
ACP, forwards your turn as `session/prompt`, and streams its reply
back. kage ships **no presets** - you declare the command yourself,
the same way you would an MCP server or an LSP.

## configure from a plugin

Plugins can declare agents at runtime (the `nvim-lspconfig` analogy -
plugins *configure*, core spawns):

```lua
kage.acp.add_agent({
  name = "goose",
  command = "goose",
  args = { "acp" },
  env = { GOOSE_PROVIDER = "anthropic" },
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
- kage advertises **no** `fs` / `terminal` client capabilities, so a
  conformant upstream will not ask kage to read/write files or open
  terminals on its behalf - it uses its own.
- `kage.on_acp_permission` is a synchronous policy callback. An
  interactive "ask the human" prompt for an upstream tool is not
  available in v1; the handler must decide programmatically.
