# architecture

This page sketches the crate graph and the data flow through a turn.
For exhaustive detail, the code is the source of truth.

## crate graph

```
kage-core                                              (leaf)
kage-provider  kage-tools  kage-session  kage-sandbox  (depend on core)
kage-loop                                              (depends on provider + tools)
kage-mcp  kage-acp  kage-plugin  kage-tui              (depend on loop)
kage-cli   (binary)                                    (depends on everything it uses)
```

Layering is strict: depend only downward. The `kage-cli` binary is
the only crate that wires the whole graph together.

## what each crate owns

| Crate            | Responsibility                                      |
| ---------------- | --------------------------------------------------- |
| `kage-core`      | Message types, content blocks, errors, cancel flag  |
| `kage-provider`  | LLM provider clients, registry, model catalog       |
| `kage-tools`     | Tool trait, built-in tools, tool registry           |
| `kage-session`   | Append-only JSONL writer, replay, fork, search      |
| `kage-sandbox`   | Path resolution and workdir guards                  |
| `kage-loop`      | The agent loop, compaction, hooks                   |
| `kage-mcp`       | MCP client (deferred to post-0.1)                   |
| `kage-acp`       | ACP server mode (deferred to post-0.1)              |
| `kage-plugin`    | Lua runtime, sandbox, host API surface              |
| `kage-tui`       | The interactive TUI, modal input, block renderer    |
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

A session file is a single JSONL stream:

```jsonl
{"v":1,"kind":"header","ts":"...","session":"...","model":"...","system":"..."}
{"kind":"message","ts":"...","id":"...","message":{"role":"User","content":[...]}}
{"kind":"message","ts":"...","id":"...","message":{"role":"Assistant","content":[...]}}
{"kind":"tool-call","ts":"...","id":"...","call_id":"...","name":"read","input":{...}}
{"kind":"tool-result","ts":"...","id":"...","call_id":"...","output":"...","is_error":false}
{"kind":"compaction","ts":"...","id":"...","kept":4,"summarized":12,"summary":"..."}
```

Files are append-only. Editing one in place after the session ends
is supported but not the intended workflow; fork instead.

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
