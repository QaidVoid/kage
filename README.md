# kage

A minimal, extensible coding agent in your terminal. Written in Rust, scriptable in Lua.

Status: pre-1.0.

## Credit

kage owes a heavy debt to [`pi-mono`](https://github.com/badlogic/pi-mono) by Mario Zechner. The agent loop, the plugin-first philosophy, and the event taxonomy all come from studying pi. Read pi first if you want to understand how kage thinks.

## Install

```sh
nix develop
cargo build --release
./target/release/kage
```

Toolchain pinned to 1.95.0, MSRV 1.86.

## Usage

```sh
export ANTHROPIC_API_KEY=...     # or OPENAI / GEMINI / ZAI
kage                             # interactive TUI
kage -p "..."                    # print mode
```

Docs live in `docs/`. Run `cd docs && bun run dev` to preview locally.

## Build and test

```sh
cargo build
cargo nextest run --all-targets
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

## License

MIT. See `LICENSE`.
