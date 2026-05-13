# install

kage builds from source on Linux, macOS, and WSL. There are no
prebuilt binaries yet.

## prerequisites

You need a working Rust toolchain at version 1.86 or newer (the
project pins 1.95 via `rust-toolchain.toml`, which rustup installs
automatically the first time you build). On macOS that means Xcode
command-line tools; on Linux you also need `pkg-config` and a Lua 5.4
development package, but the bundled `mlua` feature flag vendors Lua
so you can skip a system Lua install.

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

## set up a provider

kage talks to LLM providers through API keys read from your
environment. Export one of the supported keys:

```bash
export ANTHROPIC_API_KEY=...
export OPENAI_API_KEY=...
export GEMINI_API_KEY=...
export ZAI_API_KEY=...
export ZAI_CODING_API_KEY=...
```

Alternatively, save credentials with the built-in auth flow:

```bash
kage auth login anthropic
kage auth list
```

Saved credentials live at `~/.config/kage/auth.json` with `0600`
permissions.

## next step

[Quickstart](/guide/quickstart) walks you through your first session.
