# keybindings

This page covers the keys kage's TUI listens for. It is not the full
input grammar; refer to `crates/kage-tui/src/input.rs` for the
authoritative key-to-action map.

The TUI is modal, vim-style: Normal, Insert, and Visual. The default
mode on launch is Insert so users can start typing immediately. Esc
returns to Normal from anywhere.

## modes

| Key             | From    | Effect                              |
| --------------- | ------- | ----------------------------------- |
| `Esc`           | any     | Return to Normal mode               |
| `i`             | Normal  | Enter Insert mode                   |
| `v`             | Normal  | Enter Visual mode                   |
| `Ctrl+W`        | any     | Cycle focused pane (input / buffer) |
| `Ctrl+Q`        | any     | Quit immediately                    |
| `Ctrl+C`        | Normal  | Cancel current request              |

## command pathways

There are two surfaces over the same command spec registry: the `:`
ex-line and the `/` palette. Both parse, complete, and dispatch
through identical code paths. The only visual difference is layout:
`:` sits on the status row, `/` opens inline above the input card and
lists matching commands.

| Key   | From         | Effect                                 |
| ----- | ------------ | -------------------------------------- |
| `:`   | Normal       | Open the colon command line            |
| `/`   | Insert empty | Open the slash command palette         |
| `/`   | Normal       | Begin a buffer search                  |

Both `:` and `/` accept the same commands and arguments. For example,
`:model anthropic:claude-sonnet-4` and `/model anthropic:claude-sonnet-4`
have identical effect.

## command line autocomplete

Tab completion matches vim's `wildmode=longest:full,full`:

| Key              | Effect                                              |
| ---------------- | --------------------------------------------------- |
| `Tab`            | Extend to longest common prefix; cycle thereafter   |
| `Shift+Tab`      | Cycle in reverse                                    |
| `Down` / `Up`    | Cycle through completions when the popup is open    |
| `Enter`          | Submit; validation errors keep the line open        |
| `Esc`            | Dismiss the popup; if no popup, cancel the line     |
| `Backspace`      | Delete previous character; on empty input, cancel   |
| `Left` / `Right` | Move the cursor                                     |
| `Home` / `End`   | Jump to start / end                                 |
| `Ctrl+C`         | Cancel the line                                     |

Completions are recomputed on every edit. The popup appears only after
the first `Tab` step that does more than insert the LCP, so single-
match completions resolve and close in one keystroke.

## validation

Submitting an invalid command keeps the line open and surfaces an
inline error below the row. Examples:

- `:mouse maybe` shows `state must be one of: on, off, toggle`
- `:model` shows `missing required arg: id`
- `:quut` shows `unknown command: quut (did you mean :quit?)`

Editing the line clears the error.

## other normal-mode keys

| Key       | Effect                                       |
| --------- | -------------------------------------------- |
| `j` / `k` | Scroll buffer (when buffer pane is focused)  |
| `G`       | Snap to bottom and re-arm auto-follow        |
| `[` / `]` | Focus previous / next block                  |
| `zM`      | Fold all blocks                              |
| `zR`      | Unfold all blocks                            |
| `n` / `N` | Jump to next / previous search match         |
| `Ctrl+P`  | Open model picker                            |
| `Ctrl+S`  | Open session picker                          |
| `Shift+Tab` | Cycle thinking level (off -> minimal -> low -> medium -> high -> xhigh) |
| `y`       | Yank current selection                       |
| `Y`       | Yank focused block                           |

The active thinking level shows as a `think:<level>` pill in the
modeline (hidden when off), next to the running token cost.

## input editing

The prompt input is a readline / Emacs-style line editor.

| Key            | Effect                                            |
| -------------- | ------------------------------------------------- |
| `Ctrl+A` / `Ctrl+E` | Start / end of the current line              |
| `Ctrl+W`       | Kill the word before the cursor                   |
| `Ctrl+U`       | Kill to start of line                              |
| `Ctrl+K`       | Kill to end of line                                |
| `Alt+Backspace` | Kill the previous word                           |
| `Alt+D`        | Kill the next word                                 |
| `Alt+B` / `Alt+F` | Move backward / forward one word               |
| `Ctrl+Y`       | Yank (paste) the most recent kill                  |
| `Ctrl+/`       | Undo the last edit (also `Ctrl+_`)                 |
| `Ctrl+O`       | Expand a collapsed pasted block in place           |

`Ctrl+W`, `Ctrl+U`, `Ctrl+K`, `Alt+Backspace`, and `Alt+D` feed a
kill ring; `Ctrl+Y` yanks the most recent entry. A bracketed paste
of 10 or more lines collapses to a `[paste #N: M lines]` placeholder
so it does not flood the input; the full text is still sent on
submit, and `Ctrl+O` expands it inline if you want to edit it first.

## remapping keys (`[keybindings]`)

Bind any chord to any command in `config.toml`. The bound string runs
through the same executor as the `:` command line, so anything `:`
can do - including `quit` and plugin commands - is bindable:

```toml
[keybindings]
"ctrl+s" = "settings"
"ctrl+t" = "theme set tokyo-night"
"ctrl+x" = "quit"
```

Config bindings are user-authoritative: they are checked before
plugin keybindings and before built-in handling, so you can always
reclaim a key. A chord that does not parse is reported inline at
startup (never silently dropped). Prefer modified chords - a bare
letter will shadow typing it into the prompt.

`Ctrl+Q` quits as a panic hatch even from a stuck modal. It yields
**only** if you explicitly bind `ctrl+q` to something in
`[keybindings]` - then your config wins and quit is reachable via
whatever chord you mapped `quit` to.

Run `:keybindings` (alias `:keys`) to list every active binding:
your config bindings, plugin-registered chords, and the reserved
keys the TUI handles itself.

## plugin keybindings

Plugins bind their own chords with
[`kage.register_keybinding`](/plugins/api#keybindings). A plugin
chord is checked after `[keybindings]` config but before built-in
key handling, so it wins over the built-in binding for that key -
but never over user config, an open modal layer, or the `Ctrl+Q`
quit hatch. Binding a reserved chord still works and logs a warning.
