-- Shared helpers for the embedded stdlib. Every stdlib module receives
-- the same private table of host internals as its chunk argument; this
-- module runs first and adds the helpers the later modules use.

local internal = ...
local log = kage.log
local warned = {}

--- Log `message` as a warning the first time `key` is seen.
function internal.warn_once(key, message)
  if warned[key] then
    return
  end
  warned[key] = true
  log("warn", message)
end
