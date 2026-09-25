# capabilities

The plugin sandbox is closed by default: no subprocesses, no
filesystem outside the workdir, no rewriting the live session, no
environment variables and no network. A few
plugins genuinely need more. Those powers are **capabilities**: opt-in,
per-plugin, and only ever attached to the one plugin that was granted
them.

## two-sided opt-in

A capability is active only when both sides agree:

1. The user grants it to a named plugin in `config.toml`. The name is
   the plugin file's stem (`rewind.lua` -> `rewind`).

   ```toml
   [plugins.capabilities]
   rewind = ["session_write", "exec"]
   ```

2. The plugin asks for it at load and adapts to the answer:

   ```lua
   local caps = kage.request_capabilities({ "session_write", "exec" })
   if not caps.session_write then
     kage.notify("rewind: disabled (grant session_write)")
     return
   end
   local files = caps.exec  -- optional extra, degrade if absent
   ```

`request_capabilities` returns a truthful `{ name = granted }` table:
a capability is `true` only if that exact plugin was granted it. The
elevated API is then attached onto that plugin's `kage` alone. Another
plugin cannot see it, even if it asks. An unknown capability name
raises rather than silently resolving to `false`, so a config typo is
loud.

## the capabilities

### `session_write`

Inspect and reseat the live conversation.

| call | effect |
| --- | --- |
| `kage.session.entries()` | metadata for every entry in the current session, in order: `{ id, kind, role?, ts }`. The rewind point picker. |
| `kage.session.switch(target)` | reseat onto an existing session (an id or a path from `kage.session.list()`). |
| `kage.session.fork_to(at?)` | fork the current session at entry-id prefix `at` (latest if omitted) and land on the new branch. The rewind move: base `kage.session.fork` branches and stays, while `fork_to` branches and goes there. |

`entries()` returns metadata only (ids, kinds and timestamps, no
message text) because it is a navigation index, not a transcript
reader. `switch` and `fork_to` only *request* a reseat. The host
applies it between turns, after consulting the `session_before_switch`
veto, so a plugin can confirm or block its own rewind.

### `exec`

```lua
local r = kage.exec({ cmd = "git", args = { "stash", "create" } })
-- r = { code = 0, timed_out = false, stdout = "...", stderr = "" }
```

Spawns a subprocess **directly, with no shell**, so there is no quoting
or injection surface. `cmd` resolves through `PATH`. The working
directory defaults to the host workdir, and `cwd` may name a directory
under it but never escape it (the same check `kage.fs` uses). The call
blocks until the process exits and returns its captured output, the
way `kage.http.get` blocks. A process still running after
`timeout_secs` seconds (30 by default, at least 1) is killed and the
result has `timed_out = true`. `code` is `-1` when a signal ended the
process.

The grant is coarse: a granted plugin may run any program with any
arguments.

### `env`

```lua
local token = kage.env("GITHUB_TOKEN")  -- string, or nil when unset
```

Reads one variable from the host process environment. It returns `nil`
when the variable is unset and raises when the value is not valid
UTF-8. Access is read-only, with no setter. There is no
per-variable allowlist, so a granted plugin can read every variable,
including secrets such as provider API keys. Grant it only to plugins
you trust.

### `net`

```lua
local res = kage.http.get("https://example.com/status")
-- res = { status = 200, body = "...", content_type = "...", truncated = false }
```

Attaches the request helpers to `kage.http`. Without the grant,
`kage.http` is an empty table and `kage.http.get` is `nil`.

| call | effect |
| --- | --- |
| `kage.http.get(url, opts?)` | GET request. |
| `kage.http.post(url, opts?)` | POST request. |
| `kage.http.delete(url, opts?)` | DELETE request. |
| `kage.http.post_stream(url, opts, fn)` | POST that calls `fn` with `{ event, data }` for each server-sent event frame. Returns `{ status, content_type }`. |

`opts` may carry `headers`, `body`, `json` (a table sent as JSON,
instead of `body`), `max_bytes` and `timeout_secs`. `get`, `post` and
`delete` return `{ status, body, content_type, truncated }` and give up
after `timeout_secs` seconds (30 by default). The body is capped at
`max_bytes` (2 MB by default, 32 MB for `post_stream`), and `truncated`
is `true` when the cap was hit. `post_stream` bounds only connecting
and sending, so a long stream is never cut off.

Every request passes the same SSRF check as the built-in `web_fetch`
tool: the scheme must be `http` or `https` and the host must resolve to
a routable address. There is no host allowlist beyond that.

## why this is safe enough

- Closed by default. A plugin you never granted anything to is
  exactly as confined as before this tier existed.
- Per-plugin attachment. A grant to `rewind` does nothing for any
  other plugin in the same runtime.
- No shell in `exec`, workdir-scoped `cwd`.
- `net` requests are SSRF-checked and size-capped.
- Session reseats are host-applied between turns and pass through the
  `session_before_switch` veto.

## see it in use

`plugins/examples/rewind.lua` combines `session_write` and `exec`: it
git-snapshots tracked files every `turn_end`, then `/undo` drops the
last exchange (or `/rewind` forks at a chosen point) while restoring
files to that turn, and `/redo` re-applies. See
[examples](/plugins/examples#conversation-and-file-rewind).
