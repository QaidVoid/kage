-- tps.lua - tokens-per-second display.
--
-- Adds up output tokens reported by every `message_end` event during a run.
-- On `agent_end`, computes throughput against wall-clock and pushes a
-- one-line summary through `kage.notify`.
--
-- Drop into ~/.kage/plugins/ and load with `cargo run -p kage-cli -- ...`.

local started_at = nil
local total_output = 0

kage.on('agent_start', function()
    started_at = kage.now_ms()
    total_output = 0
end)

kage.on('message_end', function(ev)
    if ev.usage and ev.usage.output then
        total_output = total_output + ev.usage.output
    end
end)

kage.on('agent_end', function()
    if started_at == nil then return end
    local elapsed_ms = kage.now_ms() - started_at
    if elapsed_ms <= 0 then
        kage.notify(string.format('tps: %d output tokens (no elapsed time)', total_output))
        return
    end
    local tps = total_output * 1000.0 / elapsed_ms
    kage.notify(string.format(
        'tps: %d tokens in %.2fs (%.1f tok/s)',
        total_output, elapsed_ms / 1000.0, tps
    ))
    started_at = nil
    total_output = 0
end)
