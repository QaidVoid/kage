-- `kage.ui.set_slot(name, spec)` and the full-row aliases
-- `kage.ui.set_header(fn)` and `kage.ui.set_footer(fn)` over
-- `kage.api.slot_set`. A row function receives the row width, takes
-- over the whole row and refreshes every 500 ms. `nil` restores the
-- default spec.

local slot_set = kage.api.slot_set
local error, type = error, type

kage.ui.set_slot = slot_set

local function takeover(name)
  local label = "kage.ui.set_" .. name
  return function(fn)
    if fn == nil then
      return slot_set(name, nil)
    end
    if type(fn) ~= "function" then
      error(label .. ": expected a function or nil", 2)
    end
    slot_set(name, {
      left = {
        {
          interval = 500,
          render = function(ctx)
            return fn(ctx.width)
          end,
        },
      },
    })
  end
end

kage.ui.set_header = takeover("header")
kage.ui.set_footer = takeover("footer")
