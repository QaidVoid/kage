# quickstart

Open a terminal in any project directory and run:

```bash
kage
```

The first launch connects a provider: kage opens a provider picker and
prompts for an API key (env vars like `ANTHROPIC_API_KEY` also work;
set one and restart instead). After connecting, you land in the
interactive TUI. The cursor sits in the input card at the bottom of
the screen, ready for your prompt.

## a first turn

Type a question or a task and press Enter:

```
explain what this repository does in one paragraph
```

The assistant streams its response into the conversation buffer above
the input card. Tool calls (file reads, shell commands, edits) show
up as their own blocks with their inputs, outputs, and timing.

Cancel a running turn with `Esc` or Ctrl+C. Quit with Ctrl+Q or by
typing `/quit`.

## finding your way around

The empty prompt shows `Send a message...  (/ commands, ! shell, ? keys)`.
Each of those prefixes works on an empty prompt:

- `/` opens the command palette. Try `/settings` for theme, model,
  and thinking defaults.
- `!` switches to shell mode (see below).
- `?` opens the keyboard reference overlay. `/help` opens it too.

## editing and navigation

The prompt is always editable: type, then press Enter to send. Press
`Shift+Enter` to insert a newline instead (`Alt+Enter` also works, for
terminals that do not report Shift+Enter). Readline keys work
(`Ctrl+A`/`Ctrl+E` line start/end, `Ctrl+U`/`Ctrl+K` kill, `Ctrl+/`
undo), and `Up`/`Down` walk your prompt history. `Esc` cancels a
running turn. `Ctrl+G` opens the whole draft in an external editor
(`$VISUAL` or `$EDITOR`) for longer prompts. `Shift+Tab` cycles the
thinking level.

Prefer vim? Set `editor = "vim"` under `[ui]` in config.toml (or
toggle it in `/settings`) to get normal/insert/visual modes with
motions, operators, and registers. In vim mode, `Esc` enters Normal
mode, `:` opens the command line there, and `?` opens the keyboard
reference.

## the conversation buffer

- `PageUp` / `PageDown` scroll ten lines
- `Ctrl+Up` / `Ctrl+Down` scroll one line
- `Ctrl+P` opens the model picker, `Ctrl+S` the session picker
- `Alt+P` / `Alt+N` focus the previous / next block
- `Ctrl+O` folds or unfolds the focused block
- `F3` opens the message jump picker
- the mouse wheel scrolls

In vim mode, normal-mode keys (`j`/`k`, `G`, `[`/`]`, `zM`/`zR`, `y`)
work on the buffer once you press `Esc`. `/` in Normal mode starts a
buffer search, `n` and `N` walk matches, and `Ctrl+W` in Normal mode
cycles focus between the input and buffer panes.

## running shell commands

Type `!` on an empty prompt to switch to shell mode (the prompt glyph
becomes `!` and the placeholder reads `run a shell command...
(Backspace to cancel)`). Enter runs the line with `sh` in the session
working directory. The output lands in the conversation as its own
block and the model sees a summary on the next turn, so follow-up
prompts can reference it. After the command runs, the prompt returns
to normal. `Backspace` on the empty shell prompt leaves shell mode
without running anything.

Shell runs are conversation-only: they are not re-applied when you
resume a recorded session.

## switching models mid-session

Type `/model <provider>:<model>` or open the model picker with
`Ctrl+P`. The next turn uses the new model; prior history is
preserved verbatim.

## recording and resuming

Every session is recorded to `~/.local/share/kage/sessions/<id>.jsonl`
unless you pass `--no-session`. Resume the most recent session with:

```bash
kage resume --last -p "continue where we left off"
```

List recorded sessions:

```bash
kage list
```

Fork a session at a specific entry to try a different path without
losing the original:

```bash
kage fork <session-prefix> --at <entry-prefix>
```

## print mode

For scripted use, run a single prompt and stream the response to
stdout:

```bash
kage -p "summarize this Cargo.toml" --model anthropic:claude-sonnet-4-6
```

## next steps

- [Keybindings](/guide/keybindings): the full key map
- [Commands](/guide/commands): everything the `/` palette accepts
- [Plugins](/plugins/): extend kage in Lua
