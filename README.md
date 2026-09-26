# kage

A coding agent for your terminal. Written in Rust, configured and
extended in Lua, and small enough to read.

Status: pre-1.0. Things still move.

## What it does

- **A terminal UI built for long sessions.** Verb-first tool rows,
  grouped reads, live output from running commands, collapsible
  thinking, inline approvals, search, mouse selection, and a transcript
  printed when you quit. Modeless by default, with a vim mode if you
  want one.
- **Agents.** The model can hand work to child agents that run in
  parallel. Each one shows up as a live card, a pinned row above the
  prompt, and a full view you can open, steer or stop.
- **Configured in Lua.** `~/.config/kage/init.lua` sets options,
  keymaps, highlights, autocmds and the chrome around the prompt.
  Plugins add tools, commands, providers and custom blocks, each behind
  the capabilities it asks for.
- **Many providers, one model picker.** Anthropic, OpenAI, Gemini,
  DeepSeek, Z.ai, OpenRouter and other OpenAI-compatible APIs, plus
  your own endpoints in `config.toml`. A bundled model catalog knows
  each model's context window, inputs and thinking levels, and
  `kage models refresh` updates it on demand.
- **MCP.** Tools, resources (`@server:uri` mentions), prompts as
  `/server:prompt` commands, and OAuth login for remote servers.
- **Editors over ACP.** `kage rpc` speaks the Agent Client Protocol,
  so Zed, Neovim and other ACP clients can drive it, with sessions,
  approvals and agents intact.
- **Sessions you can come back to.** Every conversation is saved.
  Resume, clone or fork it, and compact it when it grows.
- **Permissions when you want them.** Tools run freely by default.
  Add rules, switch to ask mode, or approve per call, per session or
  for good.
- **Scriptable.** `kage -p` runs one prompt and prints the answer, or
  a JSON event stream with `--json`.

## Install

kage builds from source on Linux, macOS and WSL. You need Rust 1.88 or
newer and a C compiler. Lua is vendored.

```sh
git clone https://github.com/QaidVoid/kage
cd kage
cargo build --release
ln -s "$PWD/target/release/kage" ~/.local/bin/kage
```

With nix, `nix develop` gives you the pinned toolchain.

## Quick start

```sh
kage init                        # starter config, optional API key
kage                             # interactive TUI
kage -p "summarize this repo"    # one prompt, printed answer
kage resume --last               # pick up where you left off
kage doctor                      # check the setup
```

Keys come from environment variables such as `ANTHROPIC_API_KEY`, or
from `kage auth login`. Inside the TUI, `/` opens the command palette,
`ctrl+p` switches models and `?` lists every key.

A taste of `init.lua`:

```lua
kage.opt.theme = "tokyo-night"

kage.keymap.set("i", "<F2>", kage.action.OpenModelPicker, { desc = "switch model" })

kage.api.autocmd_create("tool_call", {
  pattern = "bash",
  callback = function(ev)
    kage.log("info", "bash: " .. tostring(ev.data.input.command))
  end,
})
```

## Docs

The guide lives in `docs/`: install, keybindings, commands, providers,
Lua config, agents, MCP, permissions, themes, plugins and editor setup.
Preview it with `cd docs && bun run dev`.

## Build and test

```sh
cargo build
cargo test --workspace
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo xtask check-ascii
```

## Credit

kage started from studying [pi](https://github.com/badlogic/pi-mono) by
Mario Zechner. Its agent loop and plugin-first design shaped kage early
on.

## License

MIT. See `LICENSE`.
