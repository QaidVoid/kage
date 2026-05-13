# plugins

kage runs Lua plugins inside a sandboxed runtime. Drop any `.lua`
file into `~/.config/kage/plugins/` and kage loads it at startup.
File changes during a session trigger a hot reload between turns;
nothing carries over the boundary.

## what plugins can do

- register new tools the agent can call
- override built-in tools (filter `bash`, audit `write`)
- add slash commands the user invokes
- contribute status-bar widgets and transient status messages
- subscribe to loop events (turn start, message stream, tool start)
- trigger compaction or session forks
- bring up dialogs once `kage.ui.*` lands

## what plugins cannot do

- spawn subprocesses (use MCP for that, once it lands)
- read or write arbitrary filesystem paths (`kage.fs.*` is workdir-scoped)
- open network sockets outside `kage.http` (which respects an allow-list)
- load native shared libraries
- start background threads

The sandbox strips `os.execute`, `io.popen`, `package.loadlib`,
`dofile`, `loadfile`, and a handful of other escape hatches before
your code runs. Routine `string`, `math`, `table` functions stay.

## minimal example

`~/.config/kage/plugins/hello.lua`:

```lua
kage.register_command({
  name = "hello",
  description = "say hi",
  handler = function(args)
    kage.notify("hello " .. (args.rest or "there"))
  end,
})
```

Restart kage (or wait for the watcher to pick it up). Type `:hello`
or `/hello`. You should see a transient toast.

## next steps

- [Lua API](/plugins/api) - every function exposed under `kage.*`
- [Examples](/plugins/examples) - longer plugins that demonstrate
  the patterns
