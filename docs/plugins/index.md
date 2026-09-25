# plugins

kage runs Lua plugins inside a sandboxed runtime. Drop any `.lua`
file into `~/.config/kage/plugins/` (or the directory `plugins.dir`
names) and kage loads it at startup, in file-name order. In the TUI,
kage reloads every plugin when a plugin file, `init.lua` or a module
under `lua/` changes, and `/reload` does the same by hand.
Registrations, mappings, autocmds, slots and timers do not carry over
a reload. Option values and highlight overrides do.

A plugin that fails to load logs an error and the others still load.
A non-empty `[plugins] enabled` list in `config.toml` loads only the
plugins it names, by file stem.

For your own setup, use `~/.config/kage/init.lua` instead. It uses the
same API, is trusted, and loads after every plugin, so it has the last
word. See [lua config](/guide/lua-config).

## what plugins can do

- register new tools the agent can call
- override built-in tools (filter `bash`, audit `write`)
- add slash and colon commands the user invokes
- map keys to actions, commands or Lua functions
  (`kage.keymap.set`, `kage.register_keybinding`)
- open blocking dialogs (select, confirm, input, editor) with
  `kage.ui.*`, even from a command or keybinding handler
- contribute status-bar widgets and transient status messages
- fill the header, working row, input rule, footer and start card
  with built-in and Lua components (`kage.ui.set_slot`, `set_header`,
  `set_footer`)
- read and set options (`kage.opt`) and restyle highlight groups
  (`kage.api.hl_set`)
- run callbacks later or on an interval (`kage.defer`, `kage.timer`)
- add prompt-input autocomplete providers
  (`kage.add_autocomplete_provider`)
- intercept raw key events before the dispatcher
  (`kage.on_terminal_input`)
- subscribe to about 25 events (lifecycle, message stream, tool
  calls, option and theme changes) with patterns and groups, transform
  the context or provider request, rewrite or replace the compaction
  summary, and veto session ops
- trigger compaction, fork sessions, inject messages, and write
  custom session entries and labels
- keep private state across restarts (`kage.store`) and read their own
  settings from `[plugins.config.<name>]` (`kage.plugin_config`)

## what plugins cannot do by default

- spawn subprocesses
- read or write arbitrary filesystem paths (`kage.fs.*` is workdir-scoped)
- make outbound network requests (`kage.http` requires the `net`
  capability and applies SSRF filtering, not a host allow-list)
- rewrite or reseat the live session
- load native shared libraries
- `require` other files
- start background threads (timers run on kage's single Lua thread)

The sandbox strips `os.execute`, the whole `io` library, `load`,
`dofile`, `loadfile`, `require`, `package`, `debug` and a handful of
other escape hatches before your code runs. Routine `string`, `math`
and `table` functions stay.

Subprocess access, session rewriting, environment variables and
network access are available as opt-in, per-plugin
[capabilities](/plugins/capabilities) the user grants in config. They
are closed by default and loud when granted.

## trust tiers

| | plugins | `init.lua` |
| --- | --- | --- |
| Capabilities | granted per plugin in `[plugins.capabilities]` | all, without asking |
| `require` | no | from `~/.config/kage/lua/` |
| Loads | after kage's defaults, sorted by name | last |
| Runs in | the TUI, print mode and `kage rpc` | the TUI only |

Both run in the same Lua state, but each file gets its own copy of the
`kage` tables, so a plugin cannot change what another plugin or
`init.lua` sees, or reach `init.lua`'s capabilities.

## minimal example

`~/.config/kage/plugins/hello.lua`:

```lua
kage.register_command({
  name = "hello",
  description = "say hi",
  args = {
    { name = "name", kind = "text", optional = true },
  },
  handler = function(raw, ctx, args)
    kage.notify("hello " .. (args.name or "there"))
  end,
})
```

A running TUI picks the new file up on its own, or run `/reload`.
Type `:hello` or `/hello`. You should see a transient toast.

## next steps

- [Lua API](/plugins/api): every function exposed under `kage.*`
- [Lua config](/guide/lua-config): options, keymaps, autocmds,
  highlight groups and slots, from `init.lua`
- [Capabilities](/plugins/capabilities): the opt-in tier for
  subprocesses, session rewriting, environment variables and network
  access
- [Examples](/plugins/examples): longer plugins that demonstrate
  the patterns
