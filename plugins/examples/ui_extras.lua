-- ui_extras.lua - the PE.C extension surface in one file.
--
-- Three independent demos:
--
-- 1. kage.ui.set_header / kage.ui.set_footer: take over the top
--    status row and the bottom modeline. The render fn runs every
--    redraw and returns styled spans. Pass nil to either to restore
--    the built-in row.
--
-- 2. kage.add_autocomplete_provider: a prompt-input completer that
--    triggers on a ":" prefix and suggests a couple of emoji
--    shortcodes. Providers form a stack; this one only answers when
--    the token starts with ":", otherwise it returns {} and the
--    next provider (or the built-in @file completer) gets a turn.
--
-- 3. kage.on_terminal_input: a passive observer. It NEVER returns a
--    truthy value, so it never consumes a key (returning true would
--    swallow the event before any modal layer sees it). Calling the
--    returned `off` unregisters it. Prefer kage.register_keybinding
--    for "run X on chord Y"; this hook is for observing raw input.

-- 1. Header / footer chrome -------------------------------------------------

kage.ui.set_header(function(_width)
    return {
        { text = ' kage ', fg = 'black', bg = 'cyan', bold = true },
        { text = '  ui_extras demo', dim = true },
    }
end)

kage.ui.set_footer(function(_width)
    return { { text = ' header/footer owned by ui_extras.lua ', dim = true } }
end)

-- 2. Autocomplete provider --------------------------------------------------

local EMOJI = {
    [':tada:'] = 'party',
    [':rocket:'] = 'ship it',
    [':bug:'] = 'a defect',
}

kage.add_autocomplete_provider({
    name = 'emoji',
    complete = function(prefix, _ctx)
        if prefix:sub(1, 1) ~= ':' then
            return {}
        end
        local items = {}
        for code, detail in pairs(EMOJI) do
            if code:sub(1, #prefix) == prefix then
                items[#items + 1] = { value = code, label = code, detail = detail }
            end
        end
        return items
    end,
})

-- 3. Raw terminal-input observer -------------------------------------------

local seen = 0
local off = kage.on_terminal_input(function(ev)
    -- Observe only; returning a truthy value would consume the key.
    if ev.ctrl and ev.code == 'char' then
        seen = seen + 1
    end
    return false
end)

-- Expose an "unsubscribe" command so the observer can be turned off
-- at runtime without reloading the plugin.
kage.register_command({
    name = 'ui-extras-off',
    description = 'Stop the ui_extras raw-input observer',
    handler = function()
        off()
        return 'ui_extras: input observer detached after ' .. seen .. ' ctrl keys'
    end,
})
