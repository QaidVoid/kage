# examples

The kage repo ships a handful of example plugins under
`crates/kage-plugin/examples/`. Browse them as starting points; copy
whichever fits and tweak.

## tokens-per-second readout

Reports the throughput of the most recent assistant turn as a toast.

```lua
local last_ms

kage.on("turn_start", function()
  last_ms = kage.now_ms()
end)

kage.on("turn_end", function(ctx)
  if not last_ms then return end
  local elapsed = (kage.now_ms() - last_ms) / 1000
  local tps = (ctx.usage.output_tokens or 0) / math.max(elapsed, 0.01)
  kage.notify(string.format("%.1f tok/s", tps))
end)
```

## git branch in the status bar

```lua
kage.register_widget({
  key = "git",
  render = function(_w)
    local head = kage.fs.read(".git/HEAD")
    if not head then return "" end
    local branch = head:match("ref: refs/heads/(%S+)")
    return branch and ("on " .. branch) or ""
  end,
})
```

## safer bash

Override the built-in `bash` tool to refuse destructive commands.

```lua
local blocked = { "rm %-rf /", "mkfs", ":(){" }

kage.override_tool({
  name = "bash",
  description = "bash, but checked",
  schema = { type = "object", properties = { command = { type = "string" } } },
  risk = "write",
  execute = function(input)
    local cmd = input.command or ""
    for _, pattern in ipairs(blocked) do
      if cmd:find(pattern) then
        return { is_error = true, text = "blocked: " .. pattern }
      end
    end
    -- delegate to a shell via your preferred mechanism
    return { is_error = false, text = "ok: " .. cmd }
  end,
})
```

## fixture

Test plugins in CI without spinning up the TUI: drop your plugin
under `crates/kage-plugin/tests/fixtures/` and use the
`PluginRuntime::eval` harness to drive it. See
`crates/kage-plugin/tests/integration.rs` for the pattern.
