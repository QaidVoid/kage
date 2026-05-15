# examples

The kage repo ships example plugins under `plugins/examples/`. They
double as integration-test fixtures, so they stay in sync with the
runtime. Copy whichever fits and tweak.

## tokens-per-second readout

`plugins/examples/tps.lua` reports the throughput of the most recent
assistant turn as a toast. It tracks elapsed time across the turn and
reads token counts off the `message_end` payload:

```lua
local start_ms

kage.on("agent_start", function()
  start_ms = kage.now_ms()
end)

kage.on("message_end", function(ev)
  if not start_ms then return end
  local elapsed = (kage.now_ms() - start_ms) / 1000
  local out = (ev.usage and ev.usage.output) or 0
  kage.ui.notify(string.format("%d tokens, %.1f tok/s", out,
    out / math.max(elapsed, 0.001)))
end)
```

## git branch in the status bar

`plugins/examples/git-status.lua` reads `.git/HEAD` directly (the
sandbox forbids spawning `git`) and announces the branch:

```lua
kage.on("agent_start", function()
  local ok, head = pcall(kage.fs.read, ".git/HEAD")
  if not ok or not head then
    kage.notify("git: not a repo")
    return
  end
  local branch = head:gsub("%s+$", ""):match("^ref: refs/heads/(.+)$")
  kage.notify("git: " .. (branch or "detached"))
end)
```

## blocking dialogs and a keybinding

`plugins/examples/select_demo.lua` exercises the whole `kage.ui.*`
surface plus `kage.register_keybinding`. Each command opens a modal
and the coroutine suspends until the user answers:

```lua
kage.register_command({
  name = "pick-color",
  description = "Pick a color via the ui.select dialog",
  handler = function()
    local color = kage.ui.select("Pick a color", { "red", "green", "blue" })
    if color == nil then return "cancelled" end
    kage.ui.notify("pick-color: " .. color)
    return "you picked " .. color
  end,
})

kage.register_keybinding({ key = "ctrl+alt+k", description = "Quick pick" },
  function()
    local color = kage.ui.select("Quick pick", { "red", "green", "blue" })
    return color or "cancelled"
  end)
```

It also registers `/confirm-delete` (`kage.ui.confirm`), `/ask-name`
(`kage.ui.input`), and `/compose-note` (`kage.ui.editor`).

## safer bash

Override the built-in `bash` tool to refuse destructive commands:

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
    return { is_error = false, text = "ok: " .. cmd }
  end,
})
```

## testing without the TUI

Drop a plugin under `crates/kage-plugin/tests/fixtures/` and drive it
with the `PluginRuntime` harness. Bridged handlers (commands,
keybindings) can be stepped with `bridge_call` / `bridge_resume` /
`bridge_cancel`; see `crates/kage-plugin/tests/examples.rs` for the
pattern, including how the dialog round-trips are asserted.
