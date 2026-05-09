-- git-status.lua - read the current git branch and announce it at session start.
--
-- The plugin sandbox does not allow spawning processes, so this reads the
-- working tree's `.git/HEAD` directly. Shows the symbolic ref's branch
-- name (e.g. `main`) or the short hash for a detached HEAD. Dirty state
-- is reported by Phase 9's status-bar integration; this v0.1 example only
-- handles the branch line.

local function read_head()
    local ok, head = pcall(kage.fs.read, '.git/HEAD')
    if not ok or head == nil or #head == 0 then
        return nil
    end
    head = head:gsub('%s+$', '')
    local ref = head:match('^ref: (.+)$')
    if ref then
        local short = ref:gsub('^refs/heads/', '')
        return short
    end
    -- Detached: head is a raw 40-char hash.
    return head:sub(1, 7) .. ' (detached)'
end

kage.on('agent_start', function()
    local branch = read_head()
    if branch then
        kage.notify('git: ' .. branch)
    else
        kage.notify('git: not a repo')
    end
end)
