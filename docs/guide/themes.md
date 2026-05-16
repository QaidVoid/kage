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

`:theme set` applies on the next frame. It does **not** persist on its
own: set the default in `config.toml`

```toml
[ui]
theme = "my-theme"
```

or use the `:settings` dialog, which writes the choice back for you.
The configured theme is resolved at startup; an unknown name leaves
the default palette in place and reports the error inline rather than
failing silently.

Plugins switch themes through `kage.theme.set("name")`, which goes
through the same resolver.

## writing a custom theme

A user theme is a TOML file at `~/.config/kage/themes/<name>.toml`
(honoring `XDG_CONFIG_HOME`). The file name is the theme name; there
is no `name` key. Only three top-level keys are accepted, and an
unknown key is an error:

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
```

Every key under `[colors]` is a role name (full list below). A typo
in a role name or an unparseable color aborts the load with a message
naming the offender; the theme is not partially applied.

### color syntax

Color values use ratatui's grammar:

- 24-bit hex: `#rrggbb` (e.g. `#1f2430`)
- a named color: `black`, `red`, `green`, `yellow`, `blue`,
  `magenta`, `cyan`, `gray`, `darkgray`, `lightred`, `lightgreen`,
  `lightyellow`, `lightblue`, `lightmagenta`, `lightcyan`, `white`
- a 256-color index as a string: `"244"`

Terminals without truecolor downsample `#rrggbb` to their nearest
indexed color.

### color roles

`[colors]` accepts these keys. Anything you omit keeps the value from
`base`.

Canvas and turns:

- `bg` -- whole-frame base canvas behind every block
- `user_bg`, `user_rule` -- prompt bubble fill and left rule
- `assistant_rule` -- idle left spine on assistant turns
- `assistant_fg`, `thinking_fg`, `custom_fg` -- text foregrounds

Tool blocks:

- `tool_bg`, `tool_error_bg`, `tool_pending_bg` -- block fills
- `tool_rule`, `tool_error_rule`, `tool_pending_rule` -- left rules
- `tool_result_fg`, `tool_error_fg` -- result body text

Emphasis and chrome:

- `match_color`, `selection_color`, `focus_color` -- search match,
  visual selection, and focused-block accents
- `muted_fg` -- secondary-but-readable hints and metadata
- `status_bg`, `status_dim_fg` -- status bar
- `modeline_bg`, `modeline_fg` -- bottom modeline

Input card:

- `input_border_normal`, `input_border_insert`, `input_border_visual`
- `input_pill_normal_bg`, `input_pill_normal_fg`,
  `input_pill_insert_bg`, `input_pill_insert_fg`,
  `input_pill_visual_bg`, `input_pill_visual_fg`
- `input_glyph_fg`, `input_placeholder_fg`, `input_hint_fg`

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

There is no per-region opacity; "make just the conversation
see-through" is not expressible in a terminal.
