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
surface plus `kage.register_keybinding`, which maps a chord in mode
`g` and returns an `off` function. Each command opens a modal and the
coroutine suspends until the user answers:

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

## throughput in the footer

The same readout as a footer component instead of a toast. The
component recomputes only when `message_end` fires, so it costs
nothing between turns. Autocmds fire in creation order, so the `kage.on`
handler updates `last` before the component reads it:

```lua
local start_ms, last = nil, ""

kage.api.hl_set("TpsReadout", { link = "KageMuted" })

kage.on("agent_start", function()
  start_ms = kage.now_ms()
end)

kage.on("message_end", function(ev)
  if not start_ms then return end
  local elapsed = (kage.now_ms() - start_ms) / 1000
  local out = (ev.usage and ev.usage.output) or 0
  last = string.format("%.1f tok/s", out / math.max(elapsed, 0.001))
end)

kage.ui.set_slot("footer", {
  left = { "hint" },
  right = {
    "model", "permission", "context", "tokens",
    { events = { "message_end" }, hl = "TpsReadout", render = function() return last end },
  },
  sep = " \u{B7} ",
})
```

The spec repeats kage's default footer and adds the readout at the
end. `TpsReadout` is not a `Kage*` group, so it survives theme
switches. A user who prefers another footer can replace it from
`init.lua`, which loads after every plugin.

## keys and autocmds

A plugin that shows the running shell command in the header's
widget area and adds a key to copy the last one into a toast:

```lua
local group = kage.api.augroup_create("bash-watch")
local last

kage.api.autocmd_create("tool_call", {
  group = group,
  pattern = "bash",
  callback = function(ev)
    last = tostring(ev.data.input.command)
    kage.set_status("bash", "$ " .. last)
  end,
})

kage.api.autocmd_create("agent_end", {
  group = group,
  callback = function()
    kage.clear_status("bash")
  end,
})

kage.keymap.set("g", "<F6>", function()
  kage.ui.notify(last or "no shell command yet")
end, { desc = "show the last shell command", group = "bash watch" })
```

`pattern = "bash"` limits the first autocmd to the `bash` tool. The
mapping has a `desc`, so it appears in the `?` reference under
`bash watch`.

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

## conversation and file rewind

`plugins/examples/rewind.lua` is the worked example for the
[capability tier](/plugins/capabilities). Grant it both capabilities:

```toml
[plugins.capabilities]
rewind = ["session_write", "exec"]
```

It snapshots tracked files with `git stash create` on every
`turn_end`, keyed by the session's last entry id:

```lua
kage.on("turn_end", function()
  if not in_git_repo() then return end
  local id = last_entry_id()
  checkpoints[#checkpoints + 1] = { id = id, sha = snapshot() }
end)
```

`/undo` is the one-step ergonomic case: it drops the last exchange by
forking back to the entry just before your most recent prompt and
restoring files there. Repeat it to walk further back, one exchange
per call:

```lua
local function undo_target()        -- entry before the last user msg
  -- ... scan kage.session.entries() backwards for the last user role
end
local at = undo_target()
restore(checkpoint_for(at).sha)     -- kage.exec git checkout
kage.session.fork_to(at)            -- host reseats next turn
```

`/rewind` is the same move with a picker: it lists the user turns from
`kage.session.entries()` and forks at whichever you choose. `/redo`
(alias `/rewind-redo`) re-applies the file changes the last `/undo` or
`/rewind` undid.

It degrades honestly: without `session_write` the plugin disables
itself; without `exec` it still rewinds the conversation but skips
file restore. The conversation fork is one-way - `/redo` restores
files, not the un-forked conversation; that is the nature of branching
an append-only session.

## testing without the TUI

Drop a plugin under `crates/kage-plugin/tests/fixtures/` and drive it
with the `PluginRuntime` harness. Bridged handlers (commands,
keybindings) can be stepped with `bridge_call` / `bridge_resume` /
`bridge_cancel`; see `crates/kage-plugin/tests/examples.rs` for the
pattern, including how the dialog round-trips are asserted.
