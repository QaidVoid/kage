# mcp

kage speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio (newline-delimited JSON-RPC 2.0, protocol revision
`2025-06-18`) in both directions:

- **client**: spawn external MCP servers and expose their tools to the
  agent alongside the built-ins.
- **server**: `kage mcp serve` exposes kage's built-in tools to
  another MCP-aware agent.

## using external mcp servers

Declare servers in `config.toml` under `[mcp.servers.<name>]`:

```toml
[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.filesystem.env]
SOME_TOKEN = "..."

[mcp.servers.github]
command = "/usr/local/bin/github-mcp"
disabled = true   # configured but not spawned
```

Each enabled server is spawned at startup, handshaked, and its tools
are registered. The server's `stderr` is inherited so its diagnostics
reach your terminal; a server that fails to spawn or list tools is
reported (inline in the TUI, on stderr in `-p` / `kage rpc`) and the
rest still load. kage never swallows the failure.

Dropping the session kills every spawned child, so a crashed kage
leaves no orphaned servers.

### tool names and permissions

A server's tools are namespaced `<server>__<tool>` (for example
`filesystem__read_file`) so a server can never shadow a built-in.

Every MCP tool is classified as an **exec**-risk tool regardless of
what it claims to do: a server is opaque, so the host gates every
call through the normal permission flow rather than guessing a tool
is read-only. There is no auto-approve for MCP tools.

If a server announces `notifications/tools/list_changed`, kage
re-lists that server and swaps its adapters in place (tools it no
longer offers are removed, not left dangling).

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
server running and reports the error rather than causing downtime. An
unknown name is reported, never silently ignored.

## running kage as an mcp server

Expose kage's built-in tools (`read`, `write`, `edit`, `bash`, `ls`,
`find`, `grep`, `web_fetch`) to another agent:

```sh
kage mcp serve
```

It speaks MCP over stdin/stdout. Point any MCP client's server command
at `kage mcp serve`. Tool failures come back as a normal result with
`isError: true` (so the calling agent sees the message); only unknown
JSON-RPC methods are protocol-level errors.
