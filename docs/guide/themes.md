# themes

kage ships three bundled palettes and loads any additional ones you
drop into `~/.config/kage/themes/`.

Bundled names: `default`, `tokyo-night`, `catppuccin-mocha`.

## switching themes

```text
:theme list             list bundled + user themes (* marks active)
:theme set tokyo-night  switch immediately for this session
:theme current          show the active theme
```

`:theme set` applies at once. It does **not** persist on its own. Set
the default in `config.toml`

```toml
[ui]
theme = "my-theme"
```

or use the `/settings` dialog, which writes only that key back to
`config.toml`.
The configured theme is resolved at startup. An unknown name leaves
the default palette in place and reports the error inline rather than
failing silently.

`init.lua` sets the theme with `kage.opt.theme = "name"`, which wins
over `config.toml`. Plugins and `init.lua` can also call
`kage.theme.set("name")`. It switches before it returns and raises on
an unknown name. Every switch fires the `color_scheme` event (see
[lua config](/guide/lua-config#highlight-groups)).

## writing a custom theme

A user theme is a TOML file at `~/.config/kage/themes/<name>.toml`
(honoring `XDG_CONFIG_HOME`). The file name is the theme name. There
is no `name` key. Four top-level keys are accepted, and an unknown key
is an error:

```toml
# Palette to start from. One of the bundled names. Defaults to
# "default", so you only list what you want to change.
base = "tokyo-night"

# See "transparency" below. Defaults to false (fully opaque).
transparent = false

[colors]
bg = "#11131a"
assistant_fg = "#e5e5e5"
focus_color = "#f4a72b"
match_color = "#fbbf24"

[groups]
KageToolError = { fg = "#ff6b6b", bg = "#2a1618" }
KageMarkdownLink = { link = "KageWarning" }
MyAccent = { fg = "#f4a72b", bold = true }
```

Every key under `[colors]` is a role name (full list below). A typo
in a role name or an unparseable color aborts the load with a message
naming the offender. The theme is not partially applied.

### color syntax

`[colors]` values use ratatui's grammar:

- 24-bit hex: `#rrggbb` (e.g. `#1f2430`)
- a named color: `black`, `red`, `green`, `yellow`, `blue`,
  `magenta`, `cyan`, `gray`, `darkgray`, `lightred`, `lightgreen`,
  `lightyellow`, `lightblue`, `lightmagenta`, `lightcyan`, `white`
- a 256-color index as a string: `"244"`
- `reset`, the terminal's default color

`[groups]` colors take the same forms except `reset`. Leave the field
out to get the terminal default.

Terminals without truecolor downsample `#rrggbb` to their nearest
indexed color.

### color roles

`[colors]` accepts these keys. Anything you omit keeps the value from
`base`. Each role is one color of a highlight group, shown in the
second column.

| Role | Group | Colors |
| --- | --- | --- |
| `bg` | `KageNormal` bg | the whole-frame canvas behind every block |
| `assistant_fg` | `KageNormal` fg | assistant text |
| `user_bg` | `KageUserBubble` bg | prompt bubble fill |
| `user_rule` | `KageUserRule` fg | prompt bubble left rule |
| `assistant_rule` | `KageAssistantRule` fg | idle left spine on assistant turns |
| `thinking_fg` | `KageThinking` fg | thinking text |
| `custom_fg` | `KageCustom` fg | custom block text |
| `tool_bg` | `KageTool` bg | tool block fill |
| `tool_result_fg` | `KageTool` fg | tool result text |
| `tool_error_bg` | `KageToolError` bg | failed tool block fill |
| `tool_error_fg` | `KageToolError` fg | failed tool result text |
| `tool_pending_bg` | `KageToolPending` bg | running tool block fill |
| `tool_rule` | `KageToolRule` fg | tool block left rule |
| `tool_error_rule` | `KageToolErrorRule` fg | failed tool block left rule |
| `tool_pending_rule` | `KageToolPendingRule` fg | running tool block left rule |
| `muted_fg` | `KageMuted` fg | secondary hints and metadata |
| `match_color` | `KageMatch` bg | search match |
| `selection_color` | `KageSelection` bg | visual selection |
| `selection_fg` | `KageSelection` fg | selected text |
| `focus_color` | `KageFocus` bg | focused block accent |
| `status_bg` | `KageStatus` bg | header row fill |
| `status_dim_fg` | `KageStatus` fg | header row text |
| `modeline_bg` | `KageModeline` bg | command palette, completion popup and toast fill |
| `modeline_fg` | `KageModeline` fg | not painted by the built-in chrome |
| `input_border_normal` | `KageInputBorderNormal` fg | input rules, normal mode |
| `input_border_insert` | `KageInputBorderInsert` fg | input rules, insert mode and modeless |
| `input_border_visual` | `KageInputBorderVisual` fg | input rules, visual mode |
| `input_pill_normal_bg`, `input_pill_normal_fg` | `KageInputPillNormal` | emphasized text on the input rule, normal mode (fg only) |
| `input_pill_insert_bg`, `input_pill_insert_fg` | `KageInputPillInsert` | emphasized text on the input rule, insert mode (fg only) |
| `input_pill_visual_bg`, `input_pill_visual_fg` | `KageInputPillVisual` | emphasized text on the input rule, visual mode (fg only) |
| `input_glyph_fg` | `KageInputGlyph` fg | prompt glyph |
| `input_placeholder_fg` | `KageInputPlaceholder` fg | placeholder text |
| `input_hint_fg` | `KageInputHint` fg | footer hint and start card change hints |
| `overlay_fg` | `KageOverlay` fg | picker and dialog text |
| `overlay_border` | `KageOverlayBorder` fg | picker and dialog border |
| `overlay_selected_bg`, `overlay_selected_fg` | `KageOverlaySelected` | selected picker row |
| `warning_fg` | `KageWarning` fg | warnings |
| `success_fg` | `KageSuccess` fg | success marks |
| `md_h1_fg` | `KageMarkdownH1` fg | markdown level 1 headings |
| `md_h2_fg` | `KageMarkdownH2` fg | markdown level 2 headings |
| `md_link_fg` | `KageMarkdownLink` fg | markdown links |
| `md_code_fg` | `KageMarkdownCode` fg | markdown inline code |

### groups

`[groups]` sets whole highlight groups. It is applied after `[colors]`,
and each entry replaces its group, so list both colors of a group that
has two. An entry takes `fg`, `bg`, `bold`, `italic`, `underline`,
`dim`, `reverse` and `link`. A `link` follows another group and wins
over the other fields.

Four chrome groups are not theme roles. Every theme links them to
another group, and `[groups]` can replace them like any other:

| Group | Default link | Colors |
| --- | --- | --- |
| `KageWorking` | `KageMuted` | the working row above the input |
| `KageApproval` | `KageWarning` | approval panel rules, title and selected option, and the bullet of a tool call waiting for approval |
| `KageDiffAdd` | `KageSuccess` | `+` lines in edit rows and approvals |
| `KageDiffDelete` | `KageToolErrorRule` | `-` lines in edit rows and approvals |

```toml
[groups]
KageApproval = { fg = "#e0af68", bold = true }
KageDiffAdd = { fg = "#9ece6a" }
```

A `Kage*` name must be one of the groups in the two tables above, and
a typo is an error. Any other name defines a group of your own, which
plugin spans and slot components can use through `hl`. The built-in
renderer reads only the colors of `Kage*` groups. Attributes such as
`bold` show where a group is used by name.

`init.lua` can change groups at runtime with `kage.api.hl_set`. See
[lua config](/guide/lua-config#highlight-groups).

## transparency

A terminal grid has no per-cell alpha, so kage cannot dim individual
regions. The only meaningful knob is whole-UI:

- `transparent = false` (default): kage paints an opaque base canvas
  over the entire frame, so nothing of the terminal shows through and
  there is no patchwork between blocks.
- `transparent = true`: kage skips that base, so a blurred or
  transparent terminal (or your wallpaper) shows through the whole
  UI. Modal overlays and popups still get an opaque backing so they
  stay legible over arbitrary terminal content.

There is no per-region opacity. "Make just the conversation
see-through" is not expressible in a terminal.
