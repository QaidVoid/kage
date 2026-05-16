# neovim

`kage rpc` is a spec-conformant ACP agent: newline-delimited
JSON-RPC 2.0 over stdio, protocol version 1. Neovim can drive it with
a job and a one-line-per-message parser; no plugin required. See
[zed](./zed) for the full method table.

## a minimal client

Each message is a single JSON object terminated by `\n` - no
`Content-Length` headers.

```lua
local M = {}

local function send(job, obj)
  job:write(vim.json.encode(obj) .. "\n")
end

function M.start(opts)
  opts = opts or {}
  local args = { "rpc" }
  if opts.model then
    table.insert(args, "-m")
    table.insert(args, opts.model)
  end

  local buf, id, session = "", 0, nil

  local job = vim.system({ "kage", unpack(args) }, {
    stdin = true,
    stdout = function(_, data)
      if not data then
        return
      end
      buf = buf .. data
      while true do
        local nl = buf:find("\n", 1, true)
        if not nl then
          return
        end
        local line = buf:sub(1, nl - 1)
        buf = buf:sub(nl + 1)
        if #line > 0 then
          local msg = vim.json.decode(line)
          vim.schedule(function()
            M.on_message(job, msg)
          end)
        end
      end
    end,
  })

  function M.request(method, params)
    id = id + 1
    send(job, { jsonrpc = "2.0", id = id, method = method, params = params })
    return id
  end

  M.job = job
  M.request("initialize", { protocolVersion = 1, clientCapabilities = {} })
  -- session/new -> sessionId arrives in M.on_message; stash it, then
  -- M.request("session/prompt", { sessionId = session, prompt = {
  --   { type = "text", text = "explain this file" } } })
  M.request("session/new", { cwd = vim.fn.getcwd(), mcpServers = {} })
  return M
end

-- Override to render in your UI. Notifications carry agent output;
-- `session/request_permission` is a request kage BLOCKS on - you must
-- reply. The default DENIES (never auto-approve); wire it to a real
-- prompt.
function M.on_message(job, msg)
  if msg.method == "session/update" then
    local u = msg.params.update
    if u.sessionUpdate == "agent_message_chunk" and u.content.type == "text" then
      io.write(u.content.text)
    end
  elseif msg.method == "session/request_permission" then
    local reject = "reject"
    for _, opt in ipairs(msg.params.options or {}) do
      if opt.kind == "reject_once" or opt.kind == "reject_always" then
        reject = opt.optionId
      end
    end
    send(job, {
      jsonrpc = "2.0",
      id = msg.id,
      result = { outcome = { outcome = "selected", optionId = reject } },
    })
  elseif msg.result and msg.result.sessionId then
    -- stash and prompt; see comment in M.start
  end
end

return M
```

## usage

```lua
local kage = require("kage").start({ model = "anthropic:claude-sonnet-4-6" })
```

To make it usable: capture `sessionId` from the `session/new`
response, then send `session/prompt` with the user's text as a
`ContentBlock` array; replace the `session/request_permission` branch
with `vim.ui.select` so a human approves each tool call; and route
`agent_message_chunk` / `agent_thought_chunk` content into a scratch
buffer. Send `{ method = "session/cancel", params = { sessionId =
session } }` (a notification, no `id`) to stop the in-flight turn.
