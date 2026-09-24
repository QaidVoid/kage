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
| `kage-core`      | Message types, content blocks, errors, cancel flag, the engine protocol (events and commands) |
| `kage-jsonrpc`   | Shared bidirectional JSON-RPC peer over stdio       |
| `kage-provider`  | LLM provider clients, registry, model catalog       |
| `kage-tools`     | Tool trait, built-in tools, tool registry           |
| `kage-session`   | Append-only JSONL writer, replay, fork, search      |
| `kage-loop`      | The agent loop, compaction, hooks                   |
| `kage-mcp`       | MCP client (external tool servers) and MCP server (kage's built-in tools over stdio) |
| `kage-acp`       | ACP agent (editors drive kage) and ACP client (kage drives another agent as a provider) |
| `kage-plugin`    | Lua runtime, sandbox, host API surface              |
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

## plugins, briefly

`kage-plugin` builds a `PluginRuntime` per process, runs every `.lua`
file in the plugins directory against it, and stores Lua-registered
tools, commands, providers, and event handlers. The host pulls
snapshots when it needs to wire them into the agent loop.

A single thread owns the Lua state. Everything else (tools, event
dispatch, commands, keybindings) sends it jobs and waits for the reply,
so calls never interleave. Status widgets, header and footer chrome,
and block renderers keep their last output, so the screen never waits
on Lua. A long Lua tool or provider occupies that thread until it
finishes.

## why no async

The agent loop is human-paced (one prompt at a time, one stream at a
time). Providers expose blocking iterators over server-sent events.
Tools run sequentially in the simplest case, in parallel by explicit
opt-in for the read-only ones. None of this benefits from `tokio`,
and adding it forces every layer to colour-async. The synchronous
design keeps each crate small and the call stack readable.
