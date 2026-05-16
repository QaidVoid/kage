# commands

kage exposes two surfaces over the same command registry: a vim-style
ex line (`:`) and an inline slash palette (`/`). Both share the same
parser, the same per-arg autocomplete, and the same handlers; pick
whichever feels right for the moment.

## opening the command line

In Normal mode, press `:` to open the ex line on the status row.

In Insert mode with an empty input card, press `/` to open the
slash palette inline above the input card. The palette lists every
available command with its description and argument hint.

In either surface:

- `Tab` extends to the longest common prefix; press Tab again to cycle.
- `Down` / `Up` walk the candidate list when the popup is open.
- `Enter` submits.
- `Esc` dismisses the popup first, then the line.

Invalid input keeps the line open with an inline error below it.
Editing clears the error.

## built-in commands

| Command                  | What it does                                    |
| ------------------------ | ----------------------------------------------- |
| `:quit` / `:q`           | Exit the TUI                                    |
| `:cancel`                | Cancel the current turn                         |
| `:model <provider:id>`   | Switch the active model                         |
| `:fold all`              | Fold every block                                |
| `:unfold all`            | Unfold every block                              |
| `:theme list`            | Show bundled and user themes                    |
| `:theme set <name>`      | Switch theme                                    |
| `:theme current`         | Print the active theme name                     |
| `:mouse on|off|toggle`   | Control terminal mouse capture                  |
| `:help`                  | List every command in a readable form           |
| `:keybindings` / `:keys` | List active key bindings (config/plugin/reserved) |
| `:events`                | List events plugins can hook with `kage.on`     |
| `:compact`               | Run a compaction pass right now                 |
| `:settings`              | Open the settings dialog (theme/model/mouse/..) |
| `:tree`                  | Browse the session fork forest                  |
| `:clear`                 | Clear the conversation buffer                   |

## plugin commands

Plugins register their own commands via `kage.register_command`. They
appear in the slash palette tagged `[plugin]` and accept the same
argument grammar as built-ins. See
[plugins / lua api](/plugins/api#commands).
