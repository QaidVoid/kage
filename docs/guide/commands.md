# commands

Commands are typed into the prompt with a leading `/`. The palette
lists every available command with its description and argument hint,
autocompletes arguments, and runs the handler on Enter.

## opening the command palette

Press `/` on an empty prompt to open the palette inline above the
input box. It lists the most used commands first, and as you type the
list filters to matching commands. The first row is always selected,
so `/` then `Enter` opens the model picker. Aliases such as `q` stay
out of the list until what you type matches one.

- `Down` / `Up` move the selection at once.
- `Tab` extends to the longest common prefix. Press Tab again to cycle.
- `Enter` runs the selected command.
- `Esc` dismisses the popup first, then the palette.

Invalid input keeps the palette open with an inline error. Editing
clears the error. For example, `/quut` shows
`unknown command: quut (did you mean /quit?)`.

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
| `/permission [mode]` / `/perm` | Show or set the session permission mode (`allow\|ask\|deny\|default`) |
| `/login [provider]`       | Add or update a provider credential: suspends the TUI, runs the interactive login, then refreshes the model list in place |
| `/settings`               | Open the settings dialog, which lists every option (see [settings](#settings)) |
| `/help`                   | Open the keyboard reference overlay             |
| `/theme list`             | Show bundled and user themes                    |
| `/theme set <name>`       | Switch theme                                    |
| `/theme current`          | Print the active theme name                     |
| `/export [file]`          | Write the session transcript to a Markdown file (defaults to `<session-id-prefix>.md` in the working directory) |
| `/tree`                   | Browse the session fork forest (resume, fork, delete). Agent sessions sit under their parent with an `agent: ` label. |
| `/clone`                  | Duplicate the session to a new id and continue in the clone |
| `/keybindings` / `/keys`  | List active key bindings (config, plugin, reserved) |
| `/attach [path]` / `/img` | Attach an image to the next prompt: a file `path`, or the OS clipboard image when no path is given |
| `/mouse [on\|off\|toggle]` | Control terminal mouse capture                |
| `/fold all`               | Fold every block                                |
| `/unfold all`             | Unfold every block                              |
| `/clear`                  | Clear the conversation buffer                   |
| `/noh`                    | Clear search highlighting                       |
| `/events`                 | List events plugins can hook with `kage.on`     |
| `/cancel`                 | Cancel the current turn                         |
| `/reload`                 | Reload `init.lua` and plugins now, without waiting for a file change. A toast reports the result. |
| `/quit` / `/q`            | Exit the TUI                                    |

## settings

`/settings` opens one list with a row per option: its name, its
value, and `set in init.lua` when Lua set it last. The selected
option's description shows below the list.

- `Up` / `Down` move the selection.
- `Left` / `Right` or `Space` cycle a choice, a boolean or the theme,
  and step a number. Typing digits edits a number.
- Every edit applies live as you make it.
- `Enter` or `Ctrl+S` saves the edits to your user `config.toml`.
- `Esc` or `Ctrl+C` restores the values the dialog opened with.

The dialog does not pick models or keys. Use `Ctrl+P` or `/model` for
the model and `?` or `/keybindings` for keys. It does not write
`default_model` either. Set that under `[provider]` in `config.toml`.

## plugin commands

Plugins register their own commands via `kage.register_command`. They
appear in the slash palette tagged `[plugin]` and accept the same
argument grammar as built-ins. See
[plugins / lua api](/plugins/api#commands).
