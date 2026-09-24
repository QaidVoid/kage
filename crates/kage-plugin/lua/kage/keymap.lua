-- `kage.keymap.set(mode, lhs, rhs, opts)` and `kage.keymap.del(mode,
-- lhs)` over `kage.api.keymap_set` and `kage.api.keymap_del`. `mode` is
-- one mode letter or a list of them, and each mode gets its own entry.

local set, del = kage.api.keymap_set, kage.api.keymap_del
local error, ipairs, type = error, ipairs, type

local function modes(mode, name)
  if type(mode) == "string" then
    return { mode }
  end
  if type(mode) ~= "table" then
    error("kage.keymap." .. name .. ": mode must be a letter or a list of letters", 3)
  end
  return mode
end

kage.keymap = {}

function kage.keymap.set(mode, lhs, rhs, opts)
  for _, m in ipairs(modes(mode, "set")) do
    set(m, lhs, rhs, opts)
  end
end

function kage.keymap.del(mode, lhs)
  for _, m in ipairs(modes(mode, "del")) do
    del(m, lhs)
  end
end
