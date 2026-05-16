# lua api

Every function below is reachable as `kage.<name>` from inside a
plugin script. Types are described in TypeScript-ish notation for
readability; Lua is dynamically typed.

## host

### `kage.now_ms()`

Wall-clock milliseconds since the Unix epoch as an integer.

### `kage.log(level: string, message: string)`

Record a structured log line. `level` is one of `"trace"`, `"debug"`,
`"info"`, `"warn"`, `"error"`.

### `kage.config()`

Return a copy of the host-supplied configuration table (the runtime
build does not allow mutation back to the host).

## ui

### `kage.ui.notify(message: string, level?: string)`

Show a transient toast in the TUI (stderr in print mode). `level` is
`"info"` (default), `"warning"`, or `"error"`; non-info levels are
also recorded through the log sink so the severity is not lost. An
unrecognized level raises an error.

`kage.notify(message)` is a back-compat alias for the same function;
the optional level argument is additive, so existing single-argument
callers are unaffected.

### blocking dialogs

`kage.ui.select`, `confirm`, `input`, and `editor` look synchronous
but do not block the host. The calling handler's coroutine *suspends*;
the host opens the overlay, and when the user answers it resumes the
coroutine with the result. Write them like ordinary blocking code:

```lua
local color = kage.ui.select("Pick a color", { "red", "green", "blue" })
if color == nil then return end          -- user cancelled
kage.ui.notify("you picked " .. color)
```

These may be called from any bridged handler: a
[command](#commands) handler or a [keybinding](#keybindings) handler.
Only one dialog can be open at a time per plugin runtime.

### `kage.ui.select(title: string, items)` -> value | nil

Open a fuzzy picker. `items` is an array; each entry is either a
string (label and value both that string) or a table
`{ label, value?, detail? }` (`value` defaults to `label`). Returns
the chosen entry's `value`, or `nil` if the user cancelled.

### `kage.ui.confirm(title: string, message: string)` -> boolean

Open a yes/no overlay. Returns `true` / `false`. Cancelling (Esc /
Ctrl+C) counts as `false`, so the result is always a boolean.

### `kage.ui.input(title: string, placeholder?: string)` -> string | nil

Open a single-line input. Returns the entered string, or `nil` if
cancelled. The placeholder is dimmed help text and is not part of the
result.

### `kage.ui.editor(title: string, prefill?: string)` -> string | nil

Open a multi-line editor seeded with `prefill`. `Ctrl+S` submits,
`Esc` cancels. Returns the final buffer, or `nil` if cancelled.

### `kage.ui.set_header(fn | nil)` / `kage.ui.set_footer(fn | nil)`

Take over the top status row (`set_header`) or the bottom modeline
row (`set_footer`). The host calls `fn(width)` once per redraw and
paints the returned styled lines in place of the built-in chrome.
Passing `nil` clears the slot and restores the built-in row. The `:`
command line and `/` search line still take priority over a custom
header.

`fn(width)` returns one of: a plain string (one unstyled span), a
span table, or an array of those (one line per element; an element
that is itself an array of spans is a multi-span line). A span table
is `{ text, fg?, bg?, bold?, dim?, italic?, underline? }`; colors are
strings the host resolves against the active theme (`"red"`,
`"#1f1f28"`). A `nil` return, a non-conforming value, or an error
logs and paints the built-in row instead (no silent failure).

```lua
kage.ui.set_footer(function(width)
  return { { text = "branch: ", dim = true },
           { text = "main", fg = "green", bold = true } }
end)
```

The render function runs inside the shared Lua mutex, so keep it
cheap: no blocking dialogs, no network.

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
  aliases     = { "br", "git-branch" },  -- optional
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

`aliases` are alternate names that resolve to the same command (so
`:br` runs `:branch`); they appear in the palette and `:help`. A
command is rejected whole if its name *or* any alias collides with a
built-in - use `kage.override_command` to shadow a built-in on
purpose.

The handler runs through the coroutine bridge, so it may call the
blocking [`kage.ui.*`](#ui) dialogs directly.

### `kage.override_command(spec)`

Same `spec` as `register_command`, but the command is **allowed to
shadow a built-in** of the same name and is dispatched ahead of it
(parity with `kage.override_tool`):

```lua
kage.override_command({
  name = "help",
  description = "my help",
  handler = function() return "see :keybindings and :events too" end,
})
```

Now `:help` runs your handler instead of the built-in. Overrides
live in their own registry, so removing the plugin restores the
built-in.

## keybindings

### `kage.register_keybinding(spec, handler)`

Bind a chord to a handler. `spec` is either a chord string or a
table `{ key = "...", description? = "..." }`:

```lua
kage.register_keybinding("ctrl+shift+x", function()
  kage.ui.notify("hello from a chord")
end)

kage.register_keybinding({ key = "f5", description = "reload" }, reload)
```

Chord grammar (case-insensitive, modifiers in any order):

- modifiers: `ctrl`, `alt`, `shift`, `super` (aliases: `control`,
  `option`/`opt`, `cmd`/`command`/`meta`/`win`)
- key: a single character, a named key (`enter`, `esc`, `tab`,
  `space`, `backspace`, `delete`, `up`, `down`, `left`, `right`,
  `home`, `end`, `pageup`, `pagedown`, `insert`), or `f1`..`f12`

A plugin chord wins over the built-in binding for that key (it is
checked before built-in input handling, but never over an open
modal layer or `Ctrl+Q`). Binding a reserved chord (`ctrl+q`) still
works but logs a warning. The handler runs through the coroutine
bridge, so it too may open [`kage.ui.*`](#ui) dialogs; a non-empty
string return is shown as a conversation block, like a command.

## autocomplete

### `kage.add_autocomplete_provider({ name, complete })`

Add a completion provider for the prompt input. Providers form a
stack: the host consults them in reverse registration order (the
most recently added wins) on each input change and shows the first
non-empty result in a popup above the input card. Re-adding a
provider with the same `name` replaces it in place.

`complete(prefix, ctx)` is called with the run of non-whitespace
characters before the cursor and `ctx = { text, cursor }` (the full
input and the cursor byte offset, so a provider can tokenize
differently, e.g. an `@`-trigger). It returns an array of items:

```lua
kage.add_autocomplete_provider({
  name = "emoji",
  complete = function(prefix, _ctx)
    if prefix:sub(1, 1) ~= ":" then return {} end
    return {
      { value = ":tada:", label = ":tada:", detail = "party" },
    }
  end,
})
```

Item fields: `value` (required; the replacement text), `label`
(defaults to `value`), `detail` (optional dim annotation), `range`
(optional `{ from, to }` 0-based byte offsets to overwrite; absent
means the host replaces the matched prefix). A `nil`/non-table
return or an error yields no items.

In the popup: `Up`/`Down` (or `Ctrl-p`/`Ctrl-n`) navigate, `Tab`
accepts, `Esc` dismisses; any other key passes through to normal
editing and re-queries. Providers run inside the shared Lua mutex
(synchronous; keep them cheap).

A built-in provider sits at the bottom of the stack: when the token
under the cursor starts with `@`, it completes workdir-relative file
paths (directories first, dotfiles only when typed). It is the
foundation for `@file` references and needs no plugin.

## raw input

### `kage.on_terminal_input(handler) -> off`

Register a handler the host calls for every key *before* any modal
layer or built-in binding sees it. Returning a truthy value consumes
the event. The call returns an `off` function; invoking it
unregisters that handler (idempotent).

```lua
local off = kage.on_terminal_input(function(ev)
  -- ev = { code, char?, ctrl, alt, shift }
  if ev.ctrl and ev.code == "char" and ev.char == "g" then
    kage.ui.notify("intercepted ctrl+g")
    return true                              -- consume
  end
  return false
end)
```

`code` is `"char"` (with `char` set), `"enter"`, `"esc"`, `"tab"`,
`"backtab"`, `"backspace"`, an arrow / nav key, `"f1"`..`"f12"`, or
`"other"`. Handlers run synchronously in the shared Lua mutex.

This is a sharp tool. Prefer
[`kage.register_keybinding`](#keybindings) for "run X on chord Y":
it is declarative, appears in help, and cannot wedge the UI. A
handler that always returns truthy makes the editor unusable, so
the host still honors its hard `Ctrl+Q` quit hatch ahead of these
hooks. A handler error or non-boolean return is treated as "not
consumed".

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

Subscribe to an event. Multiple handlers per event fire in
registration order; a handler that raises is logged and skipped so
one bad plugin does not silence the rest.

Plain notification events (the handler's return value is ignored):

| Event                    | Handler argument                                  |
| ------------------------ | ------------------------------------------------- |
| `before_agent_start`     | `{ system_prompt, first_user_message }`           |
| `agent_start`            | `{}`                                              |
| `agent_end`              | `{ ok }`                                           |
| `turn_start`             | `{ index }`                                       |
| `turn_end`               | `{ index, had_tool_calls }`                       |
| `message_start`          | `{ id }`                                           |
| `message_update`         | `{ id, delta }`                                   |
| `message_end`            | `{ id, usage }`                                   |
| `after_provider_response`| `{ id, usage }`                                   |
| `tool_call`              | `{ id, name, input }`                             |
| `tool_update`            | `{ id, content, structured? }`                    |
| `tool_result`            | `{ id, is_error, text }`                          |
| `session_open`           | `{ ... }`                                          |
| `session_close`          | `{ ... }`                                          |
| `model_select`           | `{ prev, next, source }`                          |
| `thinking_level_select`  | `{ prev, next, source }`                          |
| `user_bash`              | `{ cmd, mode }`                                   |

`usage` is `{ input, output, cache_read, cache_write }`. `source`
is `"set"`, `"cycle"`, or `"restore"`. `tool_update` only fires
when at least one handler is subscribed.

### transform hooks

These chain: each handler receives the value the previous one
produced and returns a replacement, or `nil` for "no change".

- `transform_context` - argument is the message-history array; the
  loop replaces history with whatever the last handler returns.
  Use it to redact secrets or trim old tool output per turn.
- `before_provider_request` - argument is the serialized provider
  request; rewrite it to inject a system header, strip a tool, or
  swap the model.
- `compact_prepare` - fired right before history compaction calls
  the summarizer model. Argument is
  `{ transcript, instruction, prompt, model, summarized, kept }`.
  Return a table with `prompt` and/or `instruction` to steer the
  summary, or `summary` to skip the model call entirely and use
  that text as the summary body. `nil` passes through unchanged; an
  error aborts compaction.

### predicate hook

- `should_stop_after_turn` - argument is the turn summary; any
  handler returning `true` halts the run after `turn_end` (a
  plan-mode plugin can stop before execution).

### cancellable session-op hooks

`session_before_switch`, `session_before_fork`, and
`session_before_tree` fire before the host runs the action. The
argument is the target string (session id / entry id). Return
`nil` to proceed, `{ cancel = "reason" }` to veto, or
`{ patch = "new-target" }` to redirect.

### `resources_discover`

Fires once at startup with no argument. Return
`{ skills? = {paths}, templates? = {paths}, themes? = {paths} }`;
the loader adds those directories to the filesystem-discovered set.

## session

### `kage.session.list()`

Return an array of session entries the host knows about:

```lua
for _, s in ipairs(kage.session.list()) do
  print(s.id, s.value) -- short id, absolute path
end
```

### `kage.session.fork(at?: string)`

Ask the host to fork the current session at entry-id prefix `at`
(or the latest entry when omitted). Returns `nil`; the host drains
the request between turns and writes a new session file.

### `kage.session.append_entry(kind: string, data?: table)`

Append a custom entry to the session JSONL. `kind` is a non-empty
namespaced string (e.g. `"my-plugin:bookmark"`); `data` is any
table, JSON-serialized (defaults to `{}`). The host writes it
between turns. Pair it with a custom block renderer to display
your own entry kind end to end.

### `kage.session.set_label(anchor: string, label?: string)`

Write a label entry pointing at the entry id `anchor`. Passing
`label = nil` clears it. Used for bookmarking / "mark this point"
workflows.

## conversation

### `kage.send_message(text: string, opts?: table)`

Queue a synthetic message the host delivers between turns:

```lua
kage.send_message("re-run the tests", { trigger_turn = true })
```

`opts` fields: `trigger_turn` (bool, default `true`) and
`deliver_as` (default `"user"`). In v0.1 only `"user"` is wired;
`"assistant"` / `"system"` raise an error rather than silently
doing the wrong thing.

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
advisory; for full control over the summary subscribe to the
[`compact_prepare`](#transform-hooks) transform event, which can
rewrite the prompt/instruction or replace the summary outright.

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
