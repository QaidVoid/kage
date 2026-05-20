-- transform_demo.lua - show both transform-style hooks in action.
--
-- Two handlers, two concerns, one file so the contrast is obvious:
--
--   1. transform_context: the loop hands you the whole message history
--      right before it builds the provider request. Return a replacement
--      list to rewrite history in place, or nil to pass through.
--      Used here to scrub obvious secret tokens out of user-typed text
--      so an accidental paste does not get shipped to the model.
--
--   2. before_provider_request: the loop hands you the serialized
--      StreamRequest (model, messages, system, tools, thinking, etc.)
--      right before it goes to the provider. Return a replacement
--      request to patch any field, or nil to pass through.
--      Used here to append today's date to the system prompt so the
--      model has a stable answer for "what is the date today?".
--
-- Both hooks chain across plugins in registration order; a handler
-- that returns nil leaves whatever the previous handler produced.
-- A handler that errors is logged and skipped; the previous payload
-- survives.

local SECRET_PATTERNS = {
    'sk%-[%w_%-]+',           -- OpenAI/Anthropic-style keys
    'ghp_[%w]+',              -- GitHub personal access tokens
    'xox[abpr]%-[%w%-]+',     -- Slack tokens
    'AKIA[%w]+',              -- AWS access key ids
    '[Bb]earer%s+[%w%._%-]+', -- Authorization headers
}

local REDACTED = '[redacted]'

local function scrub(text)
    if type(text) ~= 'string' or #text == 0 then return text, false end
    local changed = false
    for _, pat in ipairs(SECRET_PATTERNS) do
        local new, n = text:gsub(pat, REDACTED)
        if n > 0 then
            text = new
            changed = true
        end
    end
    return text, changed
end

kage.on('transform_context', function(history)
    local scrubbed = 0
    for _, msg in ipairs(history) do
        if msg.role == 'user' and type(msg.content) == 'table' then
            for _, block in ipairs(msg.content) do
                if block.type == 'text' then
                    local new_text, changed = scrub(block.text)
                    if changed then
                        block.text = new_text
                        scrubbed = scrubbed + 1
                    end
                end
            end
        end
    end
    if scrubbed > 0 then
        kage.notify(string.format('redact: scrubbed %d block(s)', scrubbed))
    end
    return history
end)

kage.on('before_provider_request', function(req)
    local stamp = 'Today is ' .. os.date('%Y-%m-%d') .. '.'
    if req.system == nil or req.system == '' then
        req.system = stamp
    elseif not req.system:find(stamp, 1, true) then
        req.system = req.system .. '\n\n' .. stamp
    end
    return req
end)
