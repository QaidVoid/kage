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
| `kage-core`      | Message types, content blocks, errors, cancel flag, the engine protocol (events and commands), the keymap, option and highlight registries |
| `kage-jsonrpc`   | Shared bidirectional JSON-RPC peer over stdio       |
| `kage-provider`  | LLM provider clients, registry, model catalog       |
| `kage-tools`     | Tool trait, built-in tools, tool registry           |
| `kage-session`   | Append-only JSONL writer, replay, fork, search      |
| `kage-loop`      | The agent loop, compaction, hooks                   |
| `kage-mcp`       | MCP client (external tool servers) and MCP server (kage's built-in tools over stdio) |
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
  model or thinking level, compact, run a shell command, and the
  session operations (new, resume, fork, clone, delete, export).

A dispatcher thread owns the sessions and never blocks on a run. Each
run executes the agent loop on its own thread and hands the session
back when it ends. Permission questions travel over the same channels:
the engine publishes `permission_requested` and waits for the client's
answer. The TUI renders the stream into its buffer; the ACP adapter
turns it into `session/update` notifications.

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
`header`; every entry carries its own `id` and `ts` so forks can
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
opt-in for the read-only ones. None of this benefits from `tokio`,
and adding it forces every layer to colour-async. The synchronous
design keeps each crate small and the call stack readable.
