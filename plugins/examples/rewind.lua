-- rewind.lua - conversation + file rewind, built on the capability tier.
--
-- Grant it in config.toml:
--
--   [plugins.capabilities]
--   rewind = ["session_write", "exec"]
--
-- session_write unlocks kage.session.entries / fork_to (the
-- conversation rewind); exec unlocks kage.exec so each turn's file
-- state can be snapshotted with git and restored on rewind. Either
-- capability may be withheld: without session_write the plugin
-- disables itself; without exec it still rewinds the conversation but
-- skips file restore.
--
-- Commands:
--   /undo          drop the last exchange: fork back to just before
--                  your most recent prompt and restore tracked files
--                  there. Repeat to walk further back, one exchange
--                  per call.
--   /redo          re-apply the file changes the last /undo or
--                  /rewind undid (alias: /rewind-redo)
--   /rewind        pick any earlier point and fork the conversation
--                  there, restoring tracked files to that turn
--   /rewind-status how many checkpoints / redo entries are held
--                  (alias: /undo-status)
--
-- Scope/limits (v0.1): file snapshots use `git stash create`, so they
-- cover tracked changes only (not untracked files) and require a git
-- work tree. The conversation fork is one-way: /redo restores files,
-- not the un-forked conversation (that would need a host primitive
-- kage does not expose to Lua yet). The very first exchange cannot be
-- undone into an empty session via a fork; start a new session.

local caps = kage.request_capabilities({ 'session_write', 'exec' })

if not caps.session_write then
    kage.notify('rewind: disabled (grant session_write in [plugins.capabilities])')
    return
end

local files = caps.exec

-- Per-turn checkpoints in chronological order: each is
-- { id = <entry id at turn end>, sha = <git stash sha or false> }.
-- `redo` holds stash shas of states a /undo or /rewind moved away from.
local checkpoints = {}
local redo = {}

local function git(args)
    local r = kage.exec({ cmd = 'git', args = args })
    if r.code ~= 0 then
        return nil, (r.stderr ~= '' and r.stderr or r.stdout)
    end
    return r.stdout, nil
end

local function in_git_repo()
    local out = git({ 'rev-parse', '--is-inside-work-tree' })
    return out ~= nil and out:find('true') ~= nil
end

local function snapshot()
    local out = git({ 'stash', 'create', 'kage-rewind checkpoint' })
    if out == nil then return false end
    local sha = out:gsub('%s+$', '')
    return sha ~= '' and sha or false
end

local function restore(sha)
    if not sha then return false end
    local _, err = git({ 'checkout', sha, '--', '.' })
    if err then
        kage.notify('rewind: file restore failed: ' .. err)
        return false
    end
    return true
end

local function last_entry_id()
    local entries = kage.session.entries()
    local last = entries[#entries]
    return last and last.id or nil
end

-- The newest checkpoint whose turn ended at or before `at`. Entry ids
-- are ULIDs, so a plain string compare orders them chronologically.
local function checkpoint_for(at)
    local best
    for _, c in ipairs(checkpoints) do
        if c.id <= at then best = c else break end
    end
    return best
end

-- The fork point that drops the last exchange: the entry right
-- before the most recent user message. Returns nil when there is no
-- earlier non-header entry to land on - the first exchange cannot be
-- undone into an empty session via a fork.
local function undo_target()
    local entries = kage.session.entries()
    local last_user
    for i = #entries, 1, -1 do
        if entries[i].kind == 'message' and entries[i].role == 'user' then
            last_user = i
            break
        end
    end
    if not last_user or last_user <= 1 then return nil end
    local prev = entries[last_user - 1]
    if prev.kind == 'header' then return nil end
    return prev.id
end

kage.on('turn_end', function()
    redo = {}
    if not files or not in_git_repo() then return end
    local id = last_entry_id()
    if not id then return end
    checkpoints[#checkpoints + 1] = { id = id, sha = snapshot() }
end)

kage.register_command({
    name = 'undo',
    description = 'Undo the last exchange: fork back before your most recent prompt and restore files',
    handler = function()
        local at = undo_target()
        if not at then
            kage.notify('undo: nothing to undo')
            return 'nothing to undo'
        end
        local restored = 'conversation only'
        if files and in_git_repo() then
            local pre = snapshot()
            if pre then redo[#redo + 1] = pre end
            local cp = checkpoint_for(at)
            if cp and restore(cp.sha) then
                restored = 'files + conversation'
            end
        end
        kage.session.fork_to(at)
        kage.notify('undo: reverted last exchange (' .. restored .. ')')
        return 'undone to ' .. at:sub(1, 8)
    end,
})

kage.register_command({
    name = 'rewind',
    description = 'Fork the conversation at an earlier point and restore files there',
    handler = function()
        local entries = kage.session.entries()
        local items = {}
        for i = 1, #entries do
            local e = entries[i]
            if e.kind == 'message' and e.role == 'user' then
                items[#items + 1] = {
                    label = string.format('user  %s  %s', e.ts:sub(12, 19), e.id:sub(1, 8)),
                    value = e.id,
                }
            end
        end
        if #items == 0 then
            kage.notify('rewind: no earlier user turn to rewind to')
            return 'nothing to rewind to'
        end

        local at = kage.ui.select('Rewind to', items)
        if at == nil then return 'cancelled' end
        if not kage.ui.confirm('Rewind?',
                'Fork the conversation here and restore tracked files. Later turns stay in the original session.') then
            return 'cancelled'
        end

        local restored = 'conversation only'
        if files and in_git_repo() then
            local pre = snapshot()
            if pre then redo[#redo + 1] = pre end
            local cp = checkpoint_for(at)
            if cp and restore(cp.sha) then
                restored = 'files + conversation'
            end
        end

        kage.session.fork_to(at)
        kage.notify('rewind: forking at ' .. at:sub(1, 8) .. ' (' .. restored .. ')')
        return 'rewound to ' .. at:sub(1, 8)
    end,
})

kage.register_command({
    name = 'redo',
    aliases = { 'rewind-redo' },
    description = 'Re-apply the file changes the last /undo or /rewind undid',
    handler = function()
        if not files then return 'redo unavailable (exec not granted)' end
        local sha = table.remove(redo)
        if not sha then
            kage.notify('redo: nothing to redo')
            return 'nothing to redo'
        end
        if restore(sha) then
            kage.notify('redo: re-applied file changes')
            return 're-applied'
        end
        return 'redo failed'
    end,
})

kage.register_command({
    name = 'rewind-status',
    aliases = { 'undo-status' },
    description = 'Show how many undo/rewind checkpoints and redo entries are held',
    handler = function()
        return string.format('rewind: %d checkpoint(s), %d redo, files=%s',
            #checkpoints, #redo, tostring(files and true or false))
    end,
})
