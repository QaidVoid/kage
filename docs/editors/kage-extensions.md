# kage extensions

`kage rpc` and `kage serve` speak the Agent Client Protocol, plus a
few kage-only methods and session updates under the `_kage/` prefix.
An ACP client that knows nothing of them still works; a client that
does gets the file tree, forks, the model catalog and the rest the
kage desktop app uses. Field names are camelCase on the wire.

## initialize

The `initialize` result carries `_meta.kage.cwd`: the directory a
session opened with an empty `cwd` runs in. A client with no
directory of its own, such as a browser, names its sessions' project
by it. `_meta.kage.configOptions` lists the config options a new
session opens with, so a client can show the default model, thinking
level and mode before it opens one. `_meta.kage.externalSessions` is
`true` when the server runs external sessions, which it does on Linux.

A client may send `_meta.kage.unconfiguredTools: "allow"` in its
`clientCapabilities` to get the TUI's permission rules: tools without
a config rule run instead of asking. A client that cannot answer any
permission request starts `kage rpc --yolo` instead, which opens every
session in allow mode. Advertising `subagents` (any
value but `false`) turns on `subagent_update` and child sessions.

A client that can retract interrupted provider output advertises
`clientCapabilities._meta.kage.attemptOutput: true`. It then receives
`_kage/message` boundaries and structured `_kage/provider_retry` updates.
Without this capability, retries arrive as plain `_kage/notice` updates
and the client is not asked to retract output.

## requests

| Method | Params | Result |
| --- | --- | --- |
| `_kage/config/get` | `sessionId?` | The configuration snapshot, read for the session's directory, with secret values redacted. Without a known session, the server's own directory. `acp` holds `[acp.agents]` with environment values redacted. `installedPlugins` lists the plugins, scripts and packages, as `{ name, enabled, version?, fromRegistry }`, where `enabled` says whether `[plugins] disabled` (and the deprecated `enabled` allowlist) let one load, `version` is a package's own, and `fromRegistry` says it was installed from the plugin registry. `providerKeys` maps each provider id kage registers or the config defines to `{ env, source }`: the variable its key is read from, and whether the key is in it (`env`), in the credential store (`auth`), `missing` or not needed (`unneeded`). |
| `_kage/config/set` | `sessionId?`, `path`, `value` | The snapshot after replacing the user `config.toml` entry at `path` (such as `["mcp", "servers", "github"]`) with `value`, in the snapshot's shape, or removing it when `value` is `null`. `path` starts with `providers`, `mcp`, `permissions`, `plugins` or `acp`, or is the two-key `ui.theme_dark` or `ui.theme_light`. The edited file must load and pass kage's startup checks first, and its comments survive; a refused edit is an error and writes nothing. A redacted string in `value` keeps the value on file. An edited `[providers]` or `[acp]` section applies at once; the others apply to sessions opened after. |
| `_kage/config/test` | one of `provider`, `mcp`, `acp` | `{ ok, status?, message, millis, models, tools?, agent? }`, worked out by the engine. `provider: { id, kind?, baseUrl?, apiKeyEnv?, apiKey?, headers? }` lists the provider's models, as saved or as a form holds it; omitted fields fall back to the saved entry, then to the provider's defaults, and a typed `apiKey` is used for this request only. Each model is `{ id, name?, context?, maxOutput? }`, with gaps filled from the model catalog. `mcp: { name, server }` connects to the server (`server` in the snapshot's shape) and lists its `tools` as `{ name, description }`. `acp: { name, command, args, env }` starts the agent, runs `initialize` and names it in `agent`. A redacted value keeps the one saved under the name. An unreachable or refusing endpoint answers `ok: false` with the reason. |
| `_kage/providers/directory` | `url?`, `apiKey?` | `{ providers }`: the providers of a directory in the models.dev `api.json` shape (models.dev itself without `url`), fetched by the engine and kept ten minutes. Each is `{ id, name, api?, env, kind?, models }`, `kind` being the wire protocol kage speaks to it when it knows one (`openai`, `anthropic` or `gemini`), and each model `{ id, name, context?, output?, reasoning, input, cost? }`. Only models that call tools are listed. |
| `_kage/plugins/install` | `source`, `name?`, `replace?` | `{ name }`: installs one `.lua` file from an `https://` URL or a path on the engine's machine into the plugin directory, after checking it compiles. It loads with the next session. |
| `_kage/plugins/remove` | `name` | `{}`: removes an installed plugin, script or package, and its `kura.lock` entry. |
| `_kage/media/read` | `sessionId`, `imageOnly?`, `metadataOnly?` (both default `false`) | `{ mime, data, sha256 }`: the newest image or audio produced by the model or a tool, read from the full session recording, including blob-backed media. `data` is base64 and `sha256` is the lowercase hex digest of the bytes. With `imageOnly: true`, skip audio. With `metadataOnly: true`, return empty `data` and use the digest with `_kage/image/save` without transferring the media. Full reads exceeding the connection's frame limit are refused; metadata-only reads can still save those recordings. An error reports when no matching media is available. |
| `_kage/image/save` | `sessionId`, `sha256`, `path?` | `{}`: asks the engine to write the image or audio clip the model or a tool produced in the session whose bytes hash to `sha256` (lowercase hex) into the session's directory: at `path`, or a new file under `generated/`. An existing file is never replaced. Where it went, or why it could not, arrives as a `_kage/notice` on the session. |
| `_kage/registry/search` | `query?`, `refresh?` | `{ registry, stale?, packages }`: the plugin registry's packages whose name, description or tags hold every word of `query` (all without one), read from the index the engine caches for an hour (`refresh` fetches it again). Each is `{ name, description, tags?, homepage?, version?, unavailable?, capabilities, installed? }`: `version` is the release an install picks, `unavailable` why none fits the engine, `capabilities` that release's `{ name, summary }`, and `installed` the version installed from the registry. `stale` says why a cached index stood in for an unreachable registry. |
| `_kage/registry/install` | `name`, `version?`, `force?` | `{ name, version, previous?, capabilities }`: installs the package's newest release that fits the engine (or `version`), after checking every file against the index. `force` replaces a script or local package of the same name, or discards local edits. It grants nothing: `capabilities` lists what the package may ask for. |
| `_kage/auth/set` | `provider`, `key` | `{}`: saves the API key in the credential store (`auth.json`, never `config.toml`), or removes it when `key` is `null`, and reloads the providers. |
| `_kage/providers/reload` | none | `{}`: rebuilds providers from configuration, cached model lists, credentials and the host's Lua registrations without fetching model lists. |
| `_kage/plugin/resync` | `sessionId` | `{}`: for a session whose plugins the client hosts (`plugins: "client"`), asks the client's `_kage/plugin/tools` again, after it reloaded those plugins. The session's next run uses the tools, overrides and renames they register now; a run in flight keeps its own. |
| `_kage/options/list` | `sessionId?` | `{ options }`: every engine option a client can change, with its `toml` key, `kind` (`bool`, `int`, `fraction`, `choice`, `str`, `key`), bounds or `values`, `default`, `value`, `configured` and `live`. |
| `_kage/options/set` | `name`, `value` | `{ options }` after the write, validated and stored in the user `config.toml` with its comments kept. A refused value is an error. |
| `_kage/models/list` | none | `{ providers }`: each provider with credentials, `{ id, name, models }`, and each model `{ id, name, context?, inputCost?, outputCost?, thinking, images, released? }`. `id` is `provider/model`, the value the `model` config option takes; costs are USD per million tokens; `thinking` lists the `thinking` option values the model accepts. |
| `_kage/fs` | `sessionId`, `op` (`list` or `read`), `path` | `list`: `{ entries, truncated }`, a capped subtree of the session's workdir that skips `.git` and gitignored paths; continue a truncated listing by listing a subdirectory. `read`: `{ content, truncated, binary }`, capped at 512 KB. Paths are relative to the workdir. |
| `_kage/folders` | `path?` | `{ path, parent?, home?, folders, truncated, skipped }`: the folders inside the folder `path` names, or the home folder when it is missing or empty (`~`, `~/...` and `~\\...` join onto home). Hidden folders are left out, the rest sorts case-insensitively, and the list is capped at 2000 names (`truncated` says when). `skipped` counts unreadable entries and non-UTF-8 names. A folder that cannot be read is an error. |
| `_kage/themes` | none | `{ themes, skipped? }`: one entry per `.toml` file in the themes folder, sorted by name; each is `{ name, base?, gui }` with `name` the file name without `.toml`, `base` the bundled theme it starts from, and `gui` its `[gui]` palette (token to color). Files that cannot be read or parsed are left out and named in `skipped` as `"<file name>: <reason>"`. |
| `_kage/session/fork` | `sessionId`, `before?`, `at?` | `{ sessionId }` of a recorded copy, which `session/load` opens: whole, up to the prompt `before` names (`{ text, occurrence }`), or through the entry whose id starts with `at`. A prompt's `at` from `_kage/session/prompts` ends the copy on that prompt, before its reply. With both, `before` wins. |
| `_kage/session/undo` | `sessionId` | `{ prompt }`: drops the session's last prompt and everything after it, and answers with the dropped prompt's blocks for the client's editor. Refused while a run or one of the session's agents is in flight, or with nothing to undo. |
| `_kage/session/prompts` | `sessionId` | `{ prompts }`: every user prompt of the session's branch tree, depth-first and oldest first, each `{ at, prompt, depth, active }`. `at` is the prompt's entry id, `depth` counts the branches above it, and the one `active` prompt is where the conversation continues. |
| `_kage/session/rewind` | `sessionId`, `at` | `{}`: continues the session from the end of the turn of the prompt `at` names (any branch). Refused while a run or one of the session's agents is in flight. |
| `_kage/session/delete` | `sessionId` | `{}`: deletes a recorded session, named by its whole id. Refused while it is open, closing, running, an agent of a live session, or held by another kage process. |
| `_kage/shell` | `sessionId`, `command` | `{}`: runs a command the user typed, in the session's directory and environment. Its progress arrives as `_kage/shell` updates, and its output joins the conversation for the model's next turn. |
| `_kage/prompt/withdraw` | `sessionId`, `delivery` (`steer` or `queue`) | `{ delivery, content? }`: takes back the newest prompt still waiting in that queue, for the client's editor. `content` is absent when nothing was waiting. |
| `_kage/ui/attach` | `cwd`, `attachId`, `sessionId?` | The plugin UI of the project at `cwd`, for a client that draws it: `{ generation, keymap, highlights, options, optionSources, commands, status, blockKinds, autocomplete, terminalHooks }`. `attachId` must be a number newer than any attach the connection made before; the host refuses an older one. With `sessionId`, the UI pins to the runtime that session opened with, so what it advertises is what handles that session's commands and mappings; without one it follows the project's latest loaded runtime. `keymap` lists `{ mode, lhs, rhs, desc?, group?, owner }` in table order, `rhs` being `{ kind: "action", name, arg? }`, `{ kind: "command", line }`, `{ kind: "lua", id }` or `{ kind: "nop" }`. `commands` lists `{ name, aliases?, description, args?, overrides? }`, each `args` entry `{ kind: "text", name, optional, hint }`, `{ kind: "choice", name, values, optional }`, `{ kind: "path", name, optional }`, `{ kind: "session", name, optional }` or `{ kind: "flag", name }`. `options` holds every option's current value by name, and `optionSources` where each came from (`default`, `toml`, `lua` or `runtime`). `highlights` is `{ base, overrides, theme, transparent }`, each group `{ fg, bg, bold, italic, underline, dim, reverse, link }` with unset colors and links `null`. The plugins stay loaded while the connection is attached, and an unbound attachment's project loads again when its files change. A connection follows one attachment at a time: attaching again stops the first one's updates. |
| `_kage/ui/command` | `cwd`, `name`, `args?`, `sessionId?` | `{ text, isError?, structured? }`: runs the plugin command `name` (or an alias) of the project at `cwd` with the argument line `args`, for the client's session `sessionId` when it has one, loading the project's plugins first when needed. Each dialog the command opens reaches the client as a `_kage/ui/dialog` request. |
| `_kage/ui/keymap` | `cwd`, `id`, `sessionId?` | `{ text, isError?, structured? }`: runs the Lua handler of a key mapping whose `rhs` is `{ kind: "lua", id }`, dialogs included. |
| `_kage/mcp/restart` | `sessionId`, `server` | `{}`: restarts one of the session's MCP servers. The outcome arrives as `_kage/mcp_status` and `_kage/mcp_servers`. |
| `_kage/session/export` | `sessionId` | `{ markdown }`: the transcript as Markdown. |
| `_kage/session/compact` | `sessionId` | `{}`: summarizes older turns now. |
| `_kage/session/rename` | `sessionId`, `title` | `{}`: a named session keeps its name over generated titles. |
| `_kage/swarm/resume` | `sessionId`, `members` | `{ resumed }`: continues swarm children (child session id to a follow-up prompt; empty continues the task). It answers once every member is checked and attached, and refuses the whole request when one is not a swarm child of the session or is still working. Members report under the call that first spawned them. Once all have reported, the session gets a notice with the counts and its next turn reads their results. |

A client can open the bytes returned by `_kage/media/read` with its local
viewer or player, without sharing the host's filesystem. To save them in
the session's workspace, reuse `_kage/image/save` with the returned hash;
that method handles both images and audio.

## requests to the client

| Method | Params | Result |
| --- | --- | --- |
| `_kage/ui/dialog` | `kind` (`ui.select`, `ui.confirm`, `ui.input` or `ui.editor`), `payload` | The dialog's value, or `null` when the user dismissed it. Asked of the client whose `_kage/ui/command` or `_kage/ui/keymap` opened it; `payload` holds the fields the plugin passed, as in [`kage.ui`](/plugins/api). A client that cannot show it answers `null`, and an error reply cancels the dialog the way `null` does. |

## notifications

| Method | Params | Meaning |
| --- | --- | --- |
| `_kage/ui/update` | `cwd`, `attachId`, `generation`, and any of the snapshot's fields | Sent by the agent to a client attached with `_kage/ui/attach`, at most every 200 ms, carrying the fields of the plugin UI that changed, each in full. `attachId` names the attachment the update belongs to, so a client applies only the updates of the attachment it holds or waits for. `generation` is one more than the snapshot's or the previous update's; an update that arrives before the attach answer applies on top of it once it arrives. |
| `_kage/environment` | `vars` | The client's environment, variable name to value. Sessions the connection opens after it run their shell commands, stdio MCP servers, plugin processes, worktree VCS commands and `acp:` model agents with exactly these variables. `kage rpc` sends it when it relays to the [shared host](/guide/shared-host). Only the shared host's local socket accepts it; other connections ignore it. |

## session updates

| `sessionUpdate` | Fields | Meaning |
| --- | --- | --- |
| `_kage/turn` | `phase` (`start`, `end`), `reason?` (`tool_calls`, `no_tool_calls`), `at?`, `tookMs?` | One provider round trip of the running prompt. A loaded session's history closes each run that ended with an `end` carrying when it ended (`at`, Unix seconds) and how long it took. |
| `_kage/message` | `messageId`, `phase` (`start`, `end`) | One provider attempt. Output after `start` is provisional; a matching `end` commits it. Sent only with `attemptOutput`. |
| `_kage/provider_retry` | `messageId?`, `attempt`, `maxAttempts`, `waitSecs`, `waitMs`, `requestedSecs?`, `error` | Discard the named attempt's provisional output, then show a retry notice. `attempt` is the one-based retry count, `waitMs` the actual backoff, and `requestedSecs` the provider's requested delay when supplied. Without `messageId`, no output is discarded. Sent only with `attemptOutput`, instead of a plain retry notice. |
| `_kage/notice` | `tone` (`info`, `warn`, `error`, `success`), `text` | A message for the user that is not part of the conversation, such as `goal met: ...`. |
| `_kage/compaction` | `phase` (`started`, `complete`, `aborted`), `kept?`, `before?`, `after?`, `summarized?`, `summary?` | The compaction lifecycle in one update. `started`: the agent began summarizing, naming the turns the summarizer will read (`summarized`) and how many stay verbatim (`kept`). `complete`: older turns were summarized, with context tokens `before` and `after`, the turns the summary replaced and the framed `summary` text; `after` is absent when no turn reported usage after the compaction. `aborted`: the summarizer failed; history is intact and the run continues, with the reason as an error notice. An update without a `phase` is an older engine's `complete`. |
| `_kage/mcp_status` | `name`, `status` (`connected`, `starting`, `needs_auth`, or `{ failed: { error } }`) | One MCP server's reachability changed. |
| `_kage/shell` | `command`, `finished`, `tail?`, `output?`, `exitCode?` | A user shell command's progress: `tail` holds the last lines while it runs, and the finished update carries the whole `output` and `exitCode` (absent when a signal ended it). |
| `_kage/prompt_withdrawn` | `delivery`, `content` | A cancel handed back a steer that was still waiting, for a client to put back into its editor. A prompt `_kage/prompt/withdraw` takes back is in that request's answer only. |
| `_kage/mcp_servers` | `servers` | Every MCP server of the session, each `{ name, status, error?, tools, resources, templates, prompts }`, sent whole when the session opens, after a restart and whenever a server's catalog changes. |
| `_kage/session_replaced` | `title?` | An undo or rewind replaced the conversation in place. Every kage client showing the session gets it (a client that sent `_meta.kage` at `initialize`): drop what is shown, then the conversation as it now stands and its finished agents follow as ordinary updates, as after `session/load`. Other clients keep what they show. |

A retry retracts only the failed attempt's assistant text, thinking,
media, and pending tool calls. Keep earlier completed messages, user
messages, and notices, even if they arrived during the failed attempt.
Do not merge provisional chunks into an earlier completed message.
Ignore a discard for an unknown or already committed message, but still
show its retry notice. Each retry starts a fresh message; providers may
reuse tool-call IDs, so forget the discarded calls' tracking too.

A loaded session's history also carries the durations the engine
recorded: `_meta.kage.durationMs` on each `agent_thought_chunk` and on
the `tool_call_update` that ends each tool call.

`session/list` entries carry `_meta.kage.parentSessionId` for a
session forked from another, so a client can draw the fork tree. An
external session's entry carries `_meta.kage.externalAgent`, the name of
its agent.

`session/new` accepts `_meta.kage.externalAgent`, the name of an agent
configured under `[acp.agents]`, to open the session on that agent
instead of the kage engine. The server refuses an unknown name and a
request with `mcpServers`. See [external sessions](./acp-client.md#external-sessions).

`session/new` accepts `_meta.kage.opt.compactionReserveTokens` to override
the session's automatic-compaction reserve, an integer from 0 to
4,294,967,295. Omission keeps the host's configured reserve, which defaults
to 50,000 tokens. An explicit `0` disables only the reserve trigger;
`compactionThreshold` still applies. A positive reserve can trigger
compaction before that fraction is reached. These fields configure native
kage sessions, not the compaction policy of external agents.

Media a model produces, such as the images of a model that draws,
arrive as standard ACP `image` (or `audio`) blocks in
`agent_message_chunk`, carrying their bytes as base64, between the text
chunks around them. A loaded session replays them the same way. The
bytes ride the stream rather than a `file://` link, so a client that
does not share the engine's filesystem shows them too. An image whose
bytes the engine no longer holds replays as a text chunk saying so.
Images a tool produced arrive the same way inside the `content` of the
`tool_call_update` that ends the call, after its text.

## sessions an agent started

A subagent's session streams on its own id while it runs. Loading a
finished agent's session with `session/load` shows its transcript but
does not open it in the engine: prompting it is refused, because an
agent belongs to the call that started it. Loading a session rebuilds
`subagent_update` records for the agents its history started, at
every depth, from the results the engine recorded.

Standard `usage_update` messages carry cumulative token totals under
`_meta.kage`. Its optional `costStatus` is `recorded`, `estimated`, or
`incomplete`; omission means `recorded`. Estimated totals use current prices
for legacy usage, while incomplete totals omit some unpriced usage. A known
free amount can be present as zero. Do not display missing pricing as free.

A `subagent_update` may also carry `model`, the child's
`provider/model`, and `usage`: `{ input, output, cacheRead,
cacheWrite, cost, costStatus?, runMs? }`, the child's token totals, its cost in
USD and how long it has run. The engine sends them as the child
reports usage and on the update that ends it.
