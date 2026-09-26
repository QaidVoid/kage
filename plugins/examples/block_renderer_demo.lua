-- block_renderer_demo.lua - own how a block draws, in pure Lua.
--
-- A plugin fully controls how a custom block kind renders. `kage.register_block_
-- renderer(kind, fn)` takes a `{ kind, text, width }` table and
-- returns the same shape `kage.ui.set_header` uses - a string, a
-- span table `{ text=, fg=, bold= }`, or an array of either (one per
-- line). The host keeps the conversation's focus rule and spacing;
-- the plugin owns everything inside.
--
-- `/card <title>` writes a `demo:card` entry whose body the renderer
-- below paints as a boxed, colored card. `/card` with no argument
-- opens the built-in picker (kage.ui.select) to choose a title -
-- there is no separate "open_picker" API because ui.select already
-- is the picker.

-- Appending entries writes session content, so this capability is
-- requested up front and must be granted in [plugins.capabilities].
kage.request_capabilities({ 'session_write' })

kage.register_block_renderer('demo:card', function(block)
    local title = block.text
    local w = math.max(20, math.min(block.width - 2, 60))
    local bar = string.rep('-', w)
    return {
        { text = '.' .. bar .. '.', fg = 'cyan' },
        {
            { text = '| ', fg = 'cyan' },
            { text = title, fg = 'green', bold = true },
            { text = string.rep(' ', math.max(0, w - #title - 1)) .. '|', fg = 'cyan' },
        },
        { text = "'" .. bar .. "'", fg = 'cyan' },
    }
end)

kage.register_command({
    name = 'card',
    aliases = { 'demo-card' },
    description = 'Render a demo:card block (picker if no title)',
    args = {
        { name = 'title', kind = 'text', optional = true, hint = 'card title' },
    },
    handler = function(_, _, parsed)
        local title = parsed.title
        if title == nil or title == '' then
            title = kage.ui.select('Card title', {
                'Hello from Lua',
                'Fully hackable UI',
                'Custom renderers',
            })
            if title == nil then
                return 'cancelled'
            end
        end
        kage.session.append_entry('demo:card', { title = title })
        return 'rendered card: ' .. title
    end,
})
