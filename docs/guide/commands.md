# commands

Commands are typed into the prompt with a leading `/`. The palette
lists every available command with its description and argument hint,
autocompletes arguments, and runs the handler on `enter`.

## opening the command palette

Press `/` on an empty prompt to open the palette inline above the
input box. It lists the most used commands first, and as you type the
list filters to matching commands. The first row is always selected,
so `/` then `enter` opens the model picker. Aliases such as `q` stay
out of the list until what you type matches one.

- `down` / `up` move the selection at once.
- `tab` extends to the longest common prefix. Press `tab` again to
  cycle.
- `enter` runs the selected command.
- `esc` dismisses the popup first, then the palette. `ctrl+c` closes
  the palette at once.

Invalid input keeps the palette open with an inline error. Editing
clears the error. For example, `/quut` shows
`unknown command: quut (did you mean /quit?)`. A command whose first
argument is required does not run without it: `/theme set` then
`enter` adds a space for the argument and shows
``missing required argument `name` ``.

Commands that answer with text, such as `/theme list`,
`/permission` without a mode, `/keybindings` and `/events`, write it
into the conversation as a block. Errors from a command that ran
show there too.

::: tip vim mode
With `editor = "vim"`, the same commands are also available from the
ex line: press `:` in Normal mode to open it on the bottom row. The
`:` line and the `/` palette share one parser, one autocomplete and
one set of handlers, so `:model <id>` and `/model <id>` do the same
thing.
:::

## built-in commands

The table follows the palette's order.

| Command                   | What it does                                    |
| ------------------------- | ----------------------------------------------- |
| `/model [id]`             | Switch the active model (`provider:model`). Without an id, open the model picker. |
| `/new`                    | Start a fresh empty session, keeping the current model |
| `/compact`                | Run a compaction pass right now                 |
| `/agents`                 | Open the agents overlay: every agent of the session, live or finished (see [agents](/guide/agents#the-agents-overlay)) |
| `/permission [mode]` / `/perm` | Without a mode, show the session permission mode. `ask` or `deny` override the configured rules for this session, and `allow` or `default` return to them (see [permissions](/guide/permissions)) |
| `/login [provider]`       | Add or update a provider credential: suspends the TUI, runs the interactive login, then refreshes the model list in place |
| `/mcp`                    | Open the MCP servers picker: status per server, `enter` restarts a server or logs in to one that needs it (see [mcp](/guide/mcp#the-mcp-picker)) |
| `/mcp restart <server>`   | Restart an MCP server: now when idle, else when the next run starts |
| `/mcp login <server>`     | Log in to a remote MCP server with OAuth: suspends the TUI, runs the browser login, then reconnects the server |
| `/settings`               | Open the settings dialog, which lists every option (see [settings](#settings)) |
| `/help`                   | Open the keyboard reference overlay             |
| `/theme list`             | List bundled and user themes, `*` marking the active one |
| `/theme set <name>`       | Switch to a bundled or user theme for this session |
| `/theme current`          | Show the active theme name                      |
| `/export [file]`          | Write the session transcript to a Markdown file (defaults to `<session-id-prefix>.md` in the working directory) |
| `/tree`                   | Browse the session fork forest (resume, fork, delete). Agent sessions sit under their parent with an `agent: ` label. |
| `/clone`                  | Duplicate the session to a new id and continue in the clone |
| `/keybindings` / `/keys`  | List active key mappings per mode with their owners |
| `/attach [path]` / `/img` | Attach a png, jpeg, gif or webp image to the next prompt: a file `path`, or the OS clipboard image when no path is given. A toast warns when the model does not accept images. To include a text file, mention it as `@path` in the prompt instead |
| `/mouse [on\|off\|toggle]` | Control terminal mouse capture                |
| `/fold all`               | Fold every block                                |
| `/unfold all`             | Unfold every block                              |
| `/clear`                  | Clear the conversation buffer                   |
| `/noh`                    | Clear search highlighting                       |
| `/events`                 | List events plugins can hook with `kage.on`     |
| `/cancel`                 | Cancel the current turn                         |
| `/reload`                 | Reload `init.lua` and plugins now, without waiting for a file change. A toast reports the result, and each error shows in the conversation. |
| `/quit` / `/q`            | Exit the TUI                                    |

## settings

`/settings` opens one list with a row per option: its name, its
value, and `set in init.lua` when Lua set it last. The selected
option's description shows below the list.

- `up` / `down` move the selection.
- `left` / `right` or `space` cycle a choice, a boolean or the theme,
  and step a number. Typing digits edits a number. On the `leader`
  row, press the key you want.
- Every edit applies live as you make it.
- `enter` or `ctrl+s` saves the edits to your user `config.toml`,
  changing only those keys. Comments and every other line stay as
  written.
- `esc` or `ctrl+c` restores the values the dialog opened with.

The dialog does not pick models or edit key bindings. Use `ctrl+p` or
`/model` for the model and `?` or `/keybindings` for bindings. It
does not write `default_model` either. Set that under `[provider]` in
`config.toml`.

## mcp prompt commands

Each prompt of a connected MCP server is a command named
`/<server>:<prompt>`, such as `/everything:complex_prompt`. The palette
tags it `[mcp]` and shows its arguments as the hint, `<name>` for a
required one and `[name]` for an optional one. Arguments are
whitespace separated in declared order, and the last one takes the
rest of the line. The prompt's messages become your prompt. A prompt
whose name a built-in or plugin command takes is not listed. See
[mcp](/guide/mcp#prompts-as-commands).

## plugin commands

Plugins register their own commands via `kage.register_command`. They
appear in the slash palette tagged `[plugin]` and accept the same
argument grammar as built-ins. See
[plugins / lua api](/plugins/api#commands).
