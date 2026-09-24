# providers

kage talks to LLM providers through ids addressed as
`provider-id:model-id`, e.g. `kage -m anthropic:claude-sonnet-4-6` or
`kage -m deepseek:deepseek-chat`. Every provider kage knows is either
built in, part of the OpenAI-compatible catalog, or declared by you
under `[providers.custom.*]`.

## built-in providers

| id                 | provider            | credential env      |
| ------------------ | ------------------- | ------------------- |
| `anthropic`        | Anthropic           | `ANTHROPIC_API_KEY` |
| `openai`           | OpenAI              | `OPENAI_API_KEY`    |
| `openai-responses` | OpenAI Responses API | `OPENAI_API_KEY`   |
| `gemini`           | Google Gemini       | `GEMINI_API_KEY`    |
| `acp`              | external ACP agents | -                   |

`openai-responses` serves `openai-responses:<model>` addressing on top
of the same OpenAI credential; `kage auth login openai` covers both.
`acp` is not an HTTP provider and cannot be overridden here; its agents
are configured under `[acp.*]`.

## openai-compatible providers

Each catalog entry is an OpenAI-compatible endpoint. kage registers it
when a credential is available, and its models become addressable as
`<id>:<model>`:

| id                      | provider         | credential env       | default base URL                           |
| ----------------------- | ---------------- | -------------------- | ------------------------------------------ |
| `zai`                   | Z.AI             | `ZAI_API_KEY`        | `https://api.z.ai/api/paas/v4`             |
| `zai-coding-plan`       | Z.AI Coding Plan | `ZAI_CODING_API_KEY` | `https://api.z.ai/api/coding/paas/v4`      |
| `deepseek`              | DeepSeek         | `DEEPSEEK_API_KEY`   | `https://api.deepseek.com/v1`              |
| `groq`                  | Groq             | `GROQ_API_KEY`       | `https://api.groq.com/openai/v1`           |
| `mistral`               | Mistral          | `MISTRAL_API_KEY`    | `https://api.mistral.ai/v1`                |
| `cerebras`              | Cerebras         | `CEREBRAS_API_KEY`   | `https://api.cerebras.ai/v1`               |
| `xai`                   | xAI              | `XAI_API_KEY`        | `https://api.x.ai/v1`                      |
| `openrouter`            | OpenRouter       | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1`             |
| `fireworks-ai`          | Fireworks AI     | `FIREWORKS_API_KEY`  | `https://api.fireworks.ai/inference/v1`    |
| `moonshotai`            | Moonshot         | `MOONSHOT_API_KEY`   | `https://api.moonshot.ai/v1`               |
| `kimi-for-coding`       | Kimi for Coding  | `KIMI_API_KEY`       | `https://api.kimi.com/coding/v1`           |
| `xiaomi`                | Xiaomi           | `XIAOMI_API_KEY`     | `https://api.xiaomimimo.com/v1`            |
| `xiaomi-token-plan-ams` | Xiaomi AMS plan  | `XIAOMI_API_KEY`     | `https://token-plan-ams.xiaomimimo.com/v1` |
| `xiaomi-token-plan-cn`  | Xiaomi CN plan   | `XIAOMI_API_KEY`     | `https://token-plan-cn.xiaomimimo.com/v1`  |
| `xiaomi-token-plan-sgp` | Xiaomi SGP plan  | `XIAOMI_API_KEY`     | `https://token-plan-sgp.xiaomimimo.com/v1` |

The four `xiaomi*` ids share one key. `zai` and `zai-coding-plan` are
billed separately and need their own keys.

## credentials

A provider registers when a key is reachable from either source, first
match wins:

1. the credential environment variable from the tables above;
2. the key saved by `kage auth login <provider-id>`
   (`~/.local/share/kage/auth.json`, mode `0600`).

`kage auth login` with no argument opens a picker that also lists your
custom providers; rows marked `*` already have a stored credential.
`kage auth list` shows where each provider's key would come from.
Inside the TUI, `:login [provider]` runs the same flow: the UI suspends,
the credential prompt takes over the terminal, and the model list
refreshes in place when a key is saved.

## overriding a provider

`[providers.<provider-id>]` replaces the base URL or the key variable
of a built-in or catalog provider, or adds headers to every request.
`acp` cannot be overridden:

```toml
[providers.deepseek]
base_url = "https://relay.internal/v1"
api_key_env = "DEEPSEEK_RELAY_KEY"

[providers.deepseek.headers]
X-Team = "infra"
```

Fields left out keep the provider's default.

## custom providers

`[providers.custom.<id>]` registers a provider kage does not know. The
id must be lowercase letters, digits, and dashes; it becomes the
`<provider-id>` half of `provider-id:model-id`:

```toml
[providers.custom.llama-local]
kind = "openai"                      # openai (default), anthropic, or gemini
base_url = "http://localhost:8080/v1"
display_name = "Llama (local)"       # shown in the picker; defaults to the id
api_key_env = ""                     # endpoint needs no key; omit to use LLAMA_LOCAL_API_KEY

# one [[...models]] table per model
[[providers.custom.llama-local.models]]
id = "llama-3-70b"
name = "Llama 3 70B"
context = 131072                     # optional; tokens
max_output = 8192                    # optional; tokens

[providers.custom.relay]
kind = "anthropic"
base_url = "https://relay.internal"
api_key_env = "RELAY_API_KEY"
thinking = true                      # thinking blocks survive across turns
caching = true                       # endpoint supports prompt caching
tool_use = true                      # endpoint accepts tool definitions (default)

[providers.custom.relay.headers]
Authorization = "Basic c2VydmljZTpwdW5jdWF0aW9u"

[[providers.custom.relay.models]]
id = "sonnet-relay"
name = "Sonnet via relay"
```

Models are addressed as `llama-local:llama-3-70b` and
`relay:sonnet-relay`, and appear in the model picker once their key is
available. A provider whose `api_key_env` is set to a non-empty
variable that is unset (and has no saved key) is skipped; with
`api_key_env = ""` it registers unconditionally, which is what
keyless local endpoints want.

## base urls

`base_url` replaces the default endpoint root; kage appends the
protocol-specific path. What "root" means depends on `kind`:

| kind       | request path                                          | base_url must include |
| ---------- | ----------------------------------------------------- | --------------------- |
| `openai`   | `<base>/chat/completions`                             | the `/v1` segment, e.g. `http://localhost:8080/v1` |
| `anthropic`| `<base>/v1/messages`                                  | the host root, e.g. `https://relay.internal` |
| `gemini`   | `<base>/v1beta/models/<model>:streamGenerateContent`  | the host root, e.g. `https://generativelanguage.googleapis.com` |

The built-in `openai-responses` provider follows the `openai` rule with
`<base>/responses`. API keys travel in request headers (`x-api-key`,
`Authorization`, `x-goog-api-key`), never in the URL.

## validation

A `[providers]` section that kage would silently ignore is refused at
startup with exit status 1:

- a custom id must be non-empty lowercase letters, digits, or dashes;
- a custom id must not shadow a registered provider; override that
  provider with `[providers.<id>]` instead;
- a custom provider must declare at least one model;
- a `[providers.<id>]` override must name a provider that can be
  overridden (any built-in HTTP provider or catalog entry, not `acp`).

As with all config, environment variables override file values with
`KAGE_` plus `__` for nesting:
`KAGE_PROVIDERS__CUSTOM__MYLAMA__BASE_URL=http://10.0.0.2:8080/v1`
retargets `[providers.custom.mylama]`. Env var names cannot contain
dashes, so this form only reaches ids without one (`MYLAMA` maps to
`mylama`, never to `llama-local`); an env var whose nested name does
not match a declared id fails the whole config load, and kage warns
and falls back to defaults.
