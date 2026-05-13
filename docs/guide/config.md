# configuration

kage reads its configuration from `~/.config/kage/config.toml`. The
file is optional; sane defaults work without it.

```toml
# active default model. provider:model qualified.
model = "anthropic:claude-sonnet-4-6"

# system prompt prepended to every conversation.
system = "You are kage, a helpful coding agent."

# theme name from the bundled set, or a user theme under
# ~/.config/kage/themes/<name>.toml.
theme = "kage-dark"

# auto-compact when context fills this fraction of the model's
# window. Set to 0 to disable.
autocompact_threshold = 0.85

# enable terminal mouse capture by default. Toggle at runtime
# with :mouse.
mouse = true
```

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

| Path                           | Contents                              |
| ------------------------------ | ------------------------------------- |
| `~/.config/kage/`              | `config.toml`, `auth.json`            |
| `~/.config/kage/plugins/`      | Lua plugin scripts                    |
| `~/.config/kage/themes/`       | User theme TOML files                 |
| `~/.local/share/kage/sessions/`| Append-only session JSONL files       |

The `XDG_CONFIG_HOME` and `XDG_DATA_HOME` environment variables
override the defaults if you have them set.
