-- select_demo.lua - demonstrate the blocking kage.ui.* dialogs.
--
-- Registers `/pick-color` (kage.ui.select) and `/confirm-delete`
-- (kage.ui.confirm). Each opens a modal; the answer is reported
-- through kage.notify and returned as the command's output. Because
-- the dialogs suspend the plugin coroutine, the handlers read like
-- ordinary blocking code even though the host services them on
-- another thread.

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

kage.register_command({
    name = 'confirm-delete',
    description = 'Ask for confirmation via the ui.confirm dialog',
    handler = function()
        if kage.ui.confirm('Delete everything?', 'This cannot be undone.') then
            kage.notify('confirm-delete: confirmed')
            return 'deleting'
        end
        kage.notify('confirm-delete: declined')
        return 'kept'
    end,
})
