# lua config

`init.lua` is kage's Lua configuration file. It sets options, maps
keys, reacts to events, restyles highlight groups and rearranges the
header, footer, input pill and start screen. `config.toml` keeps
working next to it. Its UI keys and `[keybindings]` table feed the
same options and keymap table, and `init.lua` runs after it, so
`init.lua` wins.

A complete, working example is at the [end of this page](#complete-example).

## init.lua and lua/ modules

kage reads `$XDG_CONFIG_HOME/kage/init.lua`, which is
`~/.config/kage/init.lua` by default. The file is optional.

- Only the TUI loads it. Print mode (`kage -p`), `kage rpc` and
  `kage mcp serve` read `config.toml` only.
- A project's `.kage/init.lua` is never loaded. Project settings go in
  `.kage/config.toml` (see [configuration](/guide/config)).
- An error in `init.lua` is logged as an error block in the
  conversation. kage still starts, with its defaults and your plugins
  loaded.

`require` loads modules from the `lua/` directory next to `init.lua`.
A name is dot-separated segments of letters, digits, `_` and `-`, and
`a.b` tries `lua/a/b.lua`, then `lua/a/b/init.lua`:

```lua
-- ~/.config/kage/lua/me/greet.lua
local M = {}

function M.hello(name)
  kage.notify("hello " .. name)
end

return M
```

```lua
-- ~/.config/kage/init.lua
require("me.greet").hello("world")
```

A module runs once per load and its return value is cached until the
next reload. A module that requires itself, directly or through
others, raises `loop requiring <name>`. Paths that leave `lua/`,
through `..`, an absolute name or a symlink, are rejected.

### trust

`init.lua` is trusted. Plugins are not.

| | `init.lua` and `lua/` | plugins |
| --- | --- | --- |
| Location | `~/.config/kage/init.lua` | `~/.config/kage/plugins/*.lua` |
| Capabilities | all of `exec`, `env`, `net` and `session_write`, without asking | only what `[plugins.capabilities]` grants and the plugin requests |
| `require` | confined to `lua/` | not available |
| Raw `io`, `os.execute`, `debug` | removed | removed |
| Loaded by | the TUI only | the TUI, print mode and `kage rpc` |

`kage.request_capabilities` still works in `init.lua` and reports every
capability as granted. Each plugin and `init.lua` run in their own
environment, so a plugin cannot reach the trusted functions or change
what another environment sees.

## load order and reload

Every load runs the same steps, and each step can override the ones
before it. The last set wins.

1. `_defaults.lua`, embedded in kage: the default keymaps and the
   header, footer and input pill slots.
2. Plugins, sorted by file name.
3. `[keybindings] bindings` from `config.toml`.
4. `init.lua`.
5. The `color_scheme` event fires once for the current theme, so
   highlight overrides written as `color_scheme` autocmds apply.

Options are seeded from `config.toml` and `KAGE_*` environment
variables before step 1.

kage watches the plugins directory, `init.lua` and `lua/` (recursively)
and reloads when a Lua file changes. A reload clears keymaps,
autocmds, slots, timers and everything plugins registered, then runs
the steps above again. It does not revert options or highlight
overrides: if you delete `opt.theme = "tokyo-night"` from `init.lua`,
the theme stays until you restart kage. A `lua/` directory created
while kage runs is watched from the next start.

## options

`kage.opt.<name>` reads an option. Assigning to it sets the option:

```lua
kage.opt.theme = "tokyo-night"
kage.opt.input_max_lines = 12
kage.notify("editor is " .. kage.opt.editor)
```

An unknown name or an invalid value raises an error that lists what is
valid. `kage.api.option_get(name)` returns the value and where it came
from (`default`, `toml`, `lua` or `runtime`). `kage.api.option_set(name,
value)` is the same as assigning.

| Option | TOML key | Type | Default | Applies |
| --- | --- | --- | --- | --- |
| `theme` | `ui.theme` | a bundled or user theme name | `"default"` | immediately |
| `mouse` | `ui.mouse` | boolean | `true` | immediately |
| `editor` | `ui.editor` | `"vim"` or `"modeless"` | `"modeless"` | immediately |
| `input_min_lines` | `ui.input_min_lines` | integer, 1 to 64 | `1` | immediately |
| `input_max_lines` | `ui.input_max_lines` | integer, 1 to 64 | `8` | immediately |
| `thinking_level` | `ui.thinking_level` | `""`, `"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"` or `"xhigh"` | `""` | next session |
| `compaction_threshold` | `loop.compaction_threshold` | number, 0 to 1 (0 turns compaction off) | `0.8` | next session |
| `leader` | `keybindings.leader` | one key, such as `","` or `"<Space>"` | `"\\"` (backslash) | mappings set after it |
| `timeoutlen` | `keybindings.timeoutlen` | integer milliseconds, 0 to 5000 | `1000` | immediately |

`thinking_level` and `compaction_threshold` set in `init.lua` apply to
the first session, because kage starts it after `init.lua` has run. An
empty `thinking_level` means the model's default level.

Every set fires the [`option_set`](#configuration-events) event. `:theme set`,
`:mouse` and the `:settings` dialog set options too, with source
`runtime`. The settings dialog marks options last set from Lua, since
`init.lua` shadows a saved TOML value on the next start.

## keymaps

```lua
kage.keymap.set(mode, lhs, rhs, opts)
kage.keymap.del(mode, lhs)
```

`mode` is one letter or a list of letters, and each mode gets its own
entry:

| Mode | Where it applies |
| --- | --- |
| `n` | vim normal mode, in either pane |
| `b` | vim normal mode with the conversation pane focused |
| `i` | vim insert mode and the modeless editor |
| `v` | visual mode |
| `g` | every editing state above |

A key is looked up in these modes, first match wins:

| Editing state | Modes searched |
| --- | --- |
| vim normal, conversation pane | `b`, `n`, `g` |
| vim normal, input pane | `n`, `g` |
| vim insert, or modeless | `i`, `g` |
| visual | `v`, `g` |

Mappings never apply while an overlay (a picker, a dialog, the `:`
line, the `/` search line, the slash palette) is open. While the
autocomplete popup is open it sees its own keys first.

### notation

`lhs` uses Vim notation:

| Form | Examples |
| --- | --- |
| characters, case-sensitive | `gg`, `zM`, `Y`, `?` |
| Ctrl, Alt, Shift, Super | `<C-l>`, `<M-p>`, `<S-Tab>`, `<D-k>`, `<C-S-x>` |
| named keys | `<CR>`, `<Esc>`, `<Tab>`, `<BS>`, `<Del>`, `<Space>`, `<Up>`, `<Down>`, `<Left>`, `<Right>`, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<Insert>`, `<F1>` to `<F12>` |
| special characters | `<lt>`, `<gt>`, `<Bslash>`, `<Bar>` |
| the leader | `<leader>m` |

`<C-L>` is the same as `<C-l>`. Add `S-` for Ctrl with Shift. The chord
form from `config.toml` also works for a single key: `ctrl+shift+x`,
`alt+p`, `f5`.

`<leader>` expands to the `leader` option when the mapping is set, as
in Neovim. Set `kage.opt.leader` before your leader mappings. The
default leader is a backslash. A leader that is also a printable key,
such as `<Space>`, is best used in modes `n` and `b` only, since a
mapping in `i` or `g` would catch it while you type.

### rhs

| `rhs` | Effect |
| --- | --- |
| `kage.action.Name` | runs a built-in action |
| `kage.action.scroll(n)` | scrolls the conversation `n` lines (negative scrolls up) |
| `":command args"` | runs a command line, like typing it after `:` |
| a function | runs Lua. It may open `kage.ui.*` dialogs, and a non-empty string return is shown as a conversation block. |
| `"<Nop>"` | swallows the key |

`kage.action` holds: `Cancel`, `BeginCommand`, `BeginSearch`,
`ScrollToTop`, `ScrollToBottom`, `ToggleFold`, `UnfoldAll`, `FoldAll`,
`Yank`, `ClearSelection`, `OpenModelPicker`, `OpenSessionPicker`,
`OpenCommandPalette`, `SearchNext`, `SearchPrev`, `YankFocusedBlock`,
`CycleThinkingLevel`, `CyclePane`, `FocusPrev`, `FocusNext`,
`OpenHelp`, `OpenJumpPicker`, `AttachClipboardImage`, `EnterVisual`,
and the function `scroll(n)`.

`opts` takes `desc` and `group`. The `?` reference lists every mapping
that has a `desc`, under its `group` (`other` when unset). Mappings
without a `desc` still work but are hidden there.

```lua
local map, act = kage.keymap.set, kage.action

kage.opt.leader = ","
map("n", "<leader>m", act.OpenModelPicker, { desc = "pick a model", group = "mine" })
map("g", "<C-t>", ":theme set catppuccin-mocha", { desc = "warm theme", group = "mine" })
map("b", "<C-d>", act.scroll(20), { desc = "scroll a page", group = "mine" })
map({ "i", "n" }, "<F5>", function()
  local ok = kage.ui.confirm("compact", "Compact the conversation now?")
  if ok then
    kage.compact()
  end
end, { desc = "compact", group = "mine" })
```

### sequences and timeoutlen

A mapping can be several keys long. While the keys typed so far are
the start of a longer mapping, kage waits for more, and the input
pill shows the pending keys. When the keys stop matching, the longest
mapping they complete fires and the rest are looked up again. If none
fires, the keys go to the editor in order, so `gg` still moves to the
start of the prompt in the input pane. A mapping that is also the
start of a longer one fires after `timeoutlen` milliseconds with no
further key.

### defaults and deleting

kage's own keys are mappings from `_defaults.lua`, owned by
`defaults`, so you can replace or delete any of them:

```lua
kage.keymap.set("g", "<C-p>", "<Nop>")  -- shadow the model picker key
kage.keymap.del("g", "<C-s>")           -- remove the session picker key
```

`kage.keymap.del` raises when the mapping does not exist. See
[keybindings](/guide/keybindings) for the default table.

The editor grammar stays in Rust and is not in the table: vim motions,
operators, counts, registers, `r`, undo and redo, readline edits and
the kill ring, Enter, Shift+Enter and Alt+Enter, history Up and Down,
Esc, insert-mode Ctrl+O, Ctrl+G (external editor), the modeless `/`,
`!` and `?` empty-prompt prefixes, and `i` and `a` in the conversation
pane. A mapping or `"<Nop>"` on one of these keys shadows it.
`kage.keymap.del` cannot remove it.

Ctrl+Q (quit) and Ctrl+C (cancel the turn) work above every layer,
including overlays. They yield only to a mapping owned by `init.lua` or
`config.toml`. A plugin mapping on them never fires and logs a
warning.

`:keybindings` (alias `:keys`) lists the whole table per mode with the
owner of each mapping: `defaults`, a plugin name, `config.toml` or
`init.lua`.

## autocmds

`kage.api.autocmd_create(event, opts)` runs `opts.callback` when
`event` fires and returns an id.

```lua
local api = kage.api
local g = api.augroup_create("me")

api.autocmd_create("tool_call", {
  group = g,
  pattern = "bash",
  desc = "log shell calls",
  callback = function(ev)
    kage.log("info", "bash: " .. tostring(ev.data.input.command))
  end,
})
```

| Field | Meaning |
| --- | --- |
| `callback` | required. Receives `ev = { id, event, match, group, data }`, where `data` is the event payload. |
| `group` | a group id or name from `augroup_create` |
| `pattern` | a string or a list of strings compared exactly against the event's match value. `"*"`, the default, matches everything. |
| `once` | delete the autocmd before its first call |
| `desc` | shown in error messages |

The events and payloads are listed in the [Lua API](/plugins/api#events).
These events have a match value, and every other event accepts only
`"*"`:

| Event | Match value |
| --- | --- |
| `tool_call`, `tool_result` | the tool name |
| `model_select`, `thinking_level_select` | the new value |
| `option_set` | the option name |
| `color_scheme` | the theme name |
| `user` | the `pattern` given to `autocmd_exec` |

### configuration events

Three events come from the configuration layer:

- `option_set` fires on every option set, with
  `{ name, old, new, source }`. `source` is `"lua"` or `"runtime"`.
- `color_scheme` fires with `{ name }` after the theme's base groups
  change, and once at the end of every load.
- `user` fires only through `kage.api.autocmd_exec("user", { pattern,
  data })`. `data` arrives as `ev.data`.

Other functions:

- `kage.api.autocmd_del(id)` deletes one autocmd. A missing id is
  ignored.
- `kage.api.augroup_create(name, { clear = true })` returns the group
  id. With `clear = true`, the default, an existing group loses its
  autocmds first, so running the same setup twice does not register
  twice.
- `kage.api.augroup_del(group)` deletes a group and its autocmds.
- `kage.api.autocmd_exec(event, { pattern, data })` fires any event now.
  Nesting deeper than 16 levels raises.

`kage.on(event, fn)` is the short form: `fn` receives the payload
alone, and the call returns an `off` function. Unknown event names
raise in `autocmd_create` but only warn once in `kage.on`.

## highlight groups

Every color kage paints comes from a highlight group. Themes provide
the base groups, and Lua can override them or add its own:

```lua
kage.api.hl_set("KageUserBubble", { bg = "#1b1e2b" })
kage.api.hl_set("MyCost", { link = "KageMuted" })
local spec = kage.api.hl_get("KageToolError")
```

A spec has `fg`, `bg`, `bold`, `italic`, `underline`, `dim`, `reverse`
and `link`. Colors are `#rrggbb`, one of the 16 names (`black`, `red`,
`green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `darkgray`,
`lightred`, `lightgreen`, `lightyellow`, `lightblue`, `lightmagenta`,
`lightcyan`, `white`) or a palette index `"0"` to `"255"`. `link`
follows another group and wins over every other field. A link cycle
resolves to an empty style. An invalid color or an unknown field
raises.

- `hl_set` replaces the whole group. To change one color, read the
  group with `hl_get`, change the field and set it back.
- `hl_get(name)` returns the group as set, or `nil` when it does not
  exist. `hl_get(name, { link = false })` follows links and returns the
  effective spec.
- Switching theme replaces the base groups and drops your overrides of
  `Kage*` groups. Groups with other names persist. Set `Kage*`
  overrides from a `color_scheme` autocmd so they survive a switch:

```lua
kage.api.autocmd_create("color_scheme", {
  callback = function()
    kage.api.hl_set("KageUserBubble", { bg = "#1b1e2b" })
  end,
})
```

The built-in renderer reads only the colors of `Kage*` groups.
Attributes such as `bold` apply where a group is used by name: span
`hl` fields and slot component `hl`.

Spans in slots, `set_header`, `set_footer` and block renderers take
`hl = "Group"`. Their `fg` and `bg` accept a group name (its `fg` or
`bg`), a theme role name such as `muted_fg`, or a color. Styles
resolve when the row is painted, so they follow theme switches.

### group list

Each theme role (see [themes](/guide/themes#color-roles)) lives in one
group, as its foreground or its background.

| Group | fg role | bg role |
| --- | --- | --- |
| `KageNormal` | `assistant_fg` | `bg` |
| `KageUserBubble` | | `user_bg` |
| `KageUserRule` | `user_rule` | |
| `KageAssistantRule` | `assistant_rule` | |
| `KageThinking` | `thinking_fg` | |
| `KageTool` | `tool_result_fg` | `tool_bg` |
| `KageToolError` | `tool_error_fg` | `tool_error_bg` |
| `KageToolPending` | | `tool_pending_bg` |
| `KageToolRule` | `tool_rule` | |
| `KageToolErrorRule` | `tool_error_rule` | |
| `KageToolPendingRule` | `tool_pending_rule` | |
| `KageCustom` | `custom_fg` | |
| `KageStatus` | `status_dim_fg` | `status_bg` |
| `KageMuted` | `muted_fg` | |
| `KageMatch` | | `match_color` |
| `KageSelection` | `selection_fg` | `selection_color` |
| `KageFocus` | | `focus_color` |
| `KageInputBorderNormal` | `input_border_normal` | |
| `KageInputBorderInsert` | `input_border_insert` | |
| `KageInputBorderVisual` | `input_border_visual` | |
| `KageInputPillNormal` | `input_pill_normal_fg` | `input_pill_normal_bg` |
| `KageInputPillInsert` | `input_pill_insert_fg` | `input_pill_insert_bg` |
| `KageInputPillVisual` | `input_pill_visual_fg` | `input_pill_visual_bg` |
| `KageInputGlyph` | `input_glyph_fg` | |
| `KageInputPlaceholder` | `input_placeholder_fg` | |
| `KageInputHint` | `input_hint_fg` | |
| `KageModeline` | `modeline_fg` | `modeline_bg` |
| `KageOverlay` | `overlay_fg` | |
| `KageOverlayBorder` | `overlay_border` | |
| `KageOverlaySelected` | `overlay_selected_fg` | `overlay_selected_bg` |
| `KageWarning` | `warning_fg` | |
| `KageSuccess` | `success_fg` | |
| `KageMarkdownH1` | `md_h1_fg` | |
| `KageMarkdownH2` | `md_h2_fg` | |
| `KageMarkdownLink` | `md_link_fg` | |
| `KageMarkdownCode` | `md_code_fg` | |

A theme file can also set groups in its `[groups]` table (see
[themes](/guide/themes#groups)).

## slots

Slots are the fixed chrome regions:

| Slot | Region | Spec |
| --- | --- | --- |
| `header` | the top row | `{ left, right, sep }` |
| `footer` | the modeline row | `{ left, right, sep }` |
| `input_pill` | the input card's top border | `{ left, right, sep }` |
| `start` | the conversation area while it is empty | `{ lines }` |

`kage.ui.set_slot(name, spec)` replaces a slot's spec, and
`kage.ui.set_slot(name, nil)` restores the one `_defaults.lua` set.
`left` items paint from the left edge and `right` items against the
right edge. `sep` goes between two items that both have output. In
`start`, each item is one line, centered. The `:` command line and the
`/` search line still paint over the header. When `start` has a spec,
kage skips its welcome notice.

kage's defaults are:

```lua
kage.ui.set_slot("header", { left = { "brand", "model" }, right = { "widgets", "search", "session" } })
kage.ui.set_slot("footer", {
  left = { "working", "model", "context", "tokens", "thinking", "permission" },
  sep = " . ",
})
kage.ui.set_slot("input_pill", { left = { "mode" }, right = { "hint" } })
```

An item is one of:

- a built-in component name, painted by kage every frame
- a span, `{ text, hl?, fg?, bg?, bold?, dim?, italic?, underline? }`
- a Lua component, `{ render, events?, interval?, hl? }`

### built-in components

| Component | Shows |
| --- | --- |
| `brand` | `kage` |
| `model` | the active model |
| `widgets` | plugin widgets and `kage.set_status` entries |
| `search` | the search match count while a search is active |
| `session` | the session id |
| `working` | a spinner while a turn runs |
| `context` | context use against the window, such as `ctx 12k/200k (6%)` |
| `tokens` | input and output tokens, plus the cost when known |
| `thinking` | the thinking level, hidden when off |
| `permission` | a session permission override, hidden when there is none |
| `mode` | the editor mode glyph |
| `hint` | keys of a pending mapping sequence |
| `cwd` | the working directory |
| `version` | the kage version |

An unknown component name raises.

### Lua components

```lua
{
  render = function(ctx) return "text" end,
  events = { "message_end", "user Tick" },
  interval = 5000,
  hl = "MyGroup",
}
```

`render(ctx)` returns a string, a span, or a list of those (one line
each). A row slot paints the first line only. `nil` or an empty string
paints nothing, and the item takes no separator. A render that raises
is logged and the previous output stays.

kage keeps the output and calls `render` again only when:

- an event in `events` fires. An entry is an event name, optionally
  followed by a space and a pattern: `"tool_result bash"`,
  `"user Tick"`.
- `interval` milliseconds pass (at least 50).
- `kage.api.redraw(slot)` is called. With no argument it redraws every
  slot.
- the slot is set.
- the terminal width changes.

Rendering never waits on Lua, and a render that runs too long is
aborted.

`ctx` holds:

| Field | Value |
| --- | --- |
| `width` | terminal width in columns |
| `model` | the active `provider:model` id |
| `thinking` | the active thinking level |
| `permission_mode` | the session permission override, or `nil` |
| `working` | whether a turn is running |
| `usage` | `{ total = { input, output, cache_read, cache_write }, context_used, context_window, cost }`, or `nil` before the first report |
| `session` | `{ id, title }` |
| `cwd` | the working directory |
| `mode` | `"normal"`, `"insert"` or `"visual"` |

`kage.ui.set_header(fn)` and `kage.ui.set_footer(fn)` are shorthands
that replace the whole row with one component calling `fn(width)`
every 500 ms. `nil` restores the default row.

## timers

| Function | Runs `fn` |
| --- | --- |
| `kage.schedule(fn)` | once, right after the current Lua call returns |
| `kage.defer(fn, ms)` | once, `ms` milliseconds from now. Returns `stop`, which cancels it. |
| `kage.timer(fn, ms)` | every `ms` milliseconds (at least 50) until the returned `stop` is called |

Callbacks run on the Lua thread between other Lua work. A callback that
raises is logged, and a timer that raises stops. Callbacks cannot open
`kage.ui.*` dialogs. A reload cancels every timer.

```lua
local stop = kage.timer(function()
  kage.api.autocmd_exec("user", { pattern = "Tick" })
end, 60000)
```

## complete example

This `init.lua` uses every part of this page. It switches to vim mode
with a space leader, adds a few mappings, logs shell calls, keeps a
user bubble color across theme switches, adds a cost and a clock to the
footer and fills the start screen.

```lua
-- ~/.config/kage/init.lua
local opt, map, act = kage.opt, kage.keymap.set, kage.action
local api = kage.api

-- Options. These win over config.toml.
opt.theme = "tokyo-night"
opt.editor = "vim"
opt.input_max_lines = 12
opt.leader = " " -- set the leader before any <leader> mapping
opt.timeoutlen = 600

-- Keymaps.
map("n", "<leader>m", act.OpenModelPicker, { desc = "pick a model", group = "mine" })
map("n", "<leader>t", ":theme set catppuccin-mocha", { desc = "warm theme", group = "mine" })
map("n", "<leader>y", act.YankFocusedBlock, { desc = "yank the focused block", group = "mine" })
map("b", "<C-d>", act.scroll(20), { desc = "scroll down a page", group = "mine" })
map("i", "<C-l>", function()
  kage.notify("hello from init.lua")
end, { desc = "say hello", group = "mine" })
kage.keymap.del("g", "<C-s>") -- give Ctrl+S back

-- Autocmds. The group makes a reload replace them instead of adding more.
local g = api.augroup_create("me")

api.autocmd_create("tool_call", {
  group = g,
  pattern = "bash",
  desc = "log shell calls",
  callback = function(ev)
    kage.log("info", "bash: " .. tostring(ev.data.input.command))
  end,
})

api.autocmd_create("option_set", {
  group = g,
  pattern = "editor",
  callback = function(ev)
    kage.notify("editor: " .. ev.data.old .. " -> " .. ev.data.new)
  end,
})

-- Highlights. Kage* overrides reset on a theme switch, so set them
-- from color_scheme. Other groups persist.
api.autocmd_create("color_scheme", {
  group = g,
  callback = function()
    api.hl_set("KageUserBubble", { bg = "#1b1e2b" })
    -- hl_set replaces the whole group, so keep the theme's bg.
    local err = api.hl_get("KageToolError") or {}
    err.fg = "#ff6b6b"
    api.hl_set("KageToolError", err)
  end,
})
api.hl_set("MyCost", { link = "KageMuted" })
api.hl_set("MyClock", { fg = "cyan", bold = true })

-- Slots.
kage.ui.set_slot("footer", {
  left = { "working", "model", "context", "tokens", "thinking", "permission" },
  right = {
    {
      events = { "message_end", "model_select" },
      hl = "MyCost",
      render = function(ctx)
        local u = ctx.usage
        if not u then
          return ""
        end
        return string.format("$%.3f", u.cost or 0)
      end,
    },
    {
      events = { "user Tick" },
      render = function()
        return { text = os.date("%H:%M") .. " ", hl = "MyClock" }
      end,
    },
  },
  sep = " . ",
})

kage.ui.set_slot("start", {
  lines = {
    { text = "kage", hl = "KageMarkdownH1", bold = true },
    "version",
    "cwd",
    { text = "press <Space>m for the model picker", hl = "KageMuted" },
  },
})

-- Timers.
kage.timer(function()
  api.autocmd_exec("user", { pattern = "Tick" })
end, 30000)

kage.defer(function()
  kage.notify("init.lua loaded")
end, 200)
```

The options above, written in `config.toml` instead:

```toml
[ui]
theme = "tokyo-night"
editor = "vim"
input_max_lines = 12

[keybindings]
leader = " "
timeoutlen = 600
bindings = { "ctrl+t" = "theme set catppuccin-mocha", "<F2>" = "action:OpenModelPicker" }
```

`[keybindings] bindings` always maps in mode `g`, so keep mappings that
start with a printable leader in `init.lua`, where you can pick mode
`n`.
