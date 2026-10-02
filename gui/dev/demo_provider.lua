-- demo_provider: a scripted model for driving the GUI through the real
-- engine without spending tokens. Keywords in a prompt pick a scenario,
-- like the prototype's test keywords; the engine runs every tool call for
-- real, so approvals, plan mode, agents and swarms travel the real wire.
--
-- Install: copy into the plugin dir and grant
--   [plugins.capabilities]
--   demo_provider = ["provider"]
-- then pick the model `demo/script`. Type `help` for the keyword list.
--
-- Each request is answered from its history alone: the scenario comes
-- from the newest user prompt, and the step is the number of assistant
-- turns since it. Child agents get the generic scenario.

local caps = kage.request_capabilities({ "provider" })
if not caps.provider then
  kage.notify("demo_provider: grant the provider capability")
  return
end

local seq = 0
local function id()
  seq = seq + 1
  return string.format("demo_%d_%d", os.time(), seq)
end

local function text_of(message)
  local out = {}
  for _, block in ipairs(message.content or {}) do
    if block.type == "text" then
      out[#out + 1] = block.text
    end
  end
  return table.concat(out, "\n")
end

-- The newest real prompt (engine notes such as `[plan mode on]` are
-- skipped) and how many assistant turns answered it so far.
local function position(messages)
  local prompt, step = "", 0
  for i = #messages, 1, -1 do
    local m = messages[i]
    if m.role == "assistant" then
      step = step + 1
    elseif m.role == "user" then
      local t = text_of(m)
      if t ~= "" and t:sub(1, 1) ~= "[" then
        prompt = t
        break
      end
    end
  end
  return prompt:lower(), step
end

local function has_tool(req, name)
  for _, tool in ipairs(req.tools or {}) do
    if tool.name == name then
      return true
    end
  end
  return false
end

local PLAN = [[# Retry budget for kage-provider

## Goal
Stop the flaky retry test by giving the HTTP client one retry budget instead of a per-call counter.

## Steps
1. Add `RetryBudget` to `src/retry.rs` with the attempt count and the backoff base.
2. Thread it through `send_with_retry` in `src/http.rs`.
3. Replace the sleep in `tests/retry.rs` with a fake clock.

## Verify
- `cargo test -p kage-provider retry` passes ten runs in a row.
]]

local LONG = [[## Markdown check

Inline `code`, **bold**, *italic* and a [link](https://github.com/QaidVoid/kage).

1. ordered item
2. another item

- [x] a finished task
- [ ] an open task

> Quoted text renders as a block.

| column | value |
|---|---|
| context | 200k |
| cost | $0.42 |

```rust
fn main() {
    println!("hello from kage");
}
```
]]

local SCENARIOS = {
  help = {
    { say = "Test keywords: **fix** (reads, a grep, an edit, a shell run), **read**, **grep**, **edit**, "
      .. "**shell**, **fail**, **todo**, **approval**, **plan** (turn plan mode on first), **subagent**, "
      .. "**swarm**, **search**, **think**, **long**, **error**. Keywords combine in the order written." },
  },
  read = {
    { tools = { { "read", { path = "src/retry.rs" } }, { "read", { path = "src/http.rs" } }, { "ls", { path = "src" } } } },
    { say = "Read the retry code and the HTTP client. The retry counter lives per call." },
  },
  grep = {
    { tools = { { "grep", { pattern = "retry", path = "src" } } } },
    { say = "Found the retry call sites." },
  },
  edit = {
    { tools = { { "edit", { path = "src/retry.rs", old_str = "pub const MAX_ATTEMPTS: u32 = 3;", new_str = "pub const MAX_ATTEMPTS: u32 = 5;" } },
                { "write", { path = "notes/retry.md", content = "# Retry\n\nThe budget is shared per client.\n" } } } },
    { say = "Raised the attempt cap and wrote a short note." },
  },
  shell = {
    { tools = { { "shell", { command = "ls -la src && wc -l src/*.rs" } } } },
    { say = "The crate has three source files." },
  },
  fail = {
    { tools = { { "shell", { command = "sh -c 'echo test retry::backoff ... FAILED; exit 101'" } } } },
    { say = "The retry test fails; the backoff assertion is off by one." },
  },
  todo = {
    { tools = { { "todo_list", { todos = { { title = "Read the code", status = "in_progress" }, { title = "Write the fix", status = "pending" }, { title = "Run the tests", status = "pending" } } } } } },
    { tools = { { "todo_list", { todos = { { title = "Read the code", status = "done" }, { title = "Write the fix", status = "in_progress" }, { title = "Run the tests", status = "pending" } } } } } },
    { tools = { { "todo_list", { todos = { { title = "Read the code", status = "done" }, { title = "Write the fix", status = "done" }, { title = "Run the tests", status = "done" } } } } } },
    { say = "All three tasks are done." },
  },
  approval = {
    { tools = { { "shell", { command = "git status --short" } } } },
    { say = "That was the working tree status." },
  },
  plan = {
    { think = "Plan mode is on, so I only read and then present a plan.", tools = { { "read", { path = "src/retry.rs" } } } },
    { plan = true },
    { say = "Done: the plan is in place." },
  },
  subagent = {
    { tools = { { "agent", { agent = "explore", description = "map the retry call sites", prompt = "List every call to send_with_retry under src and summarize each in one line." } } } },
    { say = "The explorer mapped the call sites; two of them retry on 4xx, which is the bug." },
  },
  swarm = {
    { think = "Four independent files, one worker each.",
      tools = { { "swarm", { description = "review each source file", agent = "explore",
        prompt_template = "Review {{item}} for unwrap on runtime paths and report in two lines.",
        items = { "src/retry.rs", "src/http.rs", "src/lib.rs", "tests/retry.rs" } } } } },
    { say = "The swarm reviewed four files; `src/http.rs` has two unwraps on the response path." },
  },
  search = {
    { tools = { { "web_search", { query = "exponential backoff jitter", count = 5 } } } },
    { say = "The AWS write-up is the classic reference: full jitter spreads retries best." },
  },
  think = {
    { think = "The flaky test sleeps for real time, so a slow runner misses the deadline. A fake clock would make the backoff deterministic, and the budget belongs to the client, not the call.",
      say = "The fix is a fake clock plus a shared retry budget." },
  },
  long = { { say = LONG } },
  error = { { error = "decode" } },
}

local ORDER = { "help", "fix", "read", "grep", "edit", "shell", "fail", "todo", "approval", "plan", "subagent", "swarm", "search", "think", "long", "error" }

-- `fix` is a tour: the other scenarios played back to back.
local FIX = { "read", "grep", "edit", "shell" }

local function script_for(prompt)
  local hits = {}
  for _, key in ipairs(ORDER) do
    local at = prompt:find("%f[%w]" .. key .. "%f[%W]")
    if at then
      hits[#hits + 1] = { key = key, at = at }
    end
  end
  table.sort(hits, function(a, b) return a.at < b.at end)
  local steps = {}
  local function append(key)
    for _, s in ipairs(SCENARIOS[key]) do
      steps[#steps + 1] = s
    end
  end
  for _, hit in ipairs(hits) do
    if hit.key == "fix" then
      for _, key in ipairs(FIX) do append(key) end
    else
      append(hit.key)
    end
  end
  -- A text-only step ends the turn, so in a combined script it rides
  -- the next step's tool calls instead, or joins the next reply, and
  -- the run keeps going.
  local merged = {}
  local function text_only(step)
    return step and step.say and not step.tools and not step.plan and not step.error
  end
  for i, step in ipairs(steps) do
    local next_step = steps[i + 1]
    local prev = merged[#merged]
    if text_only(prev) and not prev.carry and text_only(step) then
      -- Two replies in a row are one reply: the first would end the run.
      merged[#merged] = { think = prev.think or step.think, say = prev.say .. "\n\n" .. step.say }
    elseif prev and prev.carry then
      merged[#merged] = { say = prev.say, think = step.think, tools = step.tools, plan = step.plan, error = step.error }
    else
      local carry = step.say and not step.tools and not step.plan and not step.error
        and next_step and (next_step.tools or next_step.plan) and not next_step.say
      merged[#merged + 1] = carry and { say = step.say, think = step.think, carry = true } or step
    end
  end
  steps = merged
  if #steps == 0 then
    -- Plain prompts and child agents: look around, then answer.
    return {
      { tools = { { "ls", { path = "." } } } },
      { say = "Looked at the project root: `src`, `tests` and `notes`. Nothing in it needs a change for this." },
    }
  end
  return steps
end

local function stream_text(emit, kind, text)
  local chunk = 24
  for i = 1, #text, chunk do
    emit({ type = kind, delta = text:sub(i, i + chunk - 1) })
    kage.sleep_ms(15)
  end
end

local function usage_for(req)
  local chars = 0
  for _, m in ipairs(req.messages or {}) do
    chars = chars + #text_of(m)
  end
  return { input = 1200 + math.floor(chars / 4), output = 180, cache_read = 0, cache_write = 0 }
end

kage.register_provider({
  id = "demo",
  display_name = "Demo (scripted)",
  supports_thinking = true,
  models = { { id = "script", name = "Scripted demo", context = 200000 } },
  stream = function(req, emit)
    local prompt, step = position(req.messages or {})
    local steps = script_for(prompt)
    local s = steps[math.min(step + 1, #steps)]
    emit({ type = "message_start" })
    kage.sleep_ms(120)
    if s.error then
      kage.provider_error(s.error, "demo: scripted provider failure")
    end
    if s.think then
      stream_text(emit, "thinking_delta", s.think)
    end
    local calls = s.tools or {}
    if s.plan then
      if has_tool(req, "exit_plan") then
        calls = { { "exit_plan", { plan = PLAN } } }
      else
        s = { say = "Turn plan mode on (shift+tab or /plan on) and ask again to see a plan review." }
      end
    end
    if s.say then
      stream_text(emit, "text_delta", s.say)
    end
    for _, call in ipairs(calls) do
      local call_id = id()
      emit({ type = "tool_call_start", id = call_id, name = call[1] })
      emit({ type = "tool_call_end", id = call_id, input = call[2] })
    end
    emit({
      type = "message_end",
      stop_reason = #calls > 0 and "tool_use" or "end_turn",
      usage = usage_for(req),
    })
  end,
})
