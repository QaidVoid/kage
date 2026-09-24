# commands

Commands are typed into the prompt with a leading `/`. The palette
lists every available command with its description and argument hint,
autocompletes arguments, and runs the handler on Enter.

## opening the command palette

Press `/` on an empty prompt to open the palette inline above the
input card. As you type, the list filters to matching commands.

- `Tab` extends to the longest common prefix; press Tab again to cycle.
- `Down` / `Up` walk the candidate list when the popup is open.
- `Enter` submits.
- `Esc` dismisses the popup first, then the palette.

Invalid input keeps the palette open with an inline error. Editing
clears the error. For example, `/quut` shows
`unknown command: quut (did you mean /quit?)`.

::: tip vim mode
With `editor = "vim"`, the same commands are also available from the
ex line: press `:` in Normal mode to open it on the status row. The
`:` line and the `/` palette share one parser, one autocomplete and
one set of handlers, so `:model <id>` and `/model <id>` do the same
thing.
:::

## built-in commands

| Command                   | What it does                                    |
| ------------------------- | ----------------------------------------------- |
| `/quit` / `/q`            | Exit the TUI                                    |
| `/cancel`                 | Cancel the current turn                         |
| `/model <id>`             | Switch the active model (`provider:model`)      |
| `/fold all`               | Fold every block                                |
| `/unfold all`             | Unfold every block                              |
| `/theme list`             | Show bundled and user themes                    |
| `/theme set <name>`       | Switch theme                                    |
| `/theme current`          | Print the active theme name                     |
| `/mouse [on\|off\|toggle]` | Control terminal mouse capture                |
| `/login [provider]`       | Add or update a provider credential: suspends the TUI, runs the interactive login, then refreshes the model list in place |
| `/permission [mode]` / `/perm` | Show or set the session permission mode (`allow\|ask\|deny\|default`) |
| `/help`                   | Open the keyboard reference overlay             |
| `/compact`                | Run a compaction pass right now                 |
| `/settings`               | Open the settings dialog (theme, model, mouse, thinking, and more) |
| `/tree`                   | Browse the session fork forest (resume, fork, delete) |
| `/clone`                  | Duplicate the session to a new id and continue in the clone |
| `/new`                    | Start a fresh empty session, keeping the current model |
| `/export [file]`          | Write the session transcript to a Markdown file (defaults to `<session-id-prefix>.md` in the working directory) |
| `/clear`                  | Clear the conversation buffer                   |
| `/noh`                    | Clear search highlighting (vim buffer search)   |
| `/keybindings` / `/keys`  | List active key bindings (config, plugin, reserved) |
| `/events`                 | List events plugins can hook with `kage.on`     |
| `/attach [path]` / `/img` | Attach an image to the next prompt: a file `path`, or the OS clipboard image when no path is given |

## plugin commands

Plugins register their own commands via `kage.register_command`. They
appear in the slash palette tagged `[plugin]` and accept the same
argument grammar as built-ins. See
[plugins / lua api](/plugins/api#commands).
