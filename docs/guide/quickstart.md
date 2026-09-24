# quickstart

Open a terminal in any project directory and run:

```bash
kage
```

The first launch connects a provider: kage opens a provider picker and
prompts for an API key (env vars like `ANTHROPIC_API_KEY` also work;
set one and restart instead). After connecting, you land in the
interactive TUI. The cursor sits in the input box at the bottom of
the screen, ready for your prompt.

Above the input, the start card shows the model, the working
directory, the permission summary and the thinking level, each with
the key or command that changes it. Below them come your three most
recent sessions (`Ctrl+S` opens the session picker to resume one),
startup notices such as a missing or expiring credential with the
`/login` command that fixes it, and a tip. The card goes away once
the conversation starts.

## a first turn

Type a question or a task and press Enter:

```
explain what this repository does in one paragraph
```

The assistant streams its response into the conversation buffer above
the input box. Each tool call is one row that starts with what it
did, such as `Read Cargo.toml`, `Ran cargo test` or
`Edited src/lib.rs (+1 -1)`, with its duration on the right. A shell
command shows the tail of its output and an edit shows its diff. A
run of reads, listings and searches folds into one `Explored` row.
`Ctrl+O` expands the focused row. Thinking shows as `Thinking (2s)`
while it streams, then folds to `Thought for 3s`.

While kage works, the row above the input says what it is doing and
for how long. Interrupt the run with `Esc` or Ctrl+C on an empty
prompt. Quit with Ctrl+Q, by typing `/quit`, or with Ctrl+C twice on
an idle, empty prompt.

## finding your way around

The empty prompt shows `Ask kage anything`. The left side of the
footer below it always says what the next keys do, such as
`? for shortcuts` and `/ for commands` on an idle, empty prompt. The
right side shows the model, the context use, the tokens and the cost.
These prefixes work on an empty prompt:

- `/` opens the command palette, with the most used commands first.
  `/settings` lists every option. Edits apply live, `Enter` saves
  them to `config.toml`, and `Esc` undoes them.
- `!` switches to shell mode (see below).
- `?` opens the keyboard reference overlay. `/help` opens it too.

## editing and navigation

The prompt is always editable: type, then press Enter to send. Press
`Shift+Enter` to insert a newline instead (`Alt+Enter` also works, for
terminals that do not report Shift+Enter). Readline keys work
(`Ctrl+A`/`Ctrl+E` line start/end, `Ctrl+U`/`Ctrl+K` kill, `Ctrl+/`
undo), and `Up`/`Down` walk your prompt history. `Esc` clears the
draft, and `Up` brings it back. `Ctrl+G` opens the whole draft in an
external editor (`$VISUAL` or `$EDITOR`) for longer prompts.
`Shift+Tab` cycles the thinking level.

Prefer vim? Set `editor = "vim"` under `[ui]` in config.toml (or
toggle it in `/settings`) to get normal/insert/visual modes with
motions, operators, and registers. In vim mode, `Esc` enters Normal
mode, `:` opens the command line there, and `?` opens the keyboard
reference.

## sending while kage works

You can keep typing during a run. `Enter` steers: the prompt joins the
running turn after the current tool call. `Tab` queues: the prompt
waits and starts a new run when the current one ends. Prompts waiting
for delivery show above the input until kage takes them. See
[keybindings](/guide/keybindings#sending-while-kage-works).

## approving tool calls

By default built-in tools run without asking. When a call needs your
approval (MCP tools, `/permission ask`, or an `ask` rule), a panel
replaces the input box:

```text
-- Run this command? ---------------------------------------------
   $ cargo test -p parser

 > 1. Yes
   2. Yes, and allow bash for the rest of this session
   3. Yes, and always allow bash (saved to config.toml)
   4. No
   5. No, and tell kage what to do instead
------------------------------------------------------------------
```

Press a number or `y`, `s`, `a`, `n`, `t`, or move with `Up`/`Down`
and press `Enter`. `Esc` answers no. Option 5 lets you type what kage
should do instead. Your draft stays as it was and returns after the
last approval. See [permissions](/guide/permissions#approving-in-the-tui).

## the conversation buffer

- `PageUp` / `PageDown` scroll ten lines
- `Ctrl+Up` / `Ctrl+Down` scroll one line
- `Ctrl+P` opens the model picker, `Ctrl+S` the session picker
- `Alt+P` / `Alt+N` focus the previous / next block
- `Ctrl+O` folds or unfolds the focused block
- `F3` opens the message jump picker
- `Ctrl+F` searches the conversation
- the mouse wheel scrolls, and dragging selects and copies text

In vim mode, normal-mode keys (`j`/`k`, `G`, `[`/`]`, `zM`/`zR`, `y`)
work on the buffer once you press `Esc`. `/` in Normal mode starts a
buffer search, `n` and `N` walk matches, and `Ctrl+W` in Normal mode
cycles focus between the input and buffer panes.

## running shell commands

Type `!` on an empty prompt to switch to shell mode (the prompt glyph
becomes `!` and the placeholder reads `Run a shell command (Backspace
leaves shell mode)`). Enter runs the line with `sh` in the session
working directory. The output lands in the conversation as its own
block and the model sees a summary on the next turn, so follow-up
prompts can reference it. After the command runs, the prompt returns
to normal. `Backspace` on the empty shell prompt leaves shell mode
without running anything.

Shell runs are conversation-only: they are not re-applied when you
resume a recorded session.

## switching models mid-session

Type `/model <provider>:<model>`, or open the model picker with
`Ctrl+P` or a bare `/model`. The next turn uses the new model, and
prior history is preserved verbatim.

## leaving kage

When you quit, kage prints the conversation into your terminal as
plain text, then `session saved to <path>`. Set `transcript_on_exit`
to `last` to print only from your last prompt on, or to `none` to
print only the session path (see [configuration](/guide/config)).

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
