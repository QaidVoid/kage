# configuration

kage reads its configuration from `~/.config/kage/config.toml`. The file is
optional; sane defaults work without it. `kage init` writes a
starter one.

Keys are grouped into tables:

```toml
[provider]
# active default model, provider:model qualified.
default_model = "anthropic:claude-sonnet-4-6"

[ui]
# bundled theme name, or a user theme under ~/.config/kage/themes/<name>.toml.
theme = "default"
# capture terminal mouse events. Toggle at runtime with :mouse.
mouse = true

[plugins]
# override the plugin directory (default ~/.config/kage/plugins/).
# dir = "/path/to/plugins"
# if non-empty, only these plugin file stems load.
enabled = []

[sandbox]
# "local" (default in 0.1), "bubblewrap", or "sandbox-exec".
backend = "local"
# silence the "running unsandboxed" startup warning.
suppress_warning = false

[keybindings]
# chord -> builtin command name.
# bindings = { "ctrl+r" = "cancel" }

[loop]
# compact older history once the prompt fills this fraction of the
# model context window (0.0-1.0). Applies on next launch; also
# editable in the :settings dialog.
compaction_threshold = 0.8
```

Every table and key is optional; omitted values fall back to the
defaults shown above.

## layering

Configuration is merged lowest-to-highest precedence:

1. built-in defaults
2. `~/.config/kage/config.toml` (user)
3. `<workdir>/.kage/config.toml` (project-local; commit it to share
   team settings)
4. `KAGE_*` environment variables

Env vars use `KAGE_` with `__` for nesting, e.g.
`KAGE_UI__THEME=catppuccin-mocha` overrides the `[ui].theme` key.

## environment variables

API keys are read from environment variables:

| Variable                | Provider                                      |
| ----------------------- | --------------------------------------------- |
| `ANTHROPIC_API_KEY`     | Anthropic Claude                              |
| `OPENAI_API_KEY`        | OpenAI                                        |
| `GEMINI_API_KEY`        | Google Gemini                                 |
| `ZAI_API_KEY`           | Z.AI                                          |
| `ZAI_CODING_API_KEY`    | Z.AI Coding                                   |

If multiple keys are present, the model id you pass with `-m` or
configure as `model` picks the provider.

## directories

| Path                            | Contents                            |
| ------------------------------- | ----------------------------------- |
| `~/.config/kage/config.toml`    | user config                         |
| `<workdir>/.kage/config.toml`   | project-local config overlay        |
| `~/.config/kage/themes/`        | user theme TOML files               |
| `~/.config/kage/plugins/`       | Lua plugin scripts                  |
| `~/.config/kage/skills/`        | `SKILL.md` skill directories        |
| `~/.config/kage/templates/`     | prompt template `.md` files         |
| `~/.local/share/kage/sessions/` | append-only session JSONL files     |
| `~/.local/share/kage/auth.json` | saved provider credentials (`0600`) |

`XDG_CONFIG_HOME` / `XDG_DATA_HOME` override the `~/.config` and
`~/.local/share` roots. Skills and templates are also discovered
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
