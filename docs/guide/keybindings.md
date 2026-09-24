# keybindings

This page covers the keys kage's TUI listens for. Inside the TUI,
press `?` on an empty prompt (or run `/help`) for the same reference
as an overlay.

The TUI supports two editor modes: **modeless** (the default) and
**vim** (`[ui] editor = "vim"` in `config.toml`, or
`kage.opt.editor = "vim"` in `init.lua`). Both share the same
buffer navigation keys. Every key on this page that is not part of
the editor grammar can be remapped. See
[remapping keys](#remapping-keys).

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
| `Ctrl+P`         | Open the model picker                       |
| `Ctrl+S`         | Open the session picker                     |
| `F3`             | Open the message jump picker (filter, Enter jumps) |
| `Alt+P` / `Alt+N` | Jump to the previous / next block          |
| `Ctrl+N`         | Jump to the next block                      |
| `Ctrl+O`         | Toggle fold on the focused block            |
| `Shift+Tab`      | Cycle the thinking level                    |
| `Ctrl+V`         | Attach an image from the clipboard          |
| `Ctrl+G`         | Edit the prompt draft in `$VISUAL`/`$EDITOR` |
| `!`              | Shell escape: `!` on an empty prompt, then Enter runs the line |
| `?`              | Open the keyboard reference (Normal mode, or an empty modeless prompt)   |

`Ctrl+O` also expands a collapsed bracketed paste if one is present
in the input. When no paste is collapsed, it toggles the fold.

## vim modes

| Key             | From    | Effect                              |
| --------------- | ------- | ----------------------------------- |
| `Esc`           | Insert, Visual | Return to Normal mode        |
| `i`             | Normal  | Enter Insert mode                   |
| `v`             | Normal  | Enter Visual mode                   |
| `Ctrl+W`        | Normal  | Cycle focused pane (input / buffer) |
| `?`             | Normal  | Open the keyboard reference         |
| `:`             | Normal  | Open the `:` command line           |
| `Ctrl+Q`        | any     | Quit                                |
| `Ctrl+C`        | any     | Cancel current request              |

In vim Insert mode, `Ctrl+W` kills the previous word instead (see
[input editing](#input-editing)).

## modeless mode

In modeless mode the editor is always in an insert-like state.
`Esc` cancels the in-flight turn instead of entering Normal. All
Emacs/readline keys and the buffer navigation keys above work
without any mode switching.

| Key  | Effect                          |
| ---- | ------------------------------- |
| `Enter` | Send the prompt |
| `Shift+Enter` / `Alt+Enter` | Insert a newline |
| `Esc` | Cancel the current turn |
| `Ctrl+C` | Cancel the current turn |
| `PageUp` / `PageDown` | Scroll the conversation buffer 10 lines |
| `Ctrl+W` | Kill the previous word |
| `Ctrl+G` | Edit the prompt draft in `$VISUAL`/`$EDITOR` |
| `Shift+Tab` | Cycle the thinking level |
| `/`   | Open the command palette (empty prompt only) |
| `!`   | Switch to shell mode (empty prompt only) |
| `?`   | Open the keyboard reference (empty prompt only) |
| `Ctrl+Q` | Quit |

The `?` (keys), `/` (command palette), and `!` (shell escape)
prefixes all key off an empty prompt, so every surface stays one
keystroke away without a mode switch. With text in the prompt they
are typed as literal characters. In shell mode the placeholder reads
`run a shell command... (Backspace to cancel)`: Enter runs the line
with `sh`, and `Backspace` on the empty prompt leaves shell mode.

## command pathways

`/` on an empty prompt opens the command palette inline above the
input card. It lists matching commands as you type. This works in
modeless mode and in vim Insert mode.

In vim mode there is also the `:` ex line on the status row, opened
from Normal mode. It shares the palette's command registry, parser,
completion, and dispatch, so `:model anthropic:claude-sonnet-4` and
`/model anthropic:claude-sonnet-4` have identical effect.

| Key   | From                       | Effect                          |
| ----- | -------------------------- | ------------------------------- |
| `/`   | empty prompt               | Open the slash command palette  |
| `:`   | vim Normal                 | Open the colon command line     |
| `/`   | vim Normal                 | Begin a buffer search           |

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
| `Ctrl+C`         | Interrupt the running turn; line stays open        |

Completions are recomputed on every edit. The popup appears only after
the first `Tab` step that does more than insert the LCP, so single-
match completions resolve and close in one keystroke.

## validation

Submitting an invalid command keeps the line open and surfaces an
inline error below the row. Examples:

- `/mouse maybe` shows ``argument `state` must be one of: on, off, toggle (got `maybe`)``
- `/model` shows `` missing required argument `id` ``
- `/quut` shows `unknown command: quut (did you mean /quit?)`

Editing the line clears the error.

## vim normal-mode keys (buffer pane)

These keys work when the buffer pane is focused in vim Normal mode
(press `Ctrl+W` in Normal mode to switch panes, or press `Esc` from
Insert).

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
| `PageUp` / `PageDown` | Scroll buffer up / down 10 lines |

The active thinking level shows as a `think:<level>` pill in the
modeline (hidden when off), next to the running token cost.

## input editing

The prompt input is a readline / Emacs-style line editor. These keys
work in both vim Insert mode and modeless mode.

| Key            | Effect                                            |
| -------------- | ------------------------------------------------- |
| `Enter`        | Send the prompt                                   |
| `Shift+Enter`  | Insert a newline (`Alt+Enter` also works)         |
| `Up` / `Down`  | Move between lines, then walk the prompt history  |
| `Ctrl+A` / `Ctrl+E` | Start / end of the current line              |
| `Ctrl+W`       | Kill the word before the cursor (in vim Normal mode it cycles panes instead) |
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
| `Ctrl+G`       | Edit the draft in `$VISUAL`/`$EDITOR`              |

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

## remapping keys

Most keys above are mappings in one keymap table, which kage fills
from its embedded defaults at startup. Plugins, `config.toml` and
`init.lua` add to the same table in that order, and the last mapping
set for a key wins. So `init.lua` beats `config.toml`, which beats
plugins, which beat the defaults.

`init.lua` is the full interface: modes, key sequences, a leader,
Lua functions and deleting defaults. See
[lua config](/guide/lua-config#keymaps). The `[keybindings]` table in
`config.toml` covers the common case.

The editor grammar is not in the table: vim motions, operators,
counts, registers, undo and redo, readline edits, Enter, Esc, history
Up and Down, Ctrl+O in insert mode, Ctrl+G and the modeless `/`, `!`
and `?` prefixes. A mapping on one of these keys shadows it, but
cannot remove it.

### `[keybindings]` in config.toml

```toml
[keybindings]
# the key <leader> expands to: one key, default a backslash.
leader = "<C-x>"
# ms a mapping that is also the start of a longer one waits for
# more keys (0 to 5000). Default 1000.
timeoutlen = 600
bindings = { "ctrl+t" = "theme set tokyo-night", "ctrl+l" = "action:OpenModelPicker", "<leader>s" = "settings", "<leader>q" = "quit" }
```

With this table, `Ctrl+X` then `s` opens the settings dialog and
`Ctrl+X` then `q` quits.

Mappings go in the `bindings` table, written inline as above or as a
`[keybindings.bindings]` table. A key written directly under
`[keybindings]`, as older versions of this page showed, is reported as
an error at startup instead of being dropped.

Every binding maps in mode `g`, which covers every editing state. A
key is either the chord form (`ctrl+shift+x`, `alt+p`, `f5`) or Vim
notation (`<C-t>`, `<F2>`, `<leader>s`, `gs`). `<leader>` expands with
the `leader` value from the same table. A key sequence waits up to
`timeoutlen` for the next key. Prefer modified keys. A bare letter, or
a leader that is a letter or punctuation, catches that key while you
type in the prompt.

The value is a command line, run through the same executor as the
command palette, so anything `/` can do is bindable, including `quit`
and plugin commands. A value starting with `action:` names a built-in
action instead and is never run as a command. A key or action that
does not parse is reported inline at startup, never silently dropped.

These action names work after `action:`:

| Group      | Name                   | Effect                            |
| ---------- | ---------------------- | --------------------------------- |
| navigation | `ScrollToTop`          | snap the buffer to the top        |
| navigation | `ScrollToBottom`       | snap the buffer to the bottom     |
| navigation | `FocusPrev`            | focus the previous block          |
| navigation | `FocusNext`            | focus the next block              |
| navigation | `CyclePane`            | cycle pane focus (input / buffer) |
| overlays   | `OpenModelPicker`      | open the model picker             |
| overlays   | `OpenSessionPicker`    | open the session picker           |
| overlays   | `OpenCommandPalette`   | open the slash command palette    |
| overlays   | `OpenJumpPicker`       | open the message jump picker      |
| overlays   | `OpenHelp`             | open the keyboard reference       |
| folds      | `ToggleFold`           | toggle the focused block's fold   |
| folds      | `UnfoldAll`            | open every fold                   |
| folds      | `FoldAll`              | close every fold                  |
| search     | `BeginSearch`          | open the `/` search line          |
| search     | `SearchNext`           | jump to the next match            |
| search     | `SearchPrev`           | jump to the previous match        |
| misc       | `BeginCommand`         | open the `:` command line         |
| misc       | `Cancel`               | cancel the in-flight turn         |
| misc       | `Yank`                 | copy the active selection         |
| misc       | `YankFocusedBlock`     | copy the focused block            |
| misc       | `ClearSelection`       | drop the active selection         |
| misc       | `EnterVisual`          | start a visual selection          |
| misc       | `AttachClipboardImage` | attach an image from the clipboard |
| misc       | `CycleThinkingLevel`   | step the thinking level           |

Scrolling by a line count needs an argument, so it is only available
from Lua as `kage.action.scroll(n)`.

`CycleThinkingLevel` steps the thinking level (also `Shift+Tab`).
The level a new TUI session starts on comes from
`[ui] thinking_level` (one of `off`, `minimal`, `low`, `medium`,
`high`, `xhigh`). The cycle still overrides it per session.

### quit and cancel hatches

`Ctrl+Q` quits and `Ctrl+C` cancels the running turn from anywhere,
even a stuck modal. They yield **only** to a mapping from `config.toml`
or `init.lua` on the same key. Then your mapping wins, and quit stays
reachable through whatever key you mapped `quit` to. A plugin mapping
on either key never fires and logs a warning.

### listing mappings

Run `:keybindings` (alias `:keys`) to list the table per mode, with
each mapping's action or command and its owner (`defaults`, a plugin
name, `config.toml` or `init.lua`), followed by the editor grammar
keys and the two hatches. The `?` reference is built from the same
table: it shows every mapping with a description.

## plugin keybindings

Plugins bind keys with
[`kage.register_keybinding`](/plugins/api#keybindings), which maps a
chord in mode `g`, or with `kage.keymap.set`. Plugins load after the
defaults, so a plugin mapping replaces a default one on the same key.
`config.toml` and `init.lua` load after plugins, so your mappings
replace plugin ones. No mapping applies while a modal layer is open.
