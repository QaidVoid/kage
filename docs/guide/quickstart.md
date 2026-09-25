# quickstart

Open a terminal in any project directory and run:

```bash
kage
```

The first launch connects a provider: kage opens a provider picker and
prompts for an API key. Env vars like `ANTHROPIC_API_KEY` also work, so
you can set one and restart instead. After connecting, you land in the
interactive TUI. The cursor sits in the input box at the bottom of
the screen, ready for your prompt.

Above the input, the start card shows the model, the working
directory, the permission summary and the thinking level, each with
the key or command that changes it. The thinking level reads
`high (auto)` until you pick one: kage uses high, or the nearest level
the model accepts, and `model default` for a model with no thinking
setting. Below them come your three most
recent sessions (`ctrl+s` opens the session picker to resume one),
startup notices such as a missing or expiring credential with the
`/login` command that fixes it, and a tip. The card goes away once
the conversation starts.

## a first turn

Type a question or a task and press `enter`:

```
explain what this repository does in one paragraph
```

The assistant streams its response into the conversation buffer above
the input box. Each tool call is one row that starts with what it
did, such as `Read Cargo.toml`, `Ran cargo test` or
`Edited src/lib.rs (+1 -1)`, with its duration on the right. A shell
command shows the tail of its output and an edit shows its diff. A
run of reads, listings and searches folds into one `Explored` row.
`ctrl+o` expands the focused row. Thinking shows as `Thinking (2s)`
while it streams, then folds to `Thought for 3s`.

While kage works, the row above the input says what it is doing and
for how long. Interrupt the run with `esc` or `ctrl+c` on an empty
prompt. Quit with `ctrl+q`, by typing `/quit`, or with `ctrl+c` twice
on an idle, empty prompt.

## finding your way around

The empty prompt shows `Ask kage anything`. The left side of the
footer below it always says what the next keys do, such as
`? for shortcuts` and `/ for commands` on an idle, empty prompt. While
a picker, dialog or popup is open, it shows that layer's keys instead.
The right side shows the model, the permission mode when you changed
it for the session, the context use, the tokens and the cost. These
prefixes work on an empty prompt:

- `/` opens the command palette, with the most used commands first.
  `/settings` lists every option. Edits apply live, `enter` saves
  them to `config.toml`, and `esc` undoes them.
- `!` switches to shell mode (see below).
- `?` opens the keyboard reference overlay. `/help` opens it too.

Type `@` anywhere in the prompt to complete a file path under the
working directory. `enter` or `tab` accepts the highlighted path. This
only inserts the text, such as `@src/main.rs`. The file is not
attached, so the model sees the path and reads the file with its tools
when it needs to.

## editing and navigation

The prompt is always editable: type, then press `enter` to send.
Press `shift+enter` to insert a newline instead (`alt+enter` also
works, for terminals that do not report `shift+enter`). Readline keys
work (`ctrl+a`/`ctrl+e` line start/end, `ctrl+u`/`ctrl+k` kill,
`ctrl+/` undo), and `up`/`down` walk your prompt history. `esc` clears
the draft, and `up` brings it back. `ctrl+g` opens the whole draft in
an external editor (`$VISUAL` or `$EDITOR`) for longer prompts.
`shift+tab` cycles through the thinking levels the model accepts.

Prefer vim? Set `editor = "vim"` under `[ui]` in config.toml (or
toggle it in `/settings`) to get normal/insert/visual modes with
motions, operators, and registers. In vim mode, `esc` enters Normal
mode, `:` opens the command line there, and `?` opens the keyboard
reference.

## sending while kage works

You can keep typing during a run. `enter` steers: the prompt joins
the running turn after the current tool call. `tab` queues: the prompt
waits and starts a new run when the current one ends. Prompts waiting
for delivery show above the input, marked `after the current tool
call` or `when this run ends`, until kage takes them. See
[keybindings](/guide/keybindings#sending-while-kage-works).

## agents

For work that needs many tool calls, such as mapping a large codebase
or running a test suite, kage can start agents: separate sessions that
each work on one task and reply with the result. Several agents run
at the same time. Each shows as a live `Agent` row, the working row
counts them, and a list above the input keeps the running ones in
view. Click a row in that list, or pick one in the agents overlay
(`ctrl+t`), to open that agent, read its transcript and steer it.
`esc` on an empty prompt comes back. Interrupting the
main run stops its agents too. See [agents](/guide/agents), including
how to write your own.

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

Press a number or `y`, `s`, `a`, `n`, `t`, or move with `up`/`down`
and press `enter`. `esc` answers no. Option 5 lets you type what kage
should do instead. Your draft stays as it was and returns after the
last approval. See [permissions](/guide/permissions#approving-in-the-tui).

## the conversation buffer

- `pageup` / `pagedown` scroll ten lines
- `ctrl+up` / `ctrl+down` scroll one line
- `ctrl+p` opens the model picker, `ctrl+s` the session picker
- `alt+p` / `alt+n` focus the previous / next block
- `ctrl+o` folds or unfolds the focused block
- `f3` opens the message jump picker
- `ctrl+f` searches the conversation
- the mouse wheel scrolls, and dragging selects and copies text

In vim mode, normal-mode keys (`j`/`k`, `G`, `[`/`]`, `zM`/`zR`, `y`)
work on the buffer once you press `esc`. `/` in Normal mode starts a
buffer search, `n` and `N` walk matches, and `ctrl+w` in Normal mode
cycles focus between the input and buffer panes.

## running shell commands

Type `!` on an empty prompt to switch to shell mode (the prompt glyph
becomes `!` and the placeholder reads `Run a shell command (backspace
leaves shell mode)`). `enter` runs the line with `bash` in the session
working directory, and the prompt returns to normal. The command shows
as its own block with the tail of its output while it runs. `esc`
stops it and kills the process. On an idle session, prompts you send
meanwhile wait until the command ends.

When the command ends, its output (up to 8 KB) joins the conversation,
so the model sees it on the next turn and follow-up prompts can
reference it. The command and its output are recorded in the session,
so they are still there when you resume it. Resuming never runs the
command again. `backspace` or `esc` on the empty shell prompt leaves
shell mode without running anything.

## switching models mid-session

Type `/model <provider>:<model>`, or open the model picker with
`ctrl+p` or a bare `/model`. The next turn uses the new model, and
the conversation so far carries over.

## leaving kage

When you quit, kage prints the conversation into your terminal as
plain text, then `session saved to <path>` and the command that
resumes it: ``resume it with `kage resume <id>` ``. Set
`transcript_on_exit` to `last` to print only from your last prompt on,
or to `none` to print only the session lines (see
[configuration](/guide/config)).

## recording and resuming

Every session is recorded to `~/.local/share/kage/sessions/<id>.jsonl`
unless you pass `--no-session`. Resume one in the TUI by id (or a
unique prefix), or the most recent one with `--last`:

```bash
kage resume <id>
kage resume --last
```

Inside the TUI, `ctrl+s` opens the session picker instead. Add `-p` to
send one prompt in print mode without opening the TUI:

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
- [Agents](/guide/agents): delegate tasks to agents and write your own
- [Plugins](/plugins/): extend kage in Lua
