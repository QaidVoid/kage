# architecture

This page sketches the crate graph and the data flow through a turn.
For exhaustive detail, the code is the source of truth.

## crate graph

```
kage-core                                             (leaf)
kage-jsonrpc                                          (depends on core)
kage-provider  kage-session  kage-tools               (depend on core)
kage-mcp        (core + jsonrpc + tools)
kage-acp        (core + jsonrpc + provider)
kage-plugin     (core + provider + tools)
kage-loop       (core + provider + tools)
kage-tui        (core + plugin)
kage-cli        (binary)            (depends on everything it uses)
```

Layering is strict: depend only downward. The `kage-cli` binary is
the only crate that wires the whole graph together.

## what each crate owns

| Crate            | Responsibility                                      |
| ---------------- | --------------------------------------------------- |
| `kage-core`      | Message types, content blocks, errors, the cancel tree, the engine protocol (events, commands, the agent tree and the MCP catalog types), the resource block format, agent definitions, the keymap, option and highlight registries |
| `kage-jsonrpc`   | Shared bidirectional JSON-RPC peer over stdio, with an optional cancel notice per connection |
| `kage-provider`  | LLM provider clients, registry, the bundled model catalog and the optional cache that `kage models refresh` writes |
| `kage-tools`     | Tool trait, built-in tools, tool registry           |
| `kage-session`   | Append-only JSONL writer, replay, fork, search      |
| `kage-loop`      | The agent loop, compaction, hooks                   |
| `kage-mcp`       | MCP client (tools, resources, prompts, the OAuth protocol, prompt expansion) and MCP server (kage's built-in tools over stdio) |
| `kage-acp`       | ACP agent (editors drive kage) and ACP client (kage drives another agent as a provider) |
| `kage-plugin`    | Lua runtime, sandbox, host API surface, embedded stdlib and defaults, `init.lua` loading |
| `kage-tui`       | The interactive TUI, modal input, block renderer    |
| `kage-cli`       | The binary, CLI flags, the session engine, frontend wiring |

## the engine

Every frontend (the TUI, `kage rpc` for editors, and `-p` print mode)
drives the same engine through two channels:

- **Events out.** Each event is an envelope addressed to the session
  that produced it, with a per-session sequence number:
  `{"session":"01J...","seq":42,"type":"text_delta",...}`. Durable
  events describe state a client keeps (a message was appended, a run
  ended, the model changed). Live events are deltas and progress a
  client may drop (streamed text, tool output tails). `kage -p --json`
  prints these envelopes one per line.
- **Commands in.** Prompt, cancel, answer a permission request, switch
  model or thinking level, compact, run a shell command, restart an
  MCP server, and the session operations (new, resume, fork, clone,
  delete, export).

A dispatcher thread owns the sessions and never blocks on a run. Each
run executes the agent loop on its own thread and hands the session
back when it ends. Permission questions travel over the same channels:
the engine publishes `permission_requested` and waits for the client's
answer. The TUI renders the stream into its buffer, and the ACP adapter
turns it into ACP traffic (see [the ACP projection](#the-acp-projection)).

## agents

An agent is an engine session with a parent link. The `agent` tool
never touches sessions itself. It sends a spawn request to the
dispatcher and waits on its reply channel and its cancel flag.

```text
parent runner thread          dispatcher                     agent runner thread
  loop -> agent tool
    spawn request ---------->  check the parent, definition, depth
                               open the agent session
                               publish agent_spawned
                               start its run, or queue it  --->  kage-loop run()
    wait on the reply ...                                        (own seq, file,
                                                                  cancel flag)
                               run finished  <-----------------
                               send the result, start the
                               next queued agent
    <----------- tool result
  loop appends the result and continues
```

The dispatcher still never blocks, and the wait graph stays a DAG: a
parent's tool thread waits on a reply that the dispatcher sends when
the agent's run ends, never the reverse. The link lives on the engine's
session, with the parent, the depth and the reply channel. The first
run that finishes takes the channel, so later runs a user starts in
the agent never answer the parent twice. The running limit counts
agent runs in flight, except agents that wait on their own agents, and
queued agents start in spawn order.

An agent session is built from its parent's live state: the
definition's model and thinking level over the parent's, the parent's
tools filtered by the definition, a clone of the parent's permission
gate (its rules, mode and session approvals are shared), and no plugin
runtime or MCP manager of its own. Its runs forward no plugin events
and use the plain run hooks. The `agent` tool is registered per run,
never stored in the session's tools, so plugin reloads and MCP
refreshes cannot drop or duplicate it. It is registered while the
session's depth is below `agent_max_depth`.

**The cancel tree.** `CancelFlag` is a node with an optional parent.
An agent's flag is a child of its parent's, and `is_cancelled` walks up
the chain. Cancelling a session stops every agent below it through the
checks every loop, tool and gate already makes, while cancelling an
agent never reaches its parent. The dispatcher resets a session's own
flag when its run ends, so an idle parent that was cancelled cannot
cancel a run later started in one of its agents.

**The agent tree.** Agents need one new event and no new commands.
`agent_spawned` is published as the agent's first envelope, on the
agent's own session:

```json
{"session":"<agent>","seq":1,"type":"agent_spawned","parent":"<parent>","tool_call_id":"<call>","agent":"explore","description":"map exports"}
```

After it, the agent publishes the usual events on its own session.
Steering, messaging and stopping an agent are the ordinary prompt and
cancel commands addressed to its session. Commands that replace or
copy a session (new, resume, fork, clone) are refused for an agent.
`AgentTree` in `kage_core::protocol` folds envelopes into one node per
agent with its parent, state, usage, tool count, latest tool and open
approvals. The TUI builds its cards, pinned list, drill-in views,
breadcrumb and agents overlay from it, and routes each agent's loop
events into that agent's own buffer. The ACP adapter uses it to find
an agent's parent session, and for editors without subagent support,
the client session at the root of the agent's branch.

## mcp in the engine

Each session owns an `McpManager` with every configured server, live
or not, and a cached catalog of their tools, resources, resource
templates and prompts. Agents have no manager of their own.

**The catalog event.** `mcp_servers` is a live event with the whole
catalog: each server's name, status (`connected`, `failed` with its
error, or `needs_auth`), tool count, resources, templates and prompts.
The dispatcher publishes it when a session opens, after a restart, and
when a reload changed the catalog. The latest snapshot wins, and it is
never recorded. The TUI builds `@server:` completion, the prompt
commands and the `/mcp` picker from it, and the ACP adapter builds the
editor's command list.

**The restart command.** `restart_mcp {server}` restarts one server.
In-flight tool calls hold the old connection, so a busy session keeps
the name until its next run starts, and an idle session restarts at
once. The TUI sends it from `/mcp restart`, from the picker and after
a login. `kage.mcp.restart` from Lua joins the same list at run start.

**MCP work off the dispatcher.** The dispatcher never waits on an MCP
server. At run start it lends the manager to the run thread, which
applies the pending restarts and the list reloads that servers
announced, then hands the manager back with the resulting tool changes
before the loop starts. An idle restart does the same on a worker
thread and keeps the session busy until the manager comes back.

**Expansion on the run thread.** After those reloads, the run expands
the prompt with `kage_mcp::expand` before it enters history: an MCP
prompt command at the start is replaced by the prompt's messages, and
each `@server:uri` mention is read and appended as a resource block.
The expanded message is what the model receives and what the recorder
writes, and clients shorten resource blocks for display. When
expansion fails, the run publishes an error notice and fails with
history untouched. Steered text is never expanded, which is why the
TUI queues such prompts and ACP queues every prompt.

**Cancel notices.** A `kage-jsonrpc` connection can build one
notification for a request it abandons, whether through a user cancel
or a deadline. MCP connections send `notifications/cancelled` (never
for `initialize`), and the ACP agent sends `$/cancel_request` when it
withdraws a permission request. A closed connection sends nothing.

**OAuth.** `kage-mcp` implements the protocol (discovery, PKCE,
registration, the loopback listener, token exchange and refresh) and
never touches disk. The HTTP transport asks a `TokenSource` for a
bearer token. `kage-cli` implements it over `mcp-auth.json` and owns
refresh and storage.

## the ACP projection

`kage rpc` maps each ACP session to an engine session and turns the
event stream into ACP traffic with one bus subscriber:

- Loop events become `session/update` chunks and tool call updates.
  `session/load` replays recorded history through the same mapping.
- `usage_updated` becomes `usage_update`, `title_changed` becomes
  `session_info_update`, a `state_changed` that moves the model,
  thinking level or permission mode becomes `config_option_update`,
  and `mcp_servers` becomes `available_commands_update`.
- `permission_requested` becomes `session/request_permission`, asked
  on its own thread. The asks still open when the run ends are
  withdrawn before the prompt answers.
- For a client with the subagents capability, `agent_spawned`
  registers the agent as a child session, announced with
  `subagent_update`, whose events then take the same path under the
  child's id. A client session's prompt answers only after every child
  has sent its final state. Other clients get agent progress and asks
  on the root session's `agent` call.

Updates for a session are held until the response to the
`session/new`, `session/load` or `session/resume` that names it is
written, so a client never sees them first.

Config option changes and prompts become ordinary engine commands, so
the editor, the TUI and print mode share one implementation.

## data flow per turn

```
user keypress
   v
kage-tui input dispatch
   v  (a request)
kage-cli TUI host  ->  engine command
   v
engine runner thread: kage-loop run()
   v -> kage-provider stream request
   v <- provider streams events
   v
each loop event:
   -> Lua plugin events ("turn_start", "message_update", ...)
   -> session recorder appends JSONL
   -> event bus: TUI, ACP client, or print output
   v
kage-tui applies the event and repaints the visible region
```

The loop is fully synchronous. There is no async runtime in core.

## sessions on disk

A session file is a single JSONL stream. The first line is a
`header`. Every entry carries its own `id` and `ts` so forks can
branch from any point:

```jsonl
{"type":"header","version":1,"session":"...","id":"...","ts":"...","cwd":"...","model":"...","system_prompt":"..."}
{"type":"message","id":"...","ts":"...","message":{"role":"user","content":[...]}}
{"type":"message","id":"...","ts":"...","message":{"role":"assistant","content":[...]}}
{"type":"compaction","id":"...","ts":"...","kept":4,"summarized":12,"summary":"..."}
```

Tool calls and results are not separate entries: they ride inside
`message` content blocks. The remaining entry kinds are
`thinking_level_change`, `model_change`, `label`, `title`, and the
plugin-defined `custom`.

An agent's file sits next to its parent's. Its header's
`parent_session` names the parent, and its first entry is a `custom`
entry of kind `kage:agent` with the parent, the `agent` call id, the
agent name and the task description, followed by a `title` entry.
Session listings read that entry to hide agent files from `kage list`
and the pickers, and `/tree` shows them under their parent.

Files are append-only: the writer never rewrites prior lines, and an
advisory lock rejects a second concurrent writer. To branch from an
existing session, fork instead.

## the Lua layer

Lua is kage's configuration and extension layer. From the top down:

```text
~/.config/kage/init.lua, lua/**   trusted user config, all capabilities
plugins/*.lua (sorted by name)    sandboxed, capabilities on request
kage.* stdlib (embedded Lua)      kage.on, kage.keymap, kage.ui.set_slot, aliases
_defaults.lua (embedded Lua)      default keymaps and slot specs
kage.api.* (Rust primitives)      autocmds, options, keymaps, highlights, slots
kage-core registries              Keymap, OptionStore, Highlights (plain data)
Rust renderers and components     kage-tui
```

`kage-plugin` builds one `PluginRuntime` per process. A single owner
thread holds the Lua state. Everything else (tools, event dispatch,
commands, key handlers, option changes from the TUI) sends it jobs
and waits for the reply, so calls never interleave. Between jobs the
same thread runs `kage.schedule`, `kage.defer` and `kage.timer`
callbacks from a deadline heap. A long Lua tool or provider occupies
the thread until it finishes.

The TUI loads, in order: `_defaults.lua`, the plugins sorted by file
name, `[keybindings]` from `config.toml`, then `init.lua`, and fires
`color_scheme` once. Each layer overrides the ones before it. Print
mode and `kage rpc` load `_defaults.lua` and the plugins, never
`init.lua`. Each file
evaluates in its own environment with private copies of the shared
`kage` tables, so no plugin can change another's view or reach the
trusted user environment.

The keymap table, the option store and the highlight table are plain
data in `kage-core`, shared behind mutexes with generation counters.
Lua writes them on the owner thread. The TUI reads them under a short
lock and recompiles what it needs when a generation moves, so looking
up a key or painting a color never runs Lua. A key mapped to a Lua
function is sent to the owner thread as a job.

Autocmd metadata and subscriber counts live on the Rust side, so an
event nobody subscribes to never reaches the owner thread. Slot
components, widgets and block renderers keep their last output. Slot
components recompute on the owner thread only when a listed event
fires, their interval passes, or the width changes, and the renderer
reads the retained lines, so the screen never waits on Lua.

## why no async

The agent loop is human-paced (one prompt at a time, one stream at a
time). Providers expose blocking iterators over server-sent events.
Tools run sequentially in the simplest case, in parallel by explicit
opt-in for the read-only ones, and a batch made only of `agent` calls
runs its agents at once on plain threads. None of this benefits from `tokio`,
and adding it forces every layer to colour-async. The synchronous
design keeps each crate small and the call stack readable.
