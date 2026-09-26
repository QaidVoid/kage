# configuration

kage reads its configuration from `~/.config/kage/config.toml`. The file is
optional, and sane defaults work without it. `kage init` writes a
starter one. A config that does not load stops kage with the error
instead of running on defaults, and `kage doctor` names the file to
fix.

The TUI also runs `~/.config/kage/init.lua` after this file, so
anything set there wins. The `[ui]`, `[loop]` and `[agents]` keys
that are options and the `[keybindings]` table feed the same settings `init.lua` sets.
See [lua config](/guide/lua-config).

Keys are grouped into tables:

```toml
[provider]
# model used when -m/--model is absent. it beats the last-used model
# but only while its provider has credentials. otherwise kage uses the
# last model you ran, then the first provider with credentials.
default_model = "anthropic:claude-sonnet-4-6"

[ui]
# bundled theme name, or a user theme under ~/.config/kage/themes/<name>.toml.
theme = "default"
# capture terminal mouse events. Toggle at runtime with /mouse.
mouse = true
# prompt editing style: "modeless" or "vim".
editor = "modeless"
# input box sizing (content rows, not counting its two rules).
# the box grows with what you type, from input_min_lines up to
# input_max_lines, then scrolls internally. raise the max for a
# bigger composing area. min floored at 1, max capped at 64.
input_min_lines = 1
input_max_lines = 8
# default thinking level for new sessions: off, minimal, low,
# medium, high, or xhigh. left unset, kage uses high, or the nearest
# level the model accepts, and shows it as "high (auto)". shift+tab
# still cycles it per session.
# thinking_level = "medium"
# what prints to the terminal after you quit the TUI: "full" (the
# whole conversation as plain text), "last" (from your last prompt
# on) or "none". the session file path follows when one was recorded.
transcript_on_exit = "full"

[plugins]
# override the plugin directory (default ~/.config/kage/plugins/).
# `~` expands to home, and relative paths resolve against ~/.config/kage.
# dir = "/path/to/plugins"
# if non-empty, only these plugin file stems load.
enabled = []

[shell]
# Program the shell tool runs commands with: `bash` (the default on
# Linux and macOS), `pwsh` (the default on Windows), `fish`, `zsh`, or
# an absolute path. The tool description and the prompt's environment
# block name it, so the model writes commands in that shell's syntax.
# User `!` commands run with the same program. Known Windows shells
# (powershell, pwsh, cmd) get their own command flag; every other
# program is driven with `-c`.
# program = "fish"
# Glob patterns of environment variables to hide from shell commands
# (case-sensitive, matched against the whole name, so "*_TOKEN" covers
# GITHUB_TOKEN). Empty leaves the environment untouched.
# scrub_env = ["*_TOKEN", "*_SECRET", "*_KEY", "*_PASSWORD"]
scrub_env = []

[keybindings]
# the key <leader> expands to in bindings: one key, default a backslash.
# leader = "<C-x>"
# ms a key that starts a longer mapping waits for more keys.
timeoutlen = 1000
# key -> command line, or `action:<Name>` for a built-in action
# (see the keybindings guide). Every binding maps in mode g.
# bindings = { "f6" = "theme set tokyo-night", "<leader>s" = "settings" }

[loop]
# compact older history once the prompt fills this fraction of the
# model context window (0.0 to 1.0). Applies to the next session, and
# the /settings dialog edits it too.
compaction_threshold = 0.8

[agents]
# how deep agents may nest (0 to 3). 0 removes the agent tool, and 1
# lets only the main session start agents.
max_depth = 1
# how many agents run at once (1 to 16). further agents wait their turn.
max_running = 4

[permissions]
# tool permission rules. built-in tools are allowed unless configured
# and MCP tools ask. see the permissions guide for the full reference.
# confine_paths = false
# [permissions.tools.shell]
# default = "ask"
# allow = ["git *"]
# deny = ["rm -rf *"]
# [permissions.mcp]
# github = "allow"   # action for every tool of one MCP server

[mcp]
# let MCP servers ask your default model for completions. default false.
allow_sampling = false
# one table per server. see the mcp guide for the full reference.
# [mcp.servers.linear]
# url = "https://mcp.linear.app/mcp"
# optional OAuth settings for a server that needs a login:
# [mcp.servers.linear.oauth]
# client_id = "kage-4f2c"   # pre-registered client, skips registration
# scope = "read write"      # overrides the scope the server advertises
```

Every table and key is optional. Omitted values fall back to the
defaults shown above.

When kage writes this file itself (saving `/settings`, or answering
"always allow" to a permission prompt), it changes only the keys
involved. Your comments, key order and every other line stay as you
wrote them. See [permissions](/guide/permissions) for the
rules reference, [agents](/guide/agents#limits) for the `[agents]`
limits, and [mcp](/guide/mcp) for MCP servers and OAuth logins.

## layering

Configuration is merged lowest-to-highest precedence:

1. built-in defaults
2. `~/.config/kage/config.toml` (user)
3. `<workdir>/.kage/config.toml` (project-local, commit it to share
   team settings)
4. `KAGE_*` environment variables

Env vars use `KAGE_` with `__` for nesting, e.g.
`KAGE_UI__THEME=catppuccin-mocha` overrides the `[ui].theme` key.

In the TUI, `init.lua` runs after all of these. An option it sets, such
as `kage.opt.theme`, wins over every layer above.

## project config and trust

A project can start processes and loosen your tool rules, so these
parts of it only apply once you trust the project:

- `[mcp]` in `.kage/config.toml` (servers, their `oauth` tables and
  `allow_sampling`)
- `[permissions]` in `.kage/config.toml` (including
  `[permissions.mcp]`)
- `[plugins.capabilities]` in `.kage/config.toml`
- the agent definitions in `.kage/agents/` (see
  [agents](/guide/agents#project-agents-and-trust))

All of them share one trust decision. Every other project key, such
as `[ui]`, `[loop]` or `[agents]`, applies without trust. Provider
settings, `[acp.agents]`, `provider.default_model` and `plugins.dir`
are only read from your user config.

When the TUI starts in a project that sets any of these, it lists what
the project asks for (server commands and URLs, sampling, capability
grants, permission changes, project agents) and asks `Trust this
project? [y/N]`. Answering yes records the trust. Any other answer
starts kage with those tables and agents ignored.

Print mode, `kage rpc` and `kage mcp serve` cannot ask. They print one
warning on stderr and ignore the tables and agents. Run `kage trust`
in the project directory to allow them, and `kage trust --revoke` to
take the trust back. An editor driving `kage rpc` needs `kage trust` once per
project.

Trust covers the values as they are when you approve them. Editing a
server command, a permission rule or a capability grant, or adding,
removing or editing a project agent file, makes kage ask again.
Reordering keys does not. The whole table is ignored while untrusted,
even settings that only tighten your rules, such as a
project that only adds `deny` patterns. `kage doctor` reports an
untrusted project file.

Trusted projects are recorded in `~/.local/state/kage/trust.json`,
keyed by the project directory.

## environment variables

API keys are read from environment variables:

| Variable                | Provider                                      |
| ----------------------- | --------------------------------------------- |
| `ANTHROPIC_API_KEY`     | Anthropic Claude                              |
| `OPENAI_API_KEY`        | OpenAI                                        |
| `GEMINI_API_KEY`        | Google Gemini                                 |
| `ZAI_API_KEY`           | Z.AI                                          |
| `ZAI_CODING_API_KEY`    | Z.AI and Zhipu AI coding plans                |
| `DEEPSEEK_API_KEY`      | DeepSeek                                      |
| `GROQ_API_KEY`          | Groq                                          |
| `MISTRAL_API_KEY`       | Mistral                                       |
| `CEREBRAS_API_KEY`      | Cerebras                                      |
| `XAI_API_KEY`           | xAI                                           |
| `OPENROUTER_API_KEY`    | OpenRouter                                    |
| `FIREWORKS_API_KEY`     | Fireworks AI                                  |
| `MOONSHOT_API_KEY`      | Moonshot                                      |
| `KIMI_API_KEY`          | Kimi for Coding                               |
| `XIAOMI_API_KEY`        | Xiaomi / Xiaomi Token Plan                    |

The model id picks the provider. Without `-m`, kage uses
`provider.default_model` when its provider has credentials, then the
last model you ran, then the preferred model of the first provider
with credentials. When the configured `default_model` has no
credentials, the TUI starts on the fallback and says which model it
used instead.

See [providers](/guide/providers) for the full provider list, custom
endpoints, and per-provider overrides (base URL, headers, key env var).

## directories

| Path                                | Contents                                                       |
| ----------------------------------- | -------------------------------------------------------------- |
| `~/.config/kage/config.toml`        | user config                                                    |
| `~/.config/kage/init.lua`           | trusted Lua config, TUI only ([lua config](/guide/lua-config)) |
| `~/.config/kage/lua/`               | modules `init.lua` can `require`                               |
| `<workdir>/.kage/config.toml`       | project-local config overlay                                   |
| `~/.config/kage/agents/`            | agent definition `.md` files ([agents](/guide/agents))         |
| `<workdir>/.kage/agents/`           | project agent definitions, loaded once the project is trusted  |
| `~/.config/kage/themes/`            | user theme TOML files                                          |
| `~/.config/kage/plugins/`           | Lua plugin scripts                                             |
| `~/.config/kage/skills/`            | `SKILL.md` skill directories                                   |
| `~/.config/kage/templates/`         | prompt template `.md` files                                    |
| `~/.local/share/kage/sessions/`     | append-only session JSONL files                                |
| `~/.local/share/kage/auth.json`     | saved provider credentials (`0600`)                            |
| `~/.local/share/kage/mcp-auth.json` | OAuth tokens for remote MCP servers (`0600`)                   |
| `~/.local/share/kage/plugin-state/` | per-plugin `kage.store` JSON files                             |
| `~/.local/state/kage/`              | session state (`state.json`) and input history (`history.txt`) |
| `~/.local/state/kage/trust.json`    | trusted project configs                                        |
| `~/.cache/kage/models.json`         | model catalog written by `kage models refresh`                 |

`XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` /
`XDG_CACHE_HOME` override the `~/.config`, `~/.local/share`,
`~/.local/state` and `~/.cache` roots. `kage doctor` prints the four
directories it resolved. Skills and
templates are also discovered
under the project-local `<workdir>/.kage/skills/` and
`<workdir>/.kage/templates/`, plus any directory a plugin
contributes via the `resources_discover` event.

## skills and templates

A skill is a directory with a `SKILL.md` (YAML frontmatter `name`,
`description`, optional `disable_model_invocation`) whose body is
injected into the system prompt so the agent always sees it.

A prompt template is a single `.md` file (frontmatter `name`,
`description`, `argument-hint`) whose body becomes a user message
with positional substitution. `$1`, `$2` and so on are single
arguments, `$@` or `$ARGUMENTS` is all of them joined, and `${@:N:L}`
is a bash-style slice. Drop files in the directories above. No Lua is
required.
