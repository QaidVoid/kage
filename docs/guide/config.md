# configuration

kage reads its configuration from `~/.config/kage/config.toml`. The file is
optional; sane defaults work without it. `kage init` writes a
starter one.

Keys are grouped into tables:

```toml
[provider]
# model used when -m/--model is absent. beats the last-used model
# memory; must name a provider whose credentials are available, or
# the saved last model / built-in order is used instead.
default_model = "anthropic:claude-sonnet-4-6"

[ui]
# bundled theme name, or a user theme under ~/.config/kage/themes/<name>.toml.
theme = "default"
# capture terminal mouse events. Toggle at runtime with :mouse.
mouse = true
# input card sizing (content rows, before the 2-row border chrome).
# the card grows with what you type, from input_min_lines up to
# input_max_lines, then scrolls internally. raise the max for a
# bigger composing area. min floored at 1, max capped at 64.
input_min_lines = 1
input_max_lines = 8
# default thinking level for new sessions: off, minimal, low,
# medium, high, or xhigh. shift+tab still cycles it per session.
# thinking_level = "medium"

[plugins]
# override the plugin directory (default ~/.config/kage/plugins/).
# `~` expands to home; relative paths resolve against ~/.config/kage.
# dir = "/path/to/plugins"
# if non-empty, only these plugin file stems load.
enabled = []

[sandbox]
# "local" (default in 0.1), "bubblewrap", or "sandbox-exec".
backend = "local"
# silence the "running unsandboxed" startup warning.
suppress_warning = false

[keybindings]
# chord -> builtin command name, or an `action:<Name>` builtin
# input action (see the keybindings guide).
# bindings = { "ctrl+r" = "cancel" }

[loop]
# compact older history once the prompt fills this fraction of the
# model context window (0.0-1.0). Applies on next launch; also
# editable in the :settings dialog.
compaction_threshold = 0.8

[permissions]
# tool permission rules. built-in tools are allowed unless configured
# and MCP tools ask; see the permissions guide for the full reference.
# confine_paths = false
# [permissions.tools.bash]
# default = "ask"
# allow = ["git *"]
# deny = ["rm -rf *"]
# [permissions.mcp]
# github = "allow"   # action for every tool of one MCP server
```

Every table and key is optional; omitted values fall back to the
defaults shown above. See [permissions](/guide/permissions) for the
rules reference.

## layering

Configuration is merged lowest-to-highest precedence:

1. built-in defaults
2. `~/.config/kage/config.toml` (user)
3. `<workdir>/.kage/config.toml` (project-local; commit it to share
   team settings)
4. `KAGE_*` environment variables

Env vars use `KAGE_` with `__` for nesting, e.g.
`KAGE_UI__THEME=catppuccin-mocha` overrides the `[ui].theme` key.

## project config and trust

A project file can start processes and loosen your tool rules, so
three of its tables only apply once you trust the project:

- `[mcp]` (servers and `allow_sampling`)
- `[permissions]` (including `[permissions.mcp]`)
- `[plugins.capabilities]`

Every other project key, such as `[ui]` or `[loop]`, applies without
trust. Provider settings, `[acp.agents]`, `provider.default_model`
and `plugins.dir` are only read from your user config.

When the TUI starts in a project whose file sets any of these tables,
it lists what the file asks for (server commands and URLs, sampling,
capability grants, permission changes) and asks `Trust this project
config? [y/N]`. Answering yes records the trust. Any other answer
starts kage with those tables ignored.

Print mode, `kage rpc` and `kage mcp serve` cannot ask. They print one
warning on stderr and ignore the tables. Run `kage trust` in the
project directory to allow them, and `kage trust --revoke` to take the
trust back. An editor driving `kage rpc` needs `kage trust` once per
project.

Trust covers the values as they are when you approve them. Editing a
server command, a permission rule or a capability grant makes kage ask
again. Reordering keys does not. The whole table is ignored while
untrusted, even settings that only tighten your rules, such as a
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
| `ZAI_CODING_API_KEY`    | Z.AI Coding                                   |
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

If multiple keys are present, the model id you pass with `-m` or
configure as `provider.default_model` picks the provider.

See [providers](/guide/providers) for the full provider list, custom
endpoints, and per-provider overrides (base URL, headers, key env var).

## directories

| Path                                | Contents                                                       |
| ----------------------------------- | -------------------------------------------------------------- |
| `~/.config/kage/config.toml`        | user config                                                    |
| `<workdir>/.kage/config.toml`       | project-local config overlay                                   |
| `~/.config/kage/themes/`            | user theme TOML files                                          |
| `~/.config/kage/plugins/`           | Lua plugin scripts                                             |
| `~/.config/kage/skills/`            | `SKILL.md` skill directories                                   |
| `~/.config/kage/templates/`         | prompt template `.md` files                                    |
| `~/.local/share/kage/sessions/`     | append-only session JSONL files                                |
| `~/.local/share/kage/auth.json`     | saved provider credentials (`0600`)                            |
| `~/.local/share/kage/plugin-state/` | per-plugin `kage.store` JSON files                             |
| `~/.local/state/kage/`              | session state (`state.json`) and input history (`history.txt`) |
| `~/.local/state/kage/trust.json`    | trusted project configs                                        |

`XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` override the
`~/.config`, `~/.local/share`, and `~/.local/state` roots. Skills and
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
with positional substitution: `$1`, `$2`, ... ; `$@` / `$ARGUMENTS`
for all args joined; `${@:N:L}` for a bash-style slice. Drop files
in the directories above; no Lua required.
