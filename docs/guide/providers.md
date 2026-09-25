# providers

kage talks to LLM providers through ids addressed as
`provider-id:model-id`, e.g. `kage -m anthropic:claude-sonnet-4-6` or
`kage -m deepseek:deepseek-v4-pro`. Every provider kage knows is either
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
of the same OpenAI credential, and `kage auth login openai` covers both.
`acp` is not an HTTP provider and cannot be overridden here. Its agents
are configured under `[acp.*]`.

## openai-compatible providers

Each of these is an OpenAI-compatible endpoint. kage registers it
when a credential is available, and its models become addressable as
`<id>:<model>`:

| id                      | provider                      | credential env       | default base URL                              |
| ----------------------- | ----------------------------- | -------------------- | --------------------------------------------- |
| `zai`                   | Z.AI                          | `ZAI_API_KEY`        | `https://api.z.ai/api/paas/v4`                |
| `zai-coding-plan`       | Z.AI Coding Plan              | `ZAI_CODING_API_KEY` | `https://api.z.ai/api/coding/paas/v4`         |
| `zhipuai-coding-plan`   | Zhipu AI Coding Plan          | `ZAI_CODING_API_KEY` | `https://open.bigmodel.cn/api/coding/paas/v4` |
| `deepseek`              | DeepSeek                      | `DEEPSEEK_API_KEY`   | `https://api.deepseek.com/v1`                 |
| `groq`                  | Groq                          | `GROQ_API_KEY`       | `https://api.groq.com/openai/v1`              |
| `mistral`               | Mistral                       | `MISTRAL_API_KEY`    | `https://api.mistral.ai/v1`                   |
| `cerebras`              | Cerebras                      | `CEREBRAS_API_KEY`   | `https://api.cerebras.ai/v1`                  |
| `xai`                   | xAI                           | `XAI_API_KEY`        | `https://api.x.ai/v1`                         |
| `openrouter`            | OpenRouter                    | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1`                |
| `fireworks-ai`          | Fireworks AI                  | `FIREWORKS_API_KEY`  | `https://api.fireworks.ai/inference/v1`       |
| `moonshotai`            | Moonshot AI                   | `MOONSHOT_API_KEY`   | `https://api.moonshot.ai/v1`                  |
| `kimi-for-coding`       | Kimi for Coding               | `KIMI_API_KEY`       | `https://api.kimi.com/coding/v1`              |
| `xiaomi`                | Xiaomi                        | `XIAOMI_API_KEY`     | `https://api.xiaomimimo.com/v1`               |
| `xiaomi-token-plan-ams` | Xiaomi Token Plan (Europe)    | `XIAOMI_API_KEY`     | `https://token-plan-ams.xiaomimimo.com/v1`    |
| `xiaomi-token-plan-cn`  | Xiaomi Token Plan (China)     | `XIAOMI_API_KEY`     | `https://token-plan-cn.xiaomimimo.com/v1`     |
| `xiaomi-token-plan-sgp` | Xiaomi Token Plan (Singapore) | `XIAOMI_API_KEY`     | `https://token-plan-sgp.xiaomimimo.com/v1`    |

The four `xiaomi*` ids share one key, and so do the two coding plans.
`zai` is billed apart from the coding plans and needs its own key.

The model picker lists the models the [catalog](#model-catalog) knows
for each provider. `kimi-for-coding` has no catalog entry, so the
picker shows no models for it. Name the model yourself, as in
`kage -m kimi-for-coding:<model>`.

### Z.AI and Zhipu AI

Z.AI sells a GLM coding plan in two regions, and kage has a provider
for each:

| id                    | plan                         | endpoint                                      |
| --------------------- | ---------------------------- | --------------------------------------------- |
| `zai-coding-plan`     | Z.AI coding plan (global)    | `https://api.z.ai/api/coding/paas/v4`         |
| `zhipuai-coding-plan` | Zhipu AI coding plan (China) | `https://open.bigmodel.cn/api/coding/paas/v4` |

Both read the same key: `ZAI_CODING_API_KEY`, or the key saved with
`kage auth login zai-coding-plan`. A key saved with
`kage auth login zhipuai-coding-plan` is used for the China plan
instead, when there is one. Each plan lists the models models.dev
publishes for it. `zai` is the pay-as-you-go API with its own key.

If only one endpoint is reachable from your network, point the other
plan at it with a `base_url` override (see below), for example:

```toml
[providers.zhipuai-coding-plan]
base_url = "https://api.z.ai/api/coding/paas/v4"
```

kage shapes requests to `zai`, both coding plans and any custom
provider whose `base_url` is on `api.z.ai` or `open.bigmodel.cn` the
way Z.AI expects: `max_tokens` for the output limit, the system prompt as a
`system` message, `thinking` with `type` `enabled` or `disabled` (and
`clear_thinking: false` when enabled), `reasoning_effort` only for
models that list effort values, and `tool_stream: true` when tools
are sent (except to `glm-4.5` models). kage sends its own
`User-Agent`.

## credentials

A provider registers when a key is reachable from either source, first
match wins:

1. the credential environment variable from the tables above;
2. the key saved by `kage auth login <provider-id>`
   (`~/.local/share/kage/auth.json`, mode `0600`).

`kage auth login` with no argument opens a picker that also lists your
custom providers. Rows marked `*` already have a stored credential.
`kage auth list` shows where each provider's key would come from,
custom providers included. Inside the TUI, `/login [provider]` runs
the same flow: the UI suspends, the credential prompt takes over the
terminal, and the model list refreshes in place when a key is saved.

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

## request headers

`headers` adds HTTP headers to every request a provider sends. It
works on built-in and catalog providers under `[providers.<id>]` and
on custom providers under `[providers.custom.<id>]`, and the headers
go out after kage's own:

```toml
[providers.zhipuai-coding-plan.headers]
X-Team = "infra"

[providers.custom.relay.headers]
Authorization = "Basic c2VydmljZTpwdW5jdWF0aW9u"
```

The inline form works too:
`headers = { X-Team = "infra" }` inside the provider's table.

## custom providers

`[providers.custom.<id>]` registers a provider kage does not know. The
id must be lowercase letters, digits, and dashes. It becomes the
`<provider-id>` half of `provider-id:model-id`:

```toml
[providers.custom.llama-local]
kind = "openai"                      # openai (default), anthropic, or gemini
base_url = "http://localhost:8080/v1"
display_name = "Llama (local)"       # shown in the picker, defaults to the id
api_key_env = ""                     # no key needed. Omit to use LLAMA_LOCAL_API_KEY

# one [[...models]] table per model
[[providers.custom.llama-local.models]]
id = "llama-3-70b"
name = "Llama 3 70B"
context = 131072                     # optional, in tokens
max_output = 8192                    # optional, in tokens
efforts = ["low", "medium", "high"]  # optional, see the model keys below
input = ["text", "image"]            # optional: text, image, pdf, audio, video

[providers.custom.relay]
kind = "anthropic"
base_url = "https://relay.internal"
api_key_env = "RELAY_API_KEY"

[providers.custom.relay.headers]
Authorization = "Basic c2VydmljZTpwdW5jdWF0aW9u"

[[providers.custom.relay.models]]
id = "sonnet-relay"
name = "Sonnet via relay"
```

Models are addressed as `llama-local:llama-3-70b` and
`relay:sonnet-relay`, and appear in the model picker once their key is
available. A provider whose `api_key_env` is set to a non-empty
variable that is unset (and has no saved key) is skipped. With
`api_key_env = ""` it registers unconditionally, which is what
keyless local endpoints want.

The provider keys `tool_use`, `thinking` and `caching` are accepted
but change nothing: kage always sends tool definitions, and thinking
goes back to the model as described under [thinking](#thinking).

Four optional model keys describe what a model accepts, since kage
has no catalog entry for it:

- `reasoning`: `false` for a model that does not think, which sends no
  thinking setting. `true` offers every level.
- `efforts`: the effort values the model takes (`none`, `minimal`,
  `low`, `medium`, `high`, `xhigh`, `max`). Implies `reasoning = true`
  and limits the levels to these.
- `input`: the kinds of input the model takes. Attaching an image to a
  model whose `input` leaves out `image` warns, and the image is not
  sent.
- `interleaved`: for `kind = "openai"`, the assistant message field
  the model reads its own reasoning back from during a tool loop,
  `reasoning_content` or `reasoning_details` (the models.dev
  `interleaved.field`). Left out, reasoning is sent as `<thinking>`
  text in the message content.

A model with neither `reasoning` nor `efforts` sends a level you pick
unchanged, and no level while thinking is automatic.

## model catalog

kage ships a snapshot of the [models.dev](https://models.dev) catalog
for its providers: model names, context and output limits, pricing,
accepted inputs and thinking options. The model picker shows each
model's inputs on the right. Attaching an image while the model's
inputs leave out `image` warns, and the image is not sent.

`kage models refresh` downloads the current catalog into
`~/.cache/kage/models.json` (`$XDG_CACHE_HOME/kage`). Later runs lay
it over the snapshot: it adds models to the providers kage knows and
updates their metadata. It never adds a provider or changes a
provider's endpoint or credentials. kage never refreshes on its own,
and a missing or unreadable cache falls back to the snapshot silently.
Delete the file to go back to the snapshot.

## thinking

A session's thinking level is one of `off`, `minimal`, `low`,
`medium`, `high` and `xhigh`, or automatic. Automatic is the default:
it sends `high`, or the nearest level the model accepts when it has no
`high` (the higher one on a tie). A level you choose with
`shift+tab`, the settings dialog or `[ui] thinking_level` is fitted the
same way, and only an explicit `off` turns thinking off. On a model
that cannot switch thinking off, `off` becomes its lowest level.
`shift+tab` only visits levels the model accepts. The start card shows an
automatic level as `high (auto)`.

The catalog lists how each model takes thinking, and kage maps the
level onto it:

| model takes        | kage sends |
| ------------------ | ---------- |
| effort values      | the effort named like the level. `none` is `off`. `max` stands in for `xhigh` on a model without `xhigh` (a model with both sends `xhigh`, so `max` is not reachable). |
| a token budget     | the level's budget (`minimal` 1024 to `xhigh` 32768 tokens), kept within the model's bounds |
| an on/off switch   | on for any level other than `off`, shown as `high` |
| nothing to set     | no thinking setting |

On the wire, Anthropic effort models get adaptive thinking with
`output_config.effort`, and budget models get `budget_tokens`. OpenAI
models get `reasoning_effort` (`reasoning.effort` on the Responses
API). Gemini models get `thinkingLevel` or `thinkingBudget`.
OpenAI-compatible models with an on/off switch get `thinking.type`
`enabled` or `disabled`. Z.AI endpoints get the shape described under
[Z.AI and Zhipu AI](#z-ai-and-zhipu-ai).

Reasoning goes back to the model that produced it, so thinking carries
across tool calls:

- Anthropic gets its signed thinking and redacted thinking blocks back
  unchanged, first in their assistant message. Budget thinking also
  sends the `interleaved-thinking-2025-05-14` beta header. A request
  that continues a tool loop whose assistant turn has no thinking
  signed by this model leaves budget thinking out, since the API
  would refuse it.
- Gemini gets each thought signature back on its function call.
- The Responses API returns encrypted reasoning, which goes back as the
  `encrypted_content` of its `reasoning` item.
- OpenAI-compatible models with a catalog or config `interleaved`
  field get the current turn's reasoning in that field. Earlier turns
  leave it out.

Anthropic and Gemini cannot verify thinking another model produced, so
they get it as `<thinking>` text. Custom providers of `kind =
"anthropic"` or `"gemini"` follow the same rules.

## base urls

`base_url` replaces the default endpoint root, and kage appends the
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

- A custom id must be non-empty lowercase letters, digits, or dashes.
- A custom id must not be a built-in id (`acp`, `anthropic`,
  `gemini`, `openai`, `openai-responses`). Override those providers
  with `[providers.<id>]` instead, and do the same for a catalog
  provider rather than reusing its id.
- A custom provider must declare at least one model.
- A `[providers.<id>]` override must name a provider that can be
  overridden (any built-in HTTP provider or catalog entry, not `acp`).

As with all config, environment variables override file values with
`KAGE_` plus `__` for nesting:
`KAGE_PROVIDERS__CUSTOM__MYLAMA__BASE_URL=http://10.0.0.2:8080/v1`
retargets `[providers.custom.mylama]`. Env var names cannot contain
dashes, so this form only reaches ids without one (`MYLAMA` maps to
`mylama`, never to `llama-local`). An env var whose nested name does
not match a declared id fails the whole config load, and kage warns
and falls back to defaults.
