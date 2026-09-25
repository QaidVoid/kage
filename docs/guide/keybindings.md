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
| `Ctrl+T`         | Open the agents overlay                     |
| `Ctrl+F`         | Search the conversation (Insert mode or modeless) |
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
| `Ctrl+C`        | any     | Clear the draft, else interrupt the run, else arm quit |

In vim Insert mode, `Ctrl+W` kills the previous word instead (see
[input editing](#input-editing)).

## modeless mode

In modeless mode the editor is always in an insert-like state.
`Esc` never enters Normal. It clears the draft or interrupts the run
(see [esc and ctrl+c](#esc-and-ctrl-c)). All Emacs/readline keys and
the buffer navigation keys above work without any mode switching.

| Key  | Effect                          |
| ---- | ------------------------------- |
| `Enter` | Send the prompt, or steer it into the running turn |
| `Tab` | Queue the prompt until the running turn ends |
| `Shift+Enter` / `Alt+Enter` | Insert a newline |
| `Esc` | Clear the draft, else interrupt the run |
| `Ctrl+C` | Clear the draft, else interrupt the run, else arm quit |
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
`Run a shell command (Backspace leaves shell mode)`: Enter runs the
line with `sh`, and `Backspace` on the empty prompt leaves shell mode.

## esc and ctrl+c

`Esc` in modeless mode and `Ctrl+C` in every mode step through the
same escalation:

1. With a draft in the prompt, they clear it. The draft goes to the
   prompt history, so `Up` brings it back, and the footer reads
   `draft cleared, up restores it`.
2. With an empty draft while kage works, they interrupt the run. The
   conversation shows `Interrupted`.
3. Idle with an empty draft, `Esc` does nothing, and `Ctrl+C` arms
   quit. The footer reads `ctrl+c again to quit`, and a second
   `Ctrl+C` within 2 seconds quits.

In an agent view (see [agents](#agents)), the last two steps change:
`Esc` goes back one level and never stops the agent, and `Ctrl+C`
stops the agent while it runs, else goes back. Quit is only armed
from the main view.

An open popup, such as the completion popup or the command palette,
takes `Esc` first. In vim mode `Esc` keeps its vim meaning. While an
overlay is open (a picker, a dialog, the `:` line, the search line or
the approval panel), `Ctrl+C` only interrupts the run and leaves the
draft alone.

## sending while kage works

The prompt stays editable during a run, and there are two ways to send
what you type:

- `Enter` steers. The prompt joins the running turn at the next turn
  boundary, after the current tool call.
- `Tab` queues. The prompt waits and starts a new run once the current
  one ends. Idle, `Tab` does nothing, so a stray press never sends a
  prompt.

Prompts that were sent but not delivered yet show above the input,
each with `after the current tool call` or `when this run ends`. Up to
three rows show, then `+N more`. A row disappears when kage delivers
its prompt. A prompt with an attached image always waits for the run
to end.

While kage works, the working row above the input shows what it is
doing and for how long, such as
`Running cargo test (14s, esc to interrupt)`. While agents run it
counts them instead, such as `Waiting for 3 agents (41s, esc to
interrupt)`.

In an agent view, `Enter` and `Tab` send to that agent instead of the
main session.

## approvals

When a tool call needs your approval (see
[permissions](/guide/permissions)), a panel replaces the input box.
Its title names the action, such as `Run this command?` or
`Edit src/lib.rs?`, and it shows the command, the diff or the
arguments. Your draft is kept and comes back once no approval is left.

| Key                   | Effect                                          |
| --------------------- | ----------------------------------------------- |
| `1` / `y`             | Yes, run this call                              |
| `2` / `s`             | Yes, and allow the tool for the rest of the session |
| `3` / `a`             | Yes, and always allow the tool (saved to `config.toml`) |
| `4` / `n` / `Esc`     | No                                              |
| `5` / `t`             | No, and tell kage what to do instead            |
| `Up` / `Down`         | Move the selection                              |
| `Enter`               | Confirm the selection. `Yes` starts selected.   |
| `Ctrl+C`              | Interrupt the run, which denies the call        |

Keys pressed in the first 400 ms after a panel opens are dropped, so
typing meant for the prompt cannot answer it. Option 5 opens a
one-line field: `Enter` denies the call and sends your text to the
model, and `Esc` goes back to the options. When several calls wait,
the title shows `1 of 3`.

Requests from [agents](/guide/agents#approvals-from-agents) join the
same queue. The title then starts with the agent's name, and option 5
sends your text to that agent.

## agents

These keys drive [agents](/guide/agents#agents-in-the-tui). Click a
row of the pinned agent list above the input, or pick one in the
agents overlay, to open that agent. The whole view then shows it,
with a breadcrumb in the header.

| Key | Where | Effect |
| --- | --- | --- |
| `Ctrl+T` | anywhere | Open the agents overlay (also `/agents`) |
| `Enter` | agent view | Steer the running agent, or message a finished one |
| `Tab` | agent view | Queue the prompt until the agent's run ends |
| `Esc` | agent view, empty prompt | Go back one level, to the parent agent or the main view |
| `Ctrl+C` | agent view, empty prompt | Stop the agent while it runs, else go back |
| `Up` / `Down`, `k` / `j` | overlay | Move the selection |
| `Home` / `End` | overlay | Jump to the first / last row |
| `Enter` | overlay | Open the selected agent, or the main view from the `kage` row |
| `x` | overlay | Stop the selected agent and the agents under it |
| `Esc` | overlay | Close the overlay |

In vim mode, `Esc` in Insert mode still enters Normal mode, and `Esc`
in Normal mode goes back from an agent view.

## search

`Ctrl+F` (modeless mode and vim Insert) and `/` in vim Normal open the
search line on the bottom row. Typing searches as you go and shows
the match count, such as `match 2/5`. While the line is open, `Up` and
`Down` walk the matches. `Enter` closes the line and keeps the
pattern, so `n` and `N` in vim Normal mode walk it later. `Esc` closes
the line and restores the previous pattern and view. `/noh` clears the
highlighting.

## mouse

With mouse capture on (`/mouse on`, the default), the wheel scrolls
the conversation. A click on a block focuses it, and a click on a
block's first row folds or unfolds it. Dragging selects text, and
dragging past the top or bottom edge scrolls. Releasing the button
copies the selection to the clipboard and shows
`copied N characters`. A right-click opens a menu for the block under
the pointer. `/mouse off` hands selection back to the terminal.

## command pathways

`/` on an empty prompt opens the command palette inline above the
input box. It lists matching commands as you type, most used first,
with the first row selected, so `/` then `Enter` opens the model
picker. This works in modeless mode and in vim Insert mode.

In vim mode there is also the `:` ex line on the bottom row, opened
from Normal mode. It shares the palette's command registry, parser,
completion, and dispatch, so `:model anthropic:claude-sonnet-4` and
`/model anthropic:claude-sonnet-4` have identical effect. `/model`
without an id opens the model picker, like `Ctrl+P`.

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
| `Ctrl+C`         | Interrupt the running turn. The line stays open.   |

Completions are recomputed on every edit. The popup appears only after
the first `Tab` step that does more than insert the LCP, so single-
match completions resolve and close in one keystroke.

## validation

Submitting an invalid command keeps the line open and surfaces an
inline error below the row. Examples:

- `/mouse maybe` shows ``argument `state` must be one of: on, off, toggle (got `maybe`)``
- `/theme set` shows `` missing required argument `name` ``
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

The active thinking level shows as `thinking <level>` on the right of
the input's top rule, hidden when off.

## input editing

The prompt input is a readline / Emacs-style line editor. These keys
work in both vim Insert mode and modeless mode.

| Key            | Effect                                            |
| -------------- | ------------------------------------------------- |
| `Enter`        | Send the prompt (steer it during a run)           |
| `Tab`          | Queue the prompt until the run ends               |
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
kill ring, and `Ctrl+Y` yanks the most recent entry. A bracketed
paste of 10 or more lines collapses to a `[paste #N: M lines]`
placeholder so it does not flood the input. A paste of more than 1000
characters on fewer lines collapses to `[paste #N: M chars]`. The full
text is still sent on submit, and `Ctrl+O` expands it inline if you
want to edit it first.

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
bindings = { "f6" = "theme set tokyo-night", "ctrl+l" = "action:OpenModelPicker", "<leader>s" = "settings", "<leader>q" = "quit" }
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
| overlays   | `OpenAgents`           | open the agents overlay           |
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
| misc       | `QueuePrompt`          | queue the prompt until the run ends (does nothing while idle) |

Scrolling by a line count needs an argument, so it is only available
from Lua as `kage.action.scroll(n)`.

`CycleThinkingLevel` steps the thinking level (also `Shift+Tab`).
The level a new TUI session starts on comes from
`[ui] thinking_level` (one of `off`, `minimal`, `low`, `medium`,
`high`, `xhigh`). The cycle still overrides it per session.

### quit and cancel hatches

`Ctrl+Q` quits and `Ctrl+C` escalates (see
[esc and ctrl+c](#esc-and-ctrl-c)) from anywhere, even a stuck
overlay. They yield **only** to a mapping from `config.toml` or
`init.lua` on the same key. Then your mapping wins, and quit stays
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
