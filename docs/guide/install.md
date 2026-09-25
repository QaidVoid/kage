# install

kage builds from source on Linux, macOS, and WSL. There are no
prebuilt binaries yet.

## prerequisites

You need a working Rust toolchain at version 1.87 or newer. The
project pins 1.95 in `rust-toolchain.toml`, which rustup installs the
first time you build. You also need a C compiler: the Xcode
command-line tools on macOS, or a C compiler and `pkg-config` on
Linux. Lua 5.4 is vendored and built with kage, so no system Lua is
needed.

If you have nix and direnv, `nix develop` (or `direnv allow`) gives
you the exact toolchain plus `cargo-nextest` and `bacon`.

## clone and build

```bash
git clone https://github.com/QaidVoid/kage
cd kage
cargo build --release
```

The binary lands at `target/release/kage`. Put it on your `PATH` or
symlink it from `~/.local/bin`:

```bash
ln -s "$PWD/target/release/kage" ~/.local/bin/kage
```

## verify

```bash
kage --version
kage --help
```

If your shell cannot find `kage`, restart it or re-source your shell
init file so the new directory shows up in `PATH`.

Optional shell completion (`bash`, `zsh`, `fish`, `elvish`):

```bash
kage completions zsh > ~/.zfunc/_kage
```

## first-run setup

`kage init` writes a starter `~/.config/kage/config.toml`, installs
the Lua type definitions for plugin editing, and offers to save a
provider credential interactively. `--force` overwrites an existing
config, and `--non-interactive` skips the prompts:

```bash
kage init
```

`kage doctor` checks the install end to end. It prints the config,
data, state and cache directories it resolved, then checks the config
files, saved credentials, usable providers (custom ones included),
plugins, the sandbox and each MCP server. It exits non-zero if any
check fails:

```bash
kage doctor
```

## set up a provider

kage talks to LLM providers through API keys read from your
environment. Export one of the supported keys:

```bash
export ANTHROPIC_API_KEY=...
export OPENAI_API_KEY=...
export GEMINI_API_KEY=...
export ZAI_API_KEY=...
export ZAI_CODING_API_KEY=...
export DEEPSEEK_API_KEY=...
export GROQ_API_KEY=...
export MISTRAL_API_KEY=...
export CEREBRAS_API_KEY=...
export XAI_API_KEY=...
export OPENROUTER_API_KEY=...
export FIREWORKS_API_KEY=...
export MOONSHOT_API_KEY=...
export KIMI_API_KEY=...
export XIAOMI_API_KEY=...
```

Alternatively, save credentials with the built-in auth flow:

```bash
kage auth login anthropic   # prompts for the key without echo
kage auth login             # pick the provider from a list
kage auth list              # every provider and where its key comes from
kage auth logout anthropic  # remove a saved key
```

Saved credentials live at `~/.local/share/kage/auth.json`
(`$XDG_DATA_HOME/kage/auth.json`) with `0600` permissions. When kage
resolves a provider key it checks the environment variable first and
falls back to the saved key only when the variable is unset or empty.

Custom endpoints and per-provider overrides are covered in
[providers](/guide/providers).

## next step

[Quickstart](/guide/quickstart) walks you through your first session.
