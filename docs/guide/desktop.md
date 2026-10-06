# desktop and web client

`kage-desktop` and the browser client are one program. The desktop
app starts `kage rpc` and shows its sessions; the browser client
attaches to the `kage serve` it was loaded from. Both read and write
the same engine state, so a session started in one shows in the other
and in the TUI.

- To install the desktop app, see [desktop client](/guide/install#desktop-client).
- To serve the browser client, see [web client](/editors/remote#web-client).

## settings

Settings opens from the gear at the bottom of the sidebar. Its pages:

| Page | What it holds |
| --- | --- |
| General | Theme, input, chat and session preferences, below |
| Agent & Sessions | Thinking default, compaction and the subagent and swarm limits, written to the user `config.toml` |
| Model Providers | Built-in, custom and ACP providers, and adding one |
| MCP Servers | The configured MCP servers with their live status, and editing them |
| Permissions | The permission mode and the allow, ask and deny rules |
| Plugins | The installed Lua plugins, and installing one |
| Connection | How the client reaches the engine, and the `kage` binary the desktop app runs |
| Keyboard | Every shortcut, with the vim keys when vim mode is on |
| Archived Sessions | Sessions archived from their menu, to restore |
| About | The client and engine versions, the protocol, and the release check |

The About page compares the engine and this client against the public
releases at most once a day; **Check now** re-runs it, and **Releases**
opens the releases page in your browser.

The pages that edit engine options write the user config, as
[configuration](/guide/config) describes. A project config that sets
the same key still wins, and changes apply to sessions started
afterwards.

## general

The General page holds the client's own preferences.

### appearance

The theme cards pick a bundled or user theme, or System, which
follows the desktop's light or dark mode. **System on a dark desktop**
and **System on a light desktop** pick the pair System draws; they set
`[ui] theme_dark` and `theme_light`, which the TUI reads too. See
[themes](/guide/themes) for writing a theme the client draws.

### input

| Setting | Default | What it does |
| --- | --- | --- |
| Vim mode | off | Normal-mode motions over the transcript (`j` `k` `gg` `G` `za` `/` `n`), a `:` command line and a modeline. Esc leaves the composer. |
| Enter sends | on | Off, Enter adds a newline and Ctrl+Enter sends. |

### chat

| Setting | Default | What it does |
| --- | --- | --- |
| Smooth streaming | on | Shows a reply that arrives in bursts at a steady pace. |
| Turn timeline rail | on | A minimap beside the transcript with ticks for turns, edits, approvals, failures and swarms. |
| Context gauge | on | The context ring in the composer opens a gauge with a Compact now action. |
| Swarm constellation | on | Swarm cards draw one star per worker, lit by its state. |

Some providers send a reply in bursts: nothing for a few seconds, then
hundreds of characters at once. Smooth streaming spreads each burst
over the time the bursts have been arriving apart, so the text keeps
moving between them. A provider that already streams smoothly shows
at most a quarter of a second behind. Turn it off to show every
character the moment it arrives.

### sessions

| Setting | Default | What it does |
| --- | --- | --- |
| Group sessions by project | on | Lists sessions under their project folder in the sidebar. |
| Ask before enabling swarm | on | Confirms before swarm mode turns on. |

## where preferences live

The General page's preferences, pinned and archived sessions, starred
models and the last session's model choices are the client's own, not
engine state, so they never reach the server. The desktop app keeps
them in `desktop.json` in the kage config directory
(`$XDG_CONFIG_HOME/kage`, else `~/.config/kage`, else `%APPDATA%\kage`).
The browser client keeps them in its `localStorage`, under
`kage.desktop`, so each browser has its own.

The two system theme picks are the exception: they live in the
engine's config, so every client and the TUI share them.
