-- kage's own defaults, evaluated in a private environment before any
-- plugin or user config, so later layers can override every entry.
-- Keys the Rust editor grammar handles (motions, operators, readline
-- edits, Enter, Esc) are not here.

local dot = " \u{B7} "
kage.ui.set_slot("header", { left = { "title" }, right = { "widgets", "search" } })
kage.ui.set_slot("activity", { left = { "activity" } })
kage.ui.set_slot("input_pill", { left = { "working", "mode" }, right = { "thinking" } })
kage.ui.set_slot("footer", {
  left = { "hint" },
  right = { "model", "permission", "context", "tokens" },
  sep = dot,
})

local tips = {
  "Tab queues a message while kage works. Enter steers the running turn.",
  "Ctrl+F searches the conversation. Up and Down walk the matches.",
  "Shift+Enter inserts a newline. Ctrl+G edits the prompt in $EDITOR.",
  "Ctrl+O folds or unfolds the focused block. Alt+P and Alt+N move the focus.",
  "Start a prompt with ! to run a shell command. Type @ to complete a file path.",
  "Ctrl+V attaches an image from the clipboard.",
  "/settings changes the theme, the editor style and more.",
}
local blank = { text = "" }
kage.ui.set_slot("start", {
  lines = {
    "brand", blank,
    "model", "cwd", "permission", "thinking", blank,
    "sessions", "notices",
    { text = "Tip: " .. tips[math.random(#tips)], hl = "KageMuted" },
  },
})

local map, act = kage.keymap.set, kage.action

map("g", "<C-p>", act.OpenModelPicker, { desc = "model picker", group = "general" })
map("g", "<C-s>", act.OpenSessionPicker, { desc = "session picker", group = "general" })
map("g", "<F3>", act.OpenJumpPicker, { desc = "jump to a message", group = "general" })
map("g", "<S-Tab>", act.CycleThinkingLevel, { desc = "cycle thinking level", group = "general" })
map("g", "<C-v>", act.AttachClipboardImage, { desc = "attach image from clipboard", group = "general" })
map("i", "<Tab>", act.QueuePrompt, { desc = "queue the prompt until the run ends", group = "general" })
map("i", "<C-f>", act.BeginSearch, { desc = "search the conversation", group = "general" })

local conversation = "conversation"
map({ "i", "n" }, "<PageUp>", act.scroll(-10), { desc = "scroll up ten lines", group = conversation })
map({ "i", "n" }, "<PageDown>", act.scroll(10), { desc = "scroll down ten lines", group = conversation })
map("i", "<C-Up>", act.scroll(-1), { desc = "scroll up one line", group = conversation })
map("i", "<C-Down>", act.scroll(1), { desc = "scroll down one line", group = conversation })
map("i", "<C-Home>", act.ScrollToTop, { desc = "jump to top", group = conversation })
map("i", "<C-End>", act.ScrollToBottom, { desc = "jump to bottom", group = conversation })
map("i", "<M-p>", act.FocusPrev, { desc = "focus previous block", group = conversation })
map("i", "<M-n>", act.FocusNext, { desc = "focus next block", group = conversation })
map("i", "<C-n>", act.FocusNext, { desc = "focus next block", group = conversation })

local normal = "normal mode"
map("n", ":", act.BeginCommand, { desc = "command line", group = normal })
map("n", "/", act.BeginSearch, { desc = "search the conversation", group = normal })
map("n", "n", act.SearchNext, { desc = "next search match", group = normal })
map("n", "N", act.SearchPrev, { desc = "previous search match", group = normal })
map("n", "?", act.OpenHelp, { desc = "this reference", group = normal })
map("n", "[", act.FocusPrev, { desc = "focus previous block", group = normal })
map("n", "]", act.FocusNext, { desc = "focus next block", group = normal })
map("n", "<C-o>", act.ToggleFold, { desc = "fold or unfold the focused block", group = normal })
map("n", "zo", act.ToggleFold, { desc = "fold or unfold the focused block", group = normal })
map("n", "zc", act.ToggleFold, { desc = "fold or unfold the focused block", group = normal })
map("n", "zR", act.UnfoldAll, { desc = "unfold all blocks", group = normal })
map("n", "zM", act.FoldAll, { desc = "fold all blocks", group = normal })
map("n", "<C-w>", act.CyclePane, { desc = "switch between input and conversation", group = normal })
map("n", "gw", act.CyclePane, { desc = "switch between input and conversation", group = normal })

local buffer = "conversation pane"
map("b", "j", act.scroll(1), { desc = "scroll down one line", group = buffer })
map("b", "<Down>", act.scroll(1), { desc = "scroll down one line", group = buffer })
map("b", "k", act.scroll(-1), { desc = "scroll up one line", group = buffer })
map("b", "<Up>", act.scroll(-1), { desc = "scroll up one line", group = buffer })
map("b", "gg", act.ScrollToTop, { desc = "jump to top", group = buffer })
map("b", "G", act.ScrollToBottom, { desc = "jump to bottom", group = buffer })
map("b", "y", act.Yank, { desc = "yank the selection or the focused block", group = buffer })
map("b", "Y", act.YankFocusedBlock, { desc = "yank the focused block", group = buffer })
map("b", "v", act.EnterVisual, { desc = "start a visual selection", group = buffer })
