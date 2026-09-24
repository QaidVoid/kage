# mcp

kage speaks the [Model Context Protocol](https://modelcontextprotocol.io)
(JSON-RPC 2.0, protocol revision `2025-06-18`) in both directions:

- **client**: connect to external MCP servers and expose their tools
  to the agent alongside the built-ins.
- **server**: `kage mcp serve` exposes a chosen set of kage's built-in
  tools to another MCP-aware agent.

## using external mcp servers

Declare servers in `config.toml` under `[mcp.servers.<name>]`. Each
server uses exactly one of two transports, chosen by which key is set.

A **stdio** server sets `command`. kage spawns the child and speaks
newline-delimited JSON-RPC over its stdin and stdout:

```toml
[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.filesystem.env]
SOME_TOKEN = "..."
```

A **Streamable HTTP** server sets `url`. kage POSTs each JSON-RPC
message to that endpoint and sends `headers` on every request, which
is where an `Authorization` header goes:

```toml
[mcp.servers.github]
url = "https://example.com/mcp"

[mcp.servers.github.headers]
Authorization = "Bearer ..."
```

Setting both `command` and `url`, or neither, is an error. Add
`disabled = true` to keep a server configured without connecting to
it.

Each enabled server is connected at startup, handshaked, and its tools
are registered. A stdio server's `stderr` is inherited so its
diagnostics reach your terminal. A server that fails to start or to
list its tools is reported (inline in the TUI, on stderr in `-p` and
`kage rpc`) and the rest still load. kage never swallows the failure.

kage advertises the working directory to every server as its single
filesystem root (`roots/list`).

When kage exits normally it kills every stdio child it spawned. If
kage itself is killed or crashes, the children are not cleaned up and
may keep running.

### sampling

A server may ask the host to run an LLM completion
(`sampling/createMessage`). This is off by default because it spends
your token budget on the server's behalf. Opt in with:

```toml
[mcp]
allow_sampling = true
```

The request runs against your default model.

### tool names

A server's tools are namespaced `<server>__<tool>` (for example
`filesystem__read_file`) so a server can never shadow a built-in.

If a server announces `notifications/tools/list_changed`, kage
re-lists that server and swaps its adapters in place. Tools it no
longer offers are removed, not left dangling.

### permissions

Built-in tools run without asking unless you configure rules. MCP
tools are different: a server is opaque, so every call to an MCP tool
**asks by default**. In the TUI a prompt opens, and in an editor over
`kage rpc` the client is asked. Print mode (`kage -p`) cannot prompt,
so the call is refused with a message naming the fix.

Allow or deny a whole server under `[permissions.mcp]`:

```toml
[permissions.mcp]
github = "allow"
filesystem = "deny"
```

Servers not listed there ask. A `[permissions.tools.<name>]` entry for
one tool wins over the server's action, so you can allow a server and
still gate one of its tools:

```toml
[permissions.mcp]
github = "allow"

[permissions.tools.github__create_issue]
default = "ask"
```

Choosing "always allow" in the TUI prompt writes such a per-tool
entry. If you run MCP servers from `kage -p` scripts, add
`[permissions.mcp]` entries for them, or their tools will be refused.
See [permissions](/guide/permissions) for the full reference.

## declaring servers from a plugin

Plugins configure; core spawns (the `nvim-lspconfig` model). From
Lua:

```lua
kage.mcp.add_server({
  name = "filesystem",
  command = "npx",
  args = { "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { SOME_TOKEN = "..." },
  disabled = false,   -- optional, default false
})

local names = kage.mcp.list_servers()  -- { "filesystem", ... }

kage.mcp.restart("filesystem")          -- respawn a wedged server
```

`name` and `command` are required. A plugin-declared server overrides
a `config.toml` entry of the same name, so a plugin can ship a
working default a user can still replace.

`kage.mcp.restart(name)` enqueues a restart that the host applies
before the next turn: the server is respawned from its original spec
and its tools are re-registered. The respawn is brought up before the
old child is killed, so a restart that fails leaves the existing
server running and reports the error rather than causing downtime.
Restart only knows the servers that came up when the session started.
A server that failed to start, or one declared later, is reported as
unknown and needs a new session.

## running kage as an mcp server

Expose kage's built-in tools to another agent over stdio:

```sh
kage mcp serve
```

By default only the read-only tools are served: `read`, `grep`,
`find`, and `ls`. Pick the set with `--tools`, a comma-separated list
of built-in names (`read`, `write`, `edit`, `bash`, `ls`, `find`,
`grep`, `web_fetch`). An unknown name is an error.

```sh
kage mcp serve --tools read,grep,find,ls,edit
```

Every call is also checked against the `[permissions]` rules of the
working directory. A `deny` verdict refuses the call, and so does
`ask`, because there is no one to ask. `confine_paths = true` keeps
the file tools inside the working directory.

Point any MCP client's server command at `kage mcp serve`. Tool
failures and refusals come back as a normal result with
`isError: true`, so the calling agent sees the message. Only unknown
JSON-RPC methods are protocol-level errors.
