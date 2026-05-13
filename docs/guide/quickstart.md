# quickstart

Open a terminal in any project directory and run:

```bash
kage
```

You land in the interactive TUI. The cursor sits in the input card at
the bottom of the screen, ready for your prompt.

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

## modal navigation

kage is modal in the vim sense.

| Mode    | Cursor look | Purpose                                        |
| ------- | ----------- | ---------------------------------------------- |
| Insert  | thin bar    | edit the input card (default on launch)        |
| Normal  | block       | navigate, scroll, fold, search                 |
| Visual  | block       | select text in the buffer or input card        |

`Esc` returns to Normal from anywhere. `i` enters Insert.

## the conversation buffer

In Normal mode with the buffer pane focused (`Ctrl+W` cycles panes):

- `j` / `k` scroll one line
- `G` snaps to the bottom and re-arms auto-follow
- `[` / `]` move focus between blocks
- `zM` folds every block, `zR` unfolds
- `/` starts a buffer search, `n` and `N` walk matches
- `y` yanks the current selection

## switching models mid-session

Type `:model <provider>:<model>` or open the model picker with
`Ctrl+P`. The next turn uses the new model; prior history is
preserved verbatim.

## recording and resuming

Every session is recorded to `~/.local/share/kage/sessions/<id>.jsonl`
unless you pass `--no-session`. Resume the most recent session with:

```bash
kage resume --last
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
kage -p "summarize this Cargo.toml" --model anthropic:claude-sonnet-4-6 < Cargo.toml
```

## next steps

- [Keybindings](/guide/keybindings) - the full key map
- [Commands](/guide/commands) - everything the `:` and `/` palettes accept
- [Plugins](/plugins/) - extend kage in Lua
