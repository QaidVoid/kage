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
args = ["rpc", "-m", "anthropic/claude-sonnet-4-6"]

[acp.agents.upstream.env]
ANTHROPIC_API_KEY = "sk-..."
```

Any ACP-speaking program works the same way, so Command Code's agent
can be driven from kage too:

```toml
[acp.agents.commandcode]
command = "cmd"
args = ["acp"]

[acp.agents.commandcode.env]
CMD_API_KEY = "..."
```

Then select it as the model:

```sh
kage -m acp:upstream -p "explain this file"
```

`acp:<name>` resolves to the configured agent. In the CLI provider path,
kage spawns it, forwards your turn as `session/prompt`, and streams its
reply back. The desktop agent settings offer launch templates, including
the maintained Claude Agent ACP adapter. You can also configure an
agent's command yourself, as with an MCP server.

## external sessions

An agent configured under `[acp.agents.<name>]` can also run as a
persistent session. The agent keeps its own conversation, tools and
subagents, and kage shows what it does and routes the controls it
supports. Kage runs none of the agent's tools itself, so the agent works
with its own privileges. This is not a sandbox. Unlike the one-shot
`acp:<name>` provider path, the process stays up between turns and the
conversation survives them.

External sessions run on Linux. A host that supports them says so in its
`initialize` result with `_meta.kage.externalSessions: true`. The
desktop offers them when you pick `acp:<name>` on the new session
screen. Another ACP client sends `session/new` with
`_meta.kage.externalAgent` set to the configured name. The request never
carries a command: the launch settings come from your user config, and a
project's `[acp]` table is ignored.

- **One process per session.** Every client attached to the session
  shares it, and the process ends when the last client leaves or the
  host stops. A host runs at most 8 external sessions and a session
  takes at most 8 clients.
- **History.** The session file records the agent's name, its upstream
  session id and a display history: your prompts, the agent's message and
  thought text, and tool titles and statuses. Tool output, raw tool
  input and permission decisions are not recorded.
- **Reopening.** `session/load` and `session/resume` start a fresh
  process and restore the upstream session through its `session/resume`
  or `session/load`, whichever the agent advertises. An agent that
  offers neither cannot be reopened, and kage says so instead of
  starting a new conversation under the old id. A load shows the
  recorded history before it answers.
- **Permissions.** An upstream permission request goes to the client
  whose prompt is running, with the options the agent offered. Your
  choice goes back as offered, so a choice such as "allow always" applies
  to the whole upstream session, including prompts from other clients.
  With no running prompt the request is cancelled.
- **Cancelling.** Only the client whose prompt is running can cancel it.
- **Not supported.** Downstream MCP servers, forking, steering a running
  turn and kage's own models, modes, compaction and swarm. The desktop
  refuses those actions on an external session with a notice.
- **Memory.** Each session buffers at most 4 MiB of updates for slow
  clients. A client that falls 2 MiB behind fails the session, which
  you can reopen.

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

Return `true` to allow once, `false` (or no handler at all) to deny.
A handler that errors or returns a non-boolean also denies. If the agent
does not offer an option with the requested scope, kage cancels the
request instead of choosing an option that remembers the decision.

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
- A `max_tokens` stop is reported as a token limit, and cancellation
  as a cancelled turn. Refusals and `max_turn_requests` both map to
  the generic `Other` stop reason, so they cannot be distinguished.
  Token usage is not reported for ACP turns.
