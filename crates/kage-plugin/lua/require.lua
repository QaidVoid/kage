-- `require(name)` for the trusted user environment. `find` resolves a
-- module under the user's `lua/` directory and compiles it with the
-- user environment. Results are cached until the next reload, and a
-- module that requires itself, directly or not, raises.

local find, env = ...
local error, pcall, type = error, pcall, type
local loaded, loading = {}, {}

return function(name)
  if type(name) ~= "string" then
    error("require: module name must be a string", 2)
  end
  local value = loaded[name]
  if value ~= nil then
    return value
  end
  if loading[name] then
    error("loop requiring " .. name, 2)
  end
  local chunk = find(name, env)
  loading[name] = true
  local ok, result = pcall(chunk, name)
  loading[name] = nil
  if not ok then
    error(result, 0)
  end
  if result == nil then
    result = true
  end
  loaded[name] = result
  return result
end
