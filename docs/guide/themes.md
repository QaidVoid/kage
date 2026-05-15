# themes

kage ships bundled themes and lets you drop additional ones into
`~/.kage/themes/`.

## switching themes

```text
:theme list
:theme set kage-dark
:theme current
```

The change applies immediately and persists by writing to your
`config.toml`.

## writing a custom theme

A theme is a TOML file under `~/.kage/themes/<name>.toml`:

```toml
name = "amber-cli"

[colors]
background = "#0a0a0a"
foreground = "#e5e5e5"
accent = "#f4a72b"
user_fg = "#f4a72b"
assistant_fg = "#e5e5e5"
thinking_fg = "#9aa0a6"
tool_fg = "#7a7a7a"
tool_error_fg = "#ef6f6c"
focus_color = "#f4a72b"
match_color = "#fbbf24"
status_bg = "#15151c"
status_fg = "#9aa0a6"
status_dim_fg = "#65686f"
modeline_bg = "#0d0d12"
selection_color = "#1f1f2a"
```

Themes use 24-bit RGB colors (`#rrggbb`). Terminals without truecolor
support fall back to their nearest indexed color.

Reload after editing without restarting:

```text
:theme reload
```
