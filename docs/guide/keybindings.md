# keybindings

This page covers the keys kage's TUI listens for. Inside the TUI,
press `?` on an empty prompt (or run `/help`) for the same reference
as an overlay.

The TUI supports two editor modes: **modeless** (the default) and
**vim** (`editor = "vim"` in config). Both share the same
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

## remapping keys (`[keybindings]`)

Bind any chord to any command in `config.toml`. The bound string runs
through the same executor as the command palette, so anything `/`
can do is bindable, including `quit` and plugin commands:

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

### binding builtin actions (`action:` form)

A binding value can also name a builtin input action directly with
the `action:` prefix. The bound chord then fires the same action the
built-in key for it would, checked before builtin handling so the
remap wins:

```toml
[keybindings]
"ctrl+l" = "action:OpenModelPicker"
"ctrl+y" = "action:YankFocusedBlock"
```

Command strings remain the default form: a value
without the `action:` prefix is a command string, exactly as before,
and an `action:` value is never run through the command executor.

These action names are rebindable:

| Group      | Name                 | Effect                            |
| ---------- | -------------------- | --------------------------------- |
| navigation | `ScrollToTop`        | snap the buffer to the top        |
| navigation | `ScrollToBottom`     | snap the buffer to the bottom     |
| navigation | `FocusPrev`          | focus the previous block          |
| navigation | `FocusNext`          | focus the next block              |
| navigation | `CyclePane`          | cycle pane focus (input / buffer) |
| overlays   | `OpenModelPicker`    | open the model picker             |
| overlays   | `OpenSessionPicker`  | open the session picker           |
| overlays   | `OpenCommandPalette` | open the slash command palette    |
| folds      | `ToggleFold`         | toggle the focused block's fold   |
| folds      | `UnfoldAll`          | open every fold                   |
| folds      | `FoldAll`            | close every fold                  |
| search     | `BeginSearch`        | open the `/` search line          |
| search     | `SearchNext`         | jump to the next match            |
| search     | `SearchPrev`         | jump to the previous match        |
| misc       | `BeginCommand`       | open the `:` command line         |
| misc       | `Cancel`             | cancel the in-flight turn         |
| misc       | `Yank`               | copy the active selection         |
| misc       | `YankFocusedBlock`   | copy the focused block            |
| misc       | `ClearSelection`     | drop the active selection         |
| misc       | `CycleThinkingLevel` | step the thinking level           |

Payload-carrying actions (`Submit`, `Scroll`, `EnterMode`,
`FocusPane`) and the visual-mode cursor moves are not nameable: a
config binding fires with no arguments, so only payload-free
actions have names. An unknown or empty name after `action:` fails
startup with an error line naming the offending value, the same
surface an unparseable chord uses.

`CycleThinkingLevel` steps the thinking level (also `Shift+Tab`).
The level a new TUI session starts on comes from
`[ui] thinking_level` (one of `off`, `minimal`, `low`, `medium`,
`high`, `xhigh`); the cycle still overrides it per session.

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
