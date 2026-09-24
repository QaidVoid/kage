# permissions

By default kage runs every tool call without asking: the same yolo
behavior it always had. The `[permissions]` table lets you opt specific
tools into rules instead. Nothing changes until you write
configuration.

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
```

## how rules evaluate

Tool names are literal: `bash`, `write`, `edit`, `web_fetch`, or any
registered MCP tool name such as `mcp_github_create_issue`. A tool
with no `[permissions.tools.<name>]` entry is always allowed.

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

## what "ask" does per mode

| mode | ask behavior |
|---|---|
| TUI | a modal opens showing the tool and its command; choose allow once, always allow, or deny. Ctrl+C cancels the prompt and the call. |
| print (`kage -p`) | the call is denied with an error telling you to add an allow rule; there is no interactive prompt. |
| ACP (`kage rpc`) | the editor client is asked through `session/request_permission`, exactly as before. A config `allow` skips the round-trip; a config `deny` refuses locally. |

"Always allow" flips the tool's `default` to `allow` for the running
session and persists `[permissions.tools.<name>] default = "allow"`
into your user config file (`~/.config/kage/config.toml`),
comment-preserving. Project `.kage/config.toml` layers are never
baked in.

## path confinement

`confine_paths = true` routes the built-in file tools through
escape-checked resolution: a read or write must stay under the working
directory. `bash` is unaffected; a shell can always reach the whole
filesystem, so confine it with `deny` rules instead.

## validation

Broken configuration refuses to start: empty tool names, empty
patterns, or patterns that do not compile print an error and exit 1.
