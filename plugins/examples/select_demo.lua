-- select_demo.lua - demonstrate the blocking kage.ui.select dialog.
--
-- Registers a `/pick-color` command. Running it opens a picker; the
-- chosen value (or nil on cancel) is reported through kage.notify and
-- returned as the command's output. Because kage.ui.select suspends
-- the plugin coroutine, the handler reads like ordinary blocking code
-- even though the host services the dialog on another thread.

kage.register_command({
    name = 'pick-color',
    description = 'Pick a color via the ui.select dialog',
    handler = function()
        local color = kage.ui.select('Pick a color', { 'red', 'green', 'blue' })
        if color == nil then
            kage.notify('pick-color: cancelled')
            return 'cancelled'
        end
        kage.notify('pick-color: ' .. color)
        return 'you picked ' .. color
    end,
})
