-- select_demo.lua - demonstrate the blocking kage.ui.* dialogs.
--
-- Registers `/pick-color` (kage.ui.select), `/confirm-delete`
-- (kage.ui.confirm), `/ask-name` (kage.ui.input), and
-- `/compose-note` (kage.ui.editor). Each opens a modal; the answer is
-- reported through kage.notify and returned as the command's output.
-- Because the dialogs suspend the plugin coroutine, the handlers read
-- like ordinary blocking code even though the host services them on
-- another thread.
--
-- Also binds Ctrl+Alt+K to a handler that opens the color picker, to
-- show kage.register_keybinding driving a blocking dialog from a key.

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

kage.register_command({
    name = 'ask-name',
    description = 'Prompt for a name via the ui.input dialog',
    handler = function()
        local name = kage.ui.input('What is your name?', 'e.g. Ada')
        if name == nil or name == '' then
            kage.notify('ask-name: no name given')
            return 'anonymous'
        end
        kage.notify('ask-name: ' .. name)
        return 'hello ' .. name
    end,
})

kage.register_command({
    name = 'compose-note',
    description = 'Edit a multi-line note via the ui.editor dialog',
    handler = function()
        local note = kage.ui.editor('Compose a note', 'TODO: ')
        if note == nil then
            kage.notify('compose-note: discarded')
            return 'discarded'
        end
        kage.notify('compose-note: saved ' .. #note .. ' chars')
        return note
    end,
})

kage.register_keybinding({ key = 'ctrl+alt+k', description = 'Quick color pick' }, function()
    local color = kage.ui.select('Quick pick', { 'red', 'green', 'blue' })
    kage.notify('quick-pick: ' .. (color or 'cancelled'))
    return color or 'cancelled'
end)
