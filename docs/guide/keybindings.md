# keybindings

This page covers the keys kage's TUI listens for. It is not the full
input grammar; refer to `crates/kage-tui/src/input.rs` for the
authoritative key-to-action map.

The TUI supports two editor modes: **vim** (the default) and
**modeless** (`editor = "modeless"` in config). Both share the same
buffer navigation keys.

## buffer navigation (works in both modes)

These keys scroll and navigate the conversation buffer from inside
Insert mode (vim) or the always-editing state (modeless). No need to
switch modes or panes.

| Key              | Effect                                       |
| ---------------- | -------------------------------------------- |
| `Ctrl+Down`      | Scroll the buffer down 1 line                |
| `Ctrl+Up`        | Scroll the buffer up 1 line                  |
| `Ctrl+Home`      | Snap to the top of the conversation         |
| `Ctrl+End`       | Snap to the bottom, re-arm auto-follow      |
| `Ctrl+P`         | Jump to the previous block                  |
| `Ctrl+N`         | Jump to the next block                      |
| `Ctrl+O`         | Toggle fold on the focused block            |
| `PageUp`         | Scroll the buffer up 10 lines               |
| `PageDown`       | Scroll the buffer down 10 lines             |

`Ctrl+O` also expands a collapsed bracketed paste if one is present
in the input. When no paste is collapsed, it toggles the fold.

## vim modes

| Key             | From    | Effect                              |
| --------------- | ------- | ----------------------------------- |
| `Esc`           | any     | Return to Normal mode               |
| `i`             | Normal  | Enter Insert mode                   |
| `v`             | Normal  | Enter Visual mode                   |
| `Ctrl+W`        | any     | Cycle focused pane (input / buffer) |
| `Ctrl+Q`        | any     | Quit immediately                    |
| `Ctrl+C`        | Normal  | Cancel current request              |

## modeless mode

In modeless mode the editor is always in an insert-like state.
`Esc` cancels the in-flight turn instead of entering Normal. All
Emacs/readline keys and the buffer navigation keys above work
without any mode switching.

| Key  | Effect                   |
| ---- | ------------------------ |
| `Esc` | Cancel the current turn |

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

## vim normal-mode keys (buffer pane)

These keys work when the buffer pane is focused in vim Normal mode
(use `Ctrl+W` to switch panes, or press `Esc` from Insert).

| Key       | Effect                                       |
| --------- | -------------------------------------------- |
| `j` / `k` | Scroll buffer 1 line                         |
| `G`       | Snap to bottom and re-arm auto-follow        |
| `gg`      | Snap to top                                  |
| `[` / `]` | Focus previous / next block                  |
| `zo` / `zc` | Toggle fold on focused block               |
| `zM`      | Fold all blocks                              |
| `zR`      | Unfold all blocks                            |
| `n` / `N` | Jump to next / previous search match         |
| `y`       | Yank current selection                       |
| `Y`       | Yank focused block                           |
| `v`       | Enter visual (cell selection)                |

The active thinking level shows as a `think:<level>` pill in the
modeline (hidden when off), next to the running token cost.

## input editing

The prompt input is a readline / Emacs-style line editor. These keys
work in both vim Insert mode and modeless mode.

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
| `Ctrl+O`       | Toggle fold (or expand a collapsed paste)          |
| `Ctrl+S`       | Open session picker                                |
| `Ctrl+P`       | Open model picker                                  |

`Ctrl+W`, `Ctrl+U`, `Ctrl+K`, `Alt+Backspace`, and `Alt+D` feed a
kill ring; `Ctrl+Y` yanks the most recent entry. A bracketed paste
of 10 or more lines collapses to a `[paste #N: M lines]` placeholder
so it does not flood the input; the full text is still sent on
submit, and `Ctrl+O` expands it inline if you want to edit it first.

## vim normal-mode keys (input pane)

When the input pane is focused in Normal mode, full vim motions and
operators are available for editing the prompt text.

| Key           | Effect                                      |
| ------------- | -------------------------------------------- |
| `h` / `l`     | Move cursor left / right                    |
| `j` / `k`     | Move cursor down / up (multi-line input)    |
| `w` / `b` / `e` | Word motions                              |
| `0` / `$` / `^` | Line start / end / first non-blank        |
| `G` / `gg`    | End / start of text                         |
| `x` / `X`     | Delete char at / before cursor              |
| `r{ch}`       | Replace char at cursor                      |
| `D` / `C`     | Delete / change to end of line              |
| `dd`          | Delete current line                         |
| `cc`          | Change current line (delete + insert)       |
| `yy`          | Yank current line                           |
| `dw` / `cw` / `yw` | Delete / change / yank word           |
| `p` / `P`     | Paste after / before cursor                 |
| `u`           | Undo                                        |
| `Ctrl+R`      | Redo                                        |
| `v`           | Visual select (char-wise)                   |
| `i` / `a`     | Insert before / after cursor                |
| `I` / `A`     | Insert at line start / end                  |
| `o` / `O`     | Open line below / above                     |
| `3dw`         | Delete 3 words (count prefix)               |

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
