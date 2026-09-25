# agents

kage can hand a task to an agent. An agent is a separate session with
a fresh context: it gets one task, works on it with its own tool
calls, and its final reply comes back as the result. The main
conversation keeps its context for the work that matters, while
agents do the searching, reading and test running.

Every agent stays in view. Its tool row shows what it does right now,
a list above the prompt keeps the running ones visible, and you can
open any agent to read its whole transcript, steer it or stop it.

## how it works

The model starts an agent with the `agent` tool. A call names the
agent definition, a short description you see, and the whole task:

```json
{"agent": "explore", "description": "map exports under src/components",
 "prompt": "Map the named exports of every file under src/components. Reply with one JSON object that maps each file to its export names, and nothing else."}
```

`agent` defaults to `general` when the call leaves it out. The call
waits until the agent's run ends, then returns the text of the agent's
last reply, wrapped in an element that names the agent, its session
and how the run ended:

```text
<agent name="explore" session="01K62W8Q3T9V5M2C7X4B1N0R6S" state="completed">
{"Button.tsx": ["Button", "ButtonProps"], "Modal.tsx": ["Modal"]}
</agent>
```

`state` is `completed`, `cancelled` or `failed`. A cancelled agent
returns its partial reply and a failed one returns its error. Both
come back as error results, so the model knows the task did not
finish. A reply over 20,000 characters is cut, and a trailer names the
agent's session, which holds the full transcript.

The agent sees nothing of the conversation that started it. The
prompt is everything it knows, and nobody answers its questions.

When every tool call in one assistant message is an `agent` call, the
agents run at the same time. A message that mixes `agent` calls with
other tools runs its calls one after another. The tool description
tells the model to use agents for independent work that needs many
tool calls, and to run at most one agent that edits files at a time.

## built-in agents

| Agent | Tools | For |
| --- | --- | --- |
| `general` | every tool of the parent | a self-contained task that needs many tool calls. It can read, edit and run commands. |
| `explore` | `read`, `grep`, `find`, `ls`, `web_fetch` | searching and reading files and web pages to answer a question. It cannot change anything. |

Both reply with exactly what the task asked for, since the reply is
all the caller sees. A file of the same name in your config directory
or a trusted project replaces a built-in.

## writing an agent

An agent definition is a markdown file with frontmatter. kage loads
every `*.md` file directly under these directories:

| Directory | Scope |
| --- | --- |
| `~/.config/kage/agents/` | your agents, in every project |
| `<workdir>/.kage/agents/` | the project's agents, once you trust the project (see [project agents and trust](#project-agents-and-trust)) |

The file stem is the agent's name. It uses lowercase letters, digits
and single hyphens, at most 64 characters, like a skill name. The
built-ins load first, then your files, then the project's files, and a
later definition replaces an earlier one of the same name.

```markdown
---
description: Reviews a diff for bugs and missing tests. Give it the change to review.
tools: read, grep, find, ls, bash
model: anthropic:claude-sonnet-4-6
thinking: high
---
You review code changes. Read the files involved, run the tests that
cover them, and reply with a list of concrete problems, most severe
first. Do not edit files.
```

Saved as `~/.config/kage/agents/reviewer.md`, this defines the agent
`reviewer`. The model reads every agent's name and description in the
`agent` tool's description and picks one by it.

| Key | Required | Value |
| --- | --- | --- |
| `description` | yes | What the agent is for, at most 1024 characters. The model picks agents by it, so say when to use this one and what to give it. |
| `tools` | no | A comma list of tool names. Without it the agent gets every tool its parent has. A listed name that matches no tool of the parent shows a warning when the agent starts. |
| `model` | no | The model, as `provider:model`, or `inherit` (the default) for the parent's current model. A model that is not available fails the agent's run. |
| `thinking` | no | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `inherit` (the default) for the parent's current level. |
| `name` | no | Must equal the file stem when present. |

The frontmatter takes one `key: value` per line. A value may be
wrapped in double quotes, and lines starting with `#` are comments.
Unknown keys are ignored, so agent files written for other tools load,
and their extra keys do nothing.

The body is the agent's role text. The agent's system prompt is the
body plus an environment block with the working directory, the OS,
the shell, the date and the model. Skills are not listed there.

The `agent` tool itself is not a name for `tools`. An agent can start
agents of its own whenever the depth limit allows it (see
[limits](#limits)).

A file that fails to load, such as one without a `description` or
with a malformed `model`, is skipped. The TUI shows the error as a
block at startup. Print mode and `kage rpc` print it on stderr.

## project agents and trust

A project agent can pick its tools and its model, so project agents
only load once you trust the project. They share one trust decision
with the risky tables of the project's `.kage/config.toml` (see
[project config and trust](/guide/config#project-config-and-trust)).

When the TUI starts in a project with untrusted agents, the prompt
lists each one, such as `project agent reviewer
(.kage/agents/reviewer.md)`, next to any risky config settings.
Answering yes trusts all of them. Any other answer starts kage without
the project's agents and without those settings. Print mode and
`kage rpc` print one warning that names the skipped agents. Run
`kage trust` in the project directory to allow them.

Trust records the full text of every project agent file. Adding,
removing or editing one makes the project untrusted again, and kage
asks on the next start.

## what an agent gets

| Aspect | The agent gets |
| --- | --- |
| Model | the definition's `model`, else the parent's current model |
| Thinking | the definition's `thinking`, else the parent's current level |
| Tools | the parent's tools narrowed by `tools`, including plugin and MCP tools, plus `agent` while the depth limit allows it |
| Permissions | the parent's rules, permission mode and session approvals, shared live (see [permissions](/guide/permissions#agents)) |
| Asking | it asks you when the parent can ask. Print mode refuses its asks, like the main session's. |
| Working directory | the parent's |
| History | only the task prompt |
| Plugins | plugin tools only. Plugins get no events from agent runs, and their hooks do not run there. |
| Session file | a file next to the parent's when the parent is recorded |

An agent never gets more permission than its parent. It uses the same
permission gate, so `/permission deny` in the main session refuses the
agent's calls too, and approving a tool "for the rest of this session"
covers every agent of the session.

## limits

| Option | TOML key | Range | Default | What it limits |
| --- | --- | --- | --- | --- |
| `agent_max_depth` | `agents.max_depth` | 0 to 3 | `1` | How deep agents nest. `0` removes the `agent` tool, and `1` lets only the main session start agents. |
| `agent_max_running` | `agents.max_running` | 1 to 16 | `4` | How many agents run at once. Further agents wait in a queue and start in order as others finish. |

Set them in `config.toml`:

```toml
[agents]
max_depth = 2
max_running = 8
```

or in `init.lua`:

```lua
kage.opt.agent_max_depth = 2
kage.opt.agent_max_running = 8
```

Both apply when kage starts. `init.lua` only applies to the TUI.
Print mode and `kage rpc` read the `[agents]` table of your config
files. With `agent_max_depth = 0` the model sees no `agent` tool at
all.

An agent that waits for its own agents does not count against
`agent_max_running`, so nested agents cannot stall the queue. A
result over 20,000 characters is cut, as described in
[how it works](#how-it-works).

## agents in the TUI

::: tip mockups
The screens on this page are ASCII: `.` stands for the middle dot
between facts, `/` for the spinner, `*` for done, `o` for stopped and
`!` for an agent waiting for approval.
:::

### cards

Each `agent` call is a tool row whose body shows the agent live: its
latest tool, described like a tool row, and a line with its tool count
and tokens. A queued agent reads `queued`, and an agent that asks for
approval reads `Waiting for approval` with what it asks for.

```text
 / Agent explore: map exports under src/components                                              41s
     Searched "export " in src/components
     14 tools . 22k tok
 / Agent general: check the router tests                                                        12s
     Running cargo test -p router
     3 tools . 4k tok
```

A finished card shows the head of the agent's reply and its end state
on the right: `done`, `stopped` or `failed`. The wrapper the model
reads is hidden. `Ctrl+O` unfolds the full reply, like any tool row.

```text
 * Agent explore: map exports under src/components                                       done . 52s
     {"Button.tsx": ["Button", "ButtonProps"], "Modal.tsx": ["Modal"], ...}
     ... 9 more lines
 o Agent explore: find dead code                                                      stopped . 12s
     Stopped by you. Partial reply: two unused helpers in src/util.ts so far
```

### the working row and the pinned list

While agents run, the working row counts them, such as
`Waiting for 3 agents (41s, esc to interrupt)`. Below it, a pinned
list keeps every queued, running or waiting agent in view after its
card scrolls away. Agents started by agents sit indented under their
parent. The list shows at most four rows, then `+N more`, and it hides
while the approval panel is open.

```text
  Waiting for 3 agents (41s, esc to interrupt)
  / explore  map exports under src/components            Searched "export " in src/components . 41s
  / explore  map exports under src/routes                            Read src/routes/index.ts . 18s
  / general  check the router tests                              Running cargo test -p router . 12s
```

While agents run and the prompt is empty, the footer names the key
that opens the agents overlay, `ctrl+t for agents` by default.

### opening an agent

Click a pinned row, or pick an agent in the agents overlay, to open
it. The whole view then shows that agent: its transcript, its working
row, its pinned agents and the input. The header shows a breadcrumb
with the path to the agent, its task, state, time, tokens and tool
count:

```text
 kage > explore: map exports under src/components                running . 41s . 22k tok . 14 tools
```

The first message in the transcript is the task the parent wrote. The
footer's model, context and token figures stay the main session's.

| Key | In an agent view |
| --- | --- |
| `Enter` | While the agent runs, steer it at its next turn boundary. When it has finished, send it a new message. |
| `Tab` | Queue the prompt until the agent's run ends |
| `Esc` | On an empty prompt, go back one level: to the parent agent, or to the main view. It never stops the agent. |
| `Ctrl+C` | On an empty prompt, stop the agent while it runs, else go back |

A finished agent can still take messages. Its reply to those stays in
the agent, because the parent's `agent` call already returned. The
placeholder says so: `Message explore (the reply stays in this agent)`.
Going back restores the main view at the scroll position you left.
Quitting with `Ctrl+C` twice only works from the main view.

### the agents overlay

`Ctrl+T` or `/agents` opens a list of every agent of the session,
live or finished, as a tree under the main session. The top border
counts the agents per state and totals their tokens and cost.

```text
+- Agents -------------------------------------- 3 running . 1 waiting . 2 done . 71k tok . $0.21 -+
|   kage        fix the router and map the exports                      running    4m 02s  14k tok |
| > / explore   map exports under src/components   Searched "export "                 41s  22k tok |
|   / explore   map exports under src/routes       Read src/routes/i..                18s   9k tok |
|   ! general   refactor the provider crate        waiting for approval            2m 10s  31k tok |
|     / test    run the provider tests             Ran cargo test                      4s   3k tok |
|   * general   check the router tests             done                            1m 05s  12k tok |
|   o explore   find dead code                     stopped                            12s   2k tok |
+- enter to open . x to stop . esc to close -------------------------------------------------------+
```

| Key | Effect |
| --- | --- |
| `Up` / `Down`, `k` / `j` | Move the selection |
| `Home` / `End` | Jump to the first / last row |
| `Enter` | Open the selected agent. On the `kage` row, return to the main view. |
| `x` | Stop the selected agent and the agents under it. Only live agents stop. |
| `Esc` | Close the overlay |

As over any overlay, `Ctrl+C` interrupts the run of the session on
screen and leaves the overlay open. With no agent in the session yet, `Ctrl+T` shows
`no agents in this session yet`.

### approvals from agents

An agent's approval request joins the one approval queue, wherever you
are looking. The panel's title puts the agent's name before the
question, and option 5 reads
`No, and tell general what to do instead`: your text goes to that
agent, not to the main session.

### stopping agents

- `Esc` or `Ctrl+C` on an empty prompt in the main view interrupts the
  main run, which stops every agent under it.
- `Ctrl+C` in an agent view, or `x` in the agents overlay, stops that
  agent and the agents under it. Its parent keeps running and reads
  the partial reply as a cancelled result.
- Stopping a queued agent ends it before it starts.

Starting a new session, resuming another one or cloning the session
waits until no agent runs: kage says `stop or wait for the agents
first`. Afterwards the agents overlay starts empty.

## sessions on disk

When the main session is recorded, each agent is recorded too, in its
own file next to the main session's in `~/.local/share/kage/sessions/`.
The file's header names the parent session, its first entry records
the agent and the task, and its title is the task description.

Agent sessions stay out of `kage list`, the session picker and the
start card's recent sessions. `/tree` shows them under their parent
with an `agent: ` label, and resuming one there continues that agent
as the main session. The parent's own file records each `agent` call
and its result, which names the agent's session id.

## print mode

`kage -p` runs agents like the TUI does, with the limits from the
`[agents]` table. Print mode cannot ask, so an agent's approval
request is refused the same way the main session's is. Text output
shows only the main session's replies and tool calls. Notices from
any session go to stderr. An `agent` call prints
`[agent explore: map exports]` when it starts and
`[agent explore completed]` when it ends. A cancelled agent prints
`cancelled`, and a failed one prints `failed` followed by its error.

`kage -p --json` prints every envelope, the agents' included. Each
envelope names its session, and an agent's first envelope is
`agent_spawned`:

```json
{"session":"01K62W8Q3T9V5M2C7X4B1N0R6S","seq":1,"type":"agent_spawned","parent":"01K62W7ZB1D6XKQ5H8M3T2V9CE","tool_call_id":"toolu_01","agent":"explore","description":"map exports under src/components"}
```

`parent` is the session whose `agent` call started this one, and
`tool_call_id` is that call. The agent's later envelopes carry its own
session id, so a reader can build the tree from these events.

## editors over ACP

`kage rpc` gives editor sessions the `agent` tool as well. How the
editor shows an agent depends on the editor.

An editor that advertises the ACP `subagents` capability sees each
agent as its own child session:

- The parent session gets a `subagent_update` naming the child's
  session, the agent and its task.
- The agent's messages, tool calls and approval requests arrive on
  its own session.
- A final `subagent_update` marks it `completed`, `failed` or
  `cancelled`, and the editor can cancel a running agent.

Any other editor sees agents through the top-level `agent` call in
its own session:

- The `agent` call's content shows the agent's progress, one line at a
  time, such as `explore: Read src/lib.rs`, then
  `explore: done`, `explore: stopped` or `explore: failed`.
- An agent's approval request arrives as `session/request_permission`
  on the editor's session. The tool call is the top-level `agent`
  call, its title names the agent and the tool, such as
  `explore: bash`, and `rawInput` is the agent's tool input.

In both cases `session/cancel` on the editor's session stops the run
and every agent under it. See [zed](/editors/zed#agents) for the wire
details.

ACP asks for every tool without a config entry, and `agent` is no
exception, so the editor approves the start of each agent unless your
config allows it (see [permissions](/guide/permissions#agents)).
Editor sessions are recorded like TUI sessions, and so are their
agents. Agent sessions stay out of the editor's session list.
