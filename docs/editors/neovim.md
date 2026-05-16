# neovim

`kage rpc` is a JSON-RPC 2.0 server over stdio with LSP-style
`Content-Length` framing. Neovim can drive it with a job and a small
frame parser; no plugin is required.

See [zed](./zed) for the full method and notification table. This
page is a minimal, dependency-free Lua client you can drop into your
config and grow from.

## a minimal client

```lua
local M = {}

local function frame(obj)
  local body = vim.json.encode(obj)
  return ("Content-Length: %d\r\n\r\n%s"):format(#body, body)
end

function M.start(opts)
  opts = opts or {}
  local args = { "rpc" }
  if opts.model then
    table.insert(args, "-m")
    table.insert(args, opts.model)
  end

  local buf, want = "", nil
  local id = 0

  local job = vim.system({ "kage", unpack(args) }, {
    stdin = true,
    stdout = function(_, data)
      if not data then
        return
      end
      buf = buf .. data
      while true do
        if not want then
          local s, e = buf:find("\r\n\r\n", 1, true)
          if not s then
            return
          end
          want = tonumber(buf:sub(1, s - 1):match("Content%-Length:%s*(%d+)"))
          buf = buf:sub(e + 1)
        end
        if not want or #buf < want then
          return
        end
        local msg = vim.json.decode(buf:sub(1, want))
        buf, want = buf:sub(want + 1), nil
        vim.schedule(function()
          M.on_message(msg)
        end)
      end
    end,
  })

  function M.send(obj)
    job:write(frame(obj))
  end

  function M.request(method, params)
    id = id + 1
    M.send({ jsonrpc = "2.0", id = id, method = method, params = params })
    return id
  end

  M.job = job
  M.request("initialize", {})
  return M
end

-- Override this to render events in your UI. By default it logs the
-- assistant's streamed text and answers permission prompts by
-- DENYING (never auto-approve; wire this to a real prompt).
function M.on_message(msg)
  if msg.method == "event" then
    local ev = msg.params or {}
    if ev.type == "text_delta" then
      io.write(ev.delta or "")
    end
  elseif msg.method == "permission/request" then
    vim.notify(
      ("kage wants to run `%s`"):format(msg.params.name),
      vim.log.levels.WARN
    )
    M.request("permission/respond", {
      id = msg.params.id,
      allow = false,
      reason = "answer me from a real prompt",
    })
  end
end

return M
```

## usage

```lua
local kage = require("kage").start({ model = "anthropic:claude-sonnet-4-6" })
kage.request("prompt", { prompt = "explain this file" })
```

To make it usable, replace `M.on_message`'s `permission/request`
branch with `vim.ui.select` (or a confirm dialog) so a human approves
each tool call, and route `event` payloads into a scratch buffer. The
event alphabet matches `kage -p --json`, so the same renderer works
for both. Send `{ method = "cancel" }` to stop the in-flight turn.
