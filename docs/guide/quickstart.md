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

Cancel a running turn with Ctrl+C. Quit with Ctrl+Q or by typing
`:q`.

## finding your way around

Press `?` in Normal mode (`Esc`, then `?`) for the keyboard reference
overlay. Type `:` for the command line, `:settings` for themes,
model, and thinking defaults.

## editing and navigation

The prompt is always editable: type, press Enter to send. Readline
keys work (`Ctrl+A`/`Ctrl+E` line start/end, `Ctrl+U`/`Ctrl+K` kill,
`Ctrl+/` undo). `Esc` cancels a running turn, `PageUp`/`PageDown`
scroll the conversation. `Ctrl+G` opens the whole draft in an
external editor (`$VISUAL` or `$EDITOR`) for longer prompts.

Prefer vim? Set `editor = "vim"` under `[ui]` in config.toml (or
toggle it in `:settings`) to get normal/insert/visual modes with
motions, operators, and registers.

## the conversation buffer

- `PageUp` / `PageDown` scroll ten lines
- `Ctrl+P` opens the model picker, `Ctrl+S` the session picker
- `Ctrl+O` folds or unfolds the focused block
- `/` starts a buffer search, `n` and `N` walk matches
- the mouse wheel scrolls

In vim mode, normal-mode keys (`j`/`k`, `G`, `[`/`]`, `zM`/`zR`, `y`)
work on the buffer once you press `Esc` (`Ctrl+W` cycles panes).

## running shell commands

Type `!` on an empty prompt to switch to shell mode (the prompt glyph
becomes `!`). Enter runs the line with `sh` in the session working
directory; the output lands in the conversation as its own block and
the model sees a summary on the next turn, so follow-up prompts can
reference it. `Backspace` on the empty prompt (or just submitting)
returns you to the normal prompt.

Shell runs are conversation-only: they are not re-applied when you
resume a recorded session.

## switching models mid-session

Type `:model <provider>:<model>` or open the model picker with
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

- [Keybindings](/guide/keybindings) - the full key map
- [Commands](/guide/commands) - everything the `:` and `/` palettes accept
- [Plugins](/plugins/) - extend kage in Lua
