-- `kage.on(event, handler)`: the classic subscription API as an alias
-- over `kage.api.autocmd_create`. The handler receives the event
-- payload and the call returns an idempotent `off`. An unknown event
-- name warns once instead of raising, so a plugin with a typo keeps
-- loading; a context-gated event without the grant is skipped the same
-- way and its `off` is a no-op.

local internal = ...
local create, del = kage.api.autocmd_create, kage.api.autocmd_del
local known = internal.events

function kage.on(event, handler)
  if type(handler) ~= "function" then
    error("kage.on: handler must be a function", 2)
  end
  if not known[event] then
    local name = tostring(event)
    internal.warn_once(name, "kage.on: unknown event '" .. name .. "' ignored")
    return function() end
  end
  local id = create(event, {
    callback = function(ev)
      return handler(ev.data)
    end,
  })
  -- A grant-denied subscription skips the hook and returns a nil id.
  if id == nil then
    return function() end
  end
  return function()
    del(id)
  end
end
