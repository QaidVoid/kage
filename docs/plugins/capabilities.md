# capabilities

The plugin sandbox is closed by default: no subprocesses, no
filesystem outside the workdir, no rewriting the live session. A few
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
   local files = caps.exec  -- optional extra; degrade if absent
   ```

`request_capabilities` returns a truthful `{ name = granted }` table:
a capability is `true` only if that exact plugin was granted it. The
elevated API is then attached onto that plugin's `kage` alone - another
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
| `kage.session.fork_to(at?)` | fork the current session at entry-id prefix `at` (latest if omitted) and land on the new branch. The rewind move: base `kage.session.fork` branches and stays; `fork_to` branches and goes there. |

`entries()` returns metadata only - ids, kinds, timestamps, no message
text - because it is a navigation index, not a transcript reader.
`switch` and `fork_to` only *request*; the host applies the reseat
between turns, after consulting the `session_before_switch` veto, so a
plugin can confirm or block its own rewind.

### `exec`

```lua
local r = kage.exec({ cmd = "git", args = { "stash", "create" } })
-- r = { code = 0, stdout = "...", stderr = "" }
```

Spawns a subprocess **directly - no shell**, so there is no quoting or
injection surface. The working directory is pinned under the host
workdir (the same escape check `kage.fs` uses; `cwd` may not contain
`..` or be absolute). The call blocks until the process exits and
returns its captured output, the way `kage.http.get` blocks.

## why this is safe enough

- Closed by default - a plugin you never granted anything to is exactly
  as confined as before this tier existed.
- Per-plugin attachment - a grant to `rewind` does nothing for any
  other plugin in the same runtime.
- No shell in `exec`, workdir-scoped `cwd`.
- Session reseats are host-applied between turns and pass through the
  `session_before_switch` veto.

## see it in use

`plugins/examples/rewind.lua` combines both capabilities: it
git-snapshots tracked files every `turn_end`, and `/rewind` forks the
conversation at a chosen point while restoring files to that turn. See
[examples](/plugins/examples#conversation-and-file-rewind).
