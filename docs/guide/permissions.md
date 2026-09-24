# permissions

By default kage runs every built-in tool call without asking: the
same yolo behavior it always had. The `[permissions]` table lets you
opt specific tools into rules instead. MCP tools are the exception:
they ask unless you allow their server (see
[MCP tools](#mcp-tools)).

```toml
[permissions]
# opt in to path confinement for the built-in file tools (read,
# write, edit, grep, find, ls). when true, paths that escape the
# working directory through `..`, absolute paths outside it, or
# symlinks that point outside it are rejected. default false.
confine_paths = false

[permissions.tools.bash]
# what happens when neither the deny nor the allow list matches.
# "allow" (the default), "ask", or "deny".
default = "ask"
# glob patterns. a match runs the call without prompting.
allow = ["git *", "cargo *"]
# glob patterns. a match refuses the call outright.
deny = ["rm -rf *"]

[permissions.mcp]
# action for the tools of one MCP server: "allow", "ask", or "deny".
# servers not listed here ask.
github = "allow"
```

A `[permissions]` table in a project's `.kage/config.toml` applies only
once you trust the project (see
[project config and trust](/guide/config#project-config-and-trust)).

## how rules evaluate

Tool names are literal: `bash`, `write`, `edit`, `web_fetch`, or any
registered MCP tool name such as `github__create_issue`. A built-in
tool with no `[permissions.tools.<name>]` entry is always allowed.

When an entry exists, the `deny` patterns are checked first, then the
`allow` patterns, then `default`. First match wins; within a list,
patterns are tried in written order.

Patterns match against the tool's command line: the `command` string
for shell-style tools, or the compact JSON encoding of the whole input
for everything else. The globs are the standard `globset` syntax
(`*`, `?`, `[...]`).

An entry with only a `deny` list keeps everything else allowed, so you
can block a few dangerous calls without opting into prompts:

```toml
[permissions.tools.bash]
deny = ["curl *", "wget *"]
```

## mcp tools

An MCP tool is named `<server>__<tool>`. Its verdict is decided in
this order:

1. a `[permissions.tools.<server>__<tool>]` entry, evaluated as above;
2. otherwise the server's action under `[permissions.mcp]`;
3. otherwise `ask`.

So an unconfigured MCP tool always asks, while a per-tool entry can
tighten or loosen one tool of an allowed or denied server:

```toml
[permissions.mcp]
github = "allow"

[permissions.tools.github__create_issue]
default = "ask"
```

This matters most in print mode, which cannot ask: MCP tools there are
refused until you allow the server or the tool.

## what "ask" does per mode

| mode | ask behavior |
|---|---|
| TUI | an approval panel replaces the input box. See [approving in the TUI](#approving-in-the-tui). |
| print (`kage -p`) | the call is denied with an error telling you to add an allow rule; there is no interactive prompt. For an MCP tool the error names both the `[permissions.mcp]` and the per-tool fix. |
| ACP (`kage rpc`) | the editor client is asked through `session/request_permission`. Every tool without a config entry asks here, built-ins included. A config `allow` skips the round-trip; a config `deny` refuses locally. |
| MCP server (`kage mcp serve`) | the call is refused, since there is no one to ask. |

## approving in the TUI

The panel's title names the action, such as `Run this command?`,
`Edit src/lib.rs?` or `Allow github.create_issue?`. Below it, a shell
call shows its command, an edit shows its diff, a write shows the path
and the first lines, and any other tool shows its arguments as
`key value` rows. Five options follow, with `Yes` selected:

| Option | Keys | Effect |
|---|---|---|
| Yes | `1`, `y` | Run this call. The next call of the tool asks again. |
| Yes, and allow the tool for the rest of this session | `2`, `s` | Run this call and stop asking for this tool until kage exits. Nothing is written to disk. |
| Yes, and always allow the tool (saved to config.toml) | `3`, `a` | Run this call, stop asking for this tool until kage exits, and save the rule described below. |
| No | `4`, `n`, `Esc` | Refuse the call. The model sees `denied by user`. |
| No, and tell kage what to do instead | `5`, `t` | Open a one-line field. `Enter` refuses the call and sends your text to the model with the denial. `Esc` goes back to the options. |

`Up`, `Down` and `Enter` pick an option too. Keys pressed in the first
400 ms after the panel opens are dropped, so typing meant for the
prompt cannot answer it. When several calls wait, the title shows
`1 of 3`, and the panels come one after another. `Ctrl+C` interrupts
the run, which refuses the call. The tool's row in the conversation
reads `waiting` until you answer, then runs or reads `denied`.

Both "allow" scopes cover the tool by name, every call of it, for as
long as kage runs, including sessions you switch to with `/new` or the
session picker. They are checked before `/permission ask` and before
the `[permissions]` rules, so an approved tool stops asking even in
ask mode. Only `/permission deny` still refuses it. This also skips
the tool's `deny` patterns until kage exits, so approve a tool for the
session only when you would run any call of it.

"Always allow" also persists `[permissions.tools.<name>] default =
"allow"` into your user config file (`~/.config/kage/config.toml`),
comment-preserving. Project `.kage/config.toml` layers are never
baked in. After a restart the saved rule applies like any other, so
the tool's `deny` patterns count again.

## path confinement

`confine_paths = true` routes the built-in file tools through
escape-checked resolution: a read or write must stay under the working
directory. `bash` is unaffected; a shell can always reach the whole
filesystem, so confine it with `deny` rules instead.

## runtime mode (`:permission`)

Switch modes without touching the config file. In the TUI:

    :permission ask     # every tool call prompts this session
    :permission deny    # every tool call is refused this session
    :permission default # back to the configured rules (allow-all
                        # unless you configured [permissions])

`allow` is an alias of `default`. With no argument, `:permission`
shows the current mode. An active override shows as `ask mode` or
`deny mode` in the footer, and on the start card while the
conversation is empty.

The override lives for the current session only and is never written
to the config file. It short-circuits the per-tool rules entirely.
While `deny` is active, even allow-listed tools and tools approved in
the panel refuse. While `ask` is active, even never-configured tools
prompt, except the tools you approved for the session or always.

## validation

Broken configuration refuses to start: empty tool or server names,
empty patterns, or patterns that do not compile print an error and
exit 1.
