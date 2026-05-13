# lua api

Every function below is reachable as `kage.<name>` from inside a
plugin script. Types are described in TypeScript-ish notation for
readability; Lua is dynamically typed.

## host

### `kage.now_ms()`

Wall-clock milliseconds since the Unix epoch as an integer.

### `kage.notify(message: string)`

Show a transient toast in the TUI. In print mode this writes to
stderr.

### `kage.log(level: string, message: string)`

Record a structured log line. `level` is one of `"trace"`, `"debug"`,
`"info"`, `"warn"`, `"error"`.

### `kage.config()`

Return a copy of the host-supplied configuration table (the runtime
build does not allow mutation back to the host).

## tools

### `kage.register_tool(spec)`

Register a new tool. The agent can call it the same way it calls
built-in `read` or `bash`. Spec fields:

```lua
{
  name        = "echo",                       -- string, required
  description = "echo back the input",        -- string, required
  schema      = { type = "object" },          -- json schema, required
  risk        = "read",                       -- "read" | "write" | "network"
  execute     = function(input) ... end,      -- (table) -> string | table
}
```

`execute` may return a string (the tool output text) or a table:

```lua
return {
  is_error    = false,
  text        = "ok",
  structured  = { count = 7 },
}
```

### `kage.override_tool(spec)`

Same shape as `register_tool` but replaces the existing entry by name.
Useful for sandboxing `bash`, auditing `write`, etc. The host logs a
warning if no tool with that name was previously registered.

## commands

### `kage.register_command(spec)`

Register a slash / colon command:

```lua
{
  name        = "branch",
  description = "current git branch",
  args        = {
    { name = "verbose", kind = "flag" },
  },
  handler     = function(args)
    -- args.rest, args.verbose, args.<arg-name>
  end,
}
```

Argument `kind` values: `"text"`, `"choice"`, `"path"`, `"session"`,
`"flag"`. For `"choice"`, also supply `choices = { "...", ... }`.

## widgets and status

### `kage.register_widget({ key, render })`

Register a status-bar widget. `render(width)` runs once per redraw
and returns a string painted on the right edge of the status bar.

```lua
kage.register_widget({
  key = "clock",
  render = function(_width)
    return os.date("%H:%M")
  end,
})
```

### `kage.set_status(key: string, text: string | nil)`

Push or clear a transient status entry. Plain text only; the host
paints the value on the status bar between widgets.

### `kage.clear_status(key: string)`

Remove a status entry. Equivalent to `kage.set_status(key, nil)`.

## events

### `kage.on(event: string, handler)`

Subscribe to a loop event. Handler signature varies by event:

| Event             | Handler argument                                     |
| ----------------- | ---------------------------------------------------- |
| `agent_start`     | `()`                                                 |
| `agent_end`       | `({ tokens_in, tokens_out, ... })`                   |
| `turn_start`      | `({ index })`                                        |
| `turn_end`        | `({ index, usage })`                                 |
| `message_start`   | `({ role })`                                         |
| `message_update`  | `({ delta })`                                        |
| `message_end`     | `({ text })`                                         |
| `tool_call_start` | `({ id, name, input })`                              |
| `tool_call_end`   | `({ id, name, output, is_error })`                   |
| `compaction`      | `({ kept, summarized, summary })`                    |

Returning `false` from a cancellable hook (where supported) vetoes
the action.

## session

### `kage.session.list()`

Return an array of session entries the host knows about:

```lua
for _, s in ipairs(kage.session.list()) do
  print(s.id, s.value) -- short id, absolute path
end
```

### `kage.session.fork(at?: string)`

Ask the host to fork the current session at entry id `at` (or the
latest entry when `at` is omitted). Returns `nil` in v0.1; the fork
runs asynchronously. The host pushes a toast with the new session id
when the fork completes.

## context inspection

### `kage.context_usage()`

Snapshot the current per-turn token usage:

```lua
local u = kage.context_usage()
print(u.model, u.input_tokens, u.output_tokens, u.context_window)
```

Returns `nil` until the host has run at least one turn.

### `kage.compact(prompt?: string)`

Ask the host to run a compaction pass. The optional prompt is
advisory in v0.1 (a dedicated `on_compact_prepare` hook is planned).

## fs

### `kage.fs.read(path: string)`

Read a file relative to the session workdir. Paths outside the
workdir tree raise an error.

### `kage.fs.write(path: string, contents: string)`

Write a file under the workdir. Same path restriction as `read`.

## http

### `kage.http.get(url: string)`

HTTP GET. Returns `{ status, body, headers }`. The allow-list is
host-controlled; unauthorized hosts raise an error.

## providers

### `kage.register_provider(spec)`

Register a new LLM provider implementation. Advanced; see the
`pi-ai` plugin in the examples folder for a realistic shape.
