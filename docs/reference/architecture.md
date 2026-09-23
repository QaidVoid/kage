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
kage-tui        (core + loop + plugin)
kage-sandbox                                          (empty placeholder)
kage-cli        (binary)            (depends on everything it uses)
```

Layering is strict: depend only downward. The `kage-cli` binary is
the only crate that wires the whole graph together.

## what each crate owns

| Crate            | Responsibility                                      |
| ---------------- | --------------------------------------------------- |
| `kage-core`      | Message types, content blocks, errors, cancel flag  |
| `kage-jsonrpc`   | Shared bidirectional JSON-RPC peer over stdio       |
| `kage-provider`  | LLM provider clients, registry, model catalog       |
| `kage-tools`     | Tool trait, built-in tools, tool registry           |
| `kage-session`   | Append-only JSONL writer, replay, fork, search      |
| `kage-loop`      | The agent loop, compaction, hooks                   |
| `kage-mcp`       | MCP client (external tool servers) and MCP server (kage's built-in tools over stdio) |
| `kage-acp`       | ACP agent (editors drive kage) and ACP client (kage drives another agent as a provider) |
| `kage-plugin`    | Lua runtime, sandbox, host API surface              |
| `kage-tui`       | The interactive TUI, modal input, block renderer    |
| `kage-sandbox`   | Reserved slot for OS-level command isolation; ships empty in 0.1 |
| `kage-cli`       | The binary, CLI flags, main wiring                  |

## data flow per turn

```
user keypress
   v
kage-tui modal dispatch
   v  (submits text)
kage-cli worker thread
   v
kage-loop run()
   v -> kage-provider stream request
   v <- provider streams events
   v
hooks emit LoopEvents
   v
kage-plugin dispatch ("turn_start", "message_update", ...)
   v
kage-tui buffer updates
   v -> kage-session writer appends JSONL
   v
kage-tui repaints the visible region
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

Lua state is wrapped in an `Arc<Mutex<Lua>>` so the synchronous tool
dispatch path can call back into Lua without re-entrancy. The mutex
also serializes plugin status writes and event dispatches.

## why no async

The agent loop is human-paced (one prompt at a time, one stream at a
time). Providers expose blocking iterators over server-sent events.
Tools run sequentially in the simplest case, in parallel by explicit
opt-in for the read-only ones. None of this benefits from `tokio`,
and adding it forces every layer to colour-async. The synchronous
design keeps each crate small and the call stack readable.
