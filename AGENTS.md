# kage agent rules

kage is a coding agent written in Rust and extended in Lua. It ships a terminal UI, an ACP server for editors, a shared host, and a desktop client that also builds for the browser. These rules apply to every change in this repository.

## Workspace

- Crates live in `crates/<name>/`. Each crate has one clear responsibility. A new crate is justified only by a heavy dependency or a real boundary, not by tidiness.
- `xtask/` holds repository tooling (`cargo xtask ...`). It is not published.
- Every crate manifest inherits the workspace package fields and lints, and has a one-line `description`, which crates.io requires.
- The toolchain is pinned in `rust-toolchain.toml`. The workspace floor is the `rust-version` in the root manifest, and CI checks it against the committed lockfile. `kage-desktop` has its own, higher floor.
- `kage-desktop` is one client for native and wasm. Code it uses must keep building for `wasm32-unknown-unknown` with `--no-default-features`.

## Version control

- Check which VCS the checkout uses before the first VCS command: run `jj root`. If it succeeds, use `jj` for everything and never run `git` directly, because git commands can desync a colocated jj repository. If it fails, the checkout is plain git, so use `git` for everything.
- Never push and never open pull requests unless the user explicitly asks.
- Commit messages use the semantic format `type: imperative message` or `type(scope): imperative message`, for example `fix(tui): keep the cursor on resize`. Keep the first line within 68 characters. Keep bodies short.
- Commit a change once it is a coherent unit and all validation gates pass. Do not commit work that fails a gate.
- Do not hold changes too long either. Do not let more than one task's worth of work pile up uncommitted. Split large tasks into several validated commits.
- With jj, use `jj describe -m "<message>"` to describe the working-copy change and `jj new` to start the next one. Run `jj status` and `jj diff` before describing, so the commit contains only what you intend.
- With git, stage only the files you intend with `git add <paths>`, never `git add -A` or `git add .`, then run `git commit -m "<message>"`. Run `git status` and `git diff --staged` before committing. Never amend, rebase or force anything unless the user asks.

## Releases

- Releases go through release-plz only (`.release-plz.toml` and `.github/workflows/release-plz.yml`). On every push to `main`, it opens or updates a release PR with the version bump and changelog, and publishes to crates.io when that PR is merged. `.github/workflows/release.yml` then attaches the CLI, desktop and web builds to the same `v<version>` release.
- Never run `cargo publish`, never bump versions by hand, and never edit `CHANGELOG.md` by hand. release-plz derives all of them from the semantic commit messages, so write those messages with care. `feat` and `fix` commits appear in the changelog, and a `!` or a `BREAKING CHANGE:` footer marks a breaking change.
- Every crate shares one version, and the CLI, desktop app and web bundle always release together. There is one changelog, the root `CHANGELOG.md` of `kage-cli`. Crates keep no changelog of their own.
- When a new publishable crate is added, add it to `[workspace.dependencies]`, give it a `[[package]]` entry with `version_group = "kage"` in `.release-plz.toml`, and add it to `changelog_include` of `kage-cli`. A crate not meant for crates.io sets `publish = false`.

## Dependencies

- Only crates.io dependencies. No git dependencies, no path dependencies outside this workspace, and no `[patch]` sections. CI rejects them.
- A dependency used by more than one crate is declared once in the root `[workspace.dependencies]`, and members use `<name>.workspace = true`. Use `cargo add` and `cargo remove` for member manifests. Keep feature sets minimal with `default-features = false` where the defaults are not needed.
- Vet every dependency before adding it, and state the result in the commit body. Check that it:
  - is needed, because the standard library or an existing dependency cannot do the job in a few lines,
  - has a permissive license (MIT, Apache-2.0, BSD, Zlib, ISC or Unicode, and OFL for fonts),
  - is maintained, with a release in the last 12 months or a stable, finished scope,
  - pulls no git sources and downloads nothing at build time (inspect `build.rs` if one exists),
  - fits the MSRV, and builds for `wasm32-unknown-unknown` when `kage-desktop` or a crate it uses on the web depends on it,
  - adds no duplicate major versions (`cargo tree -d`) unless there is no alternative.
- Prefer re-exports from a dependency over adding the same crate separately, when the types must match.

## Code

- Keep it simple. Do not add abstractions, generics, traits, feature flags or configuration for cases nothing uses yet. If a change seems to need that, stop and ask first.
- The engine is synchronous and built on threads and channels. Wait on channels with `select!` instead of polling loops. An async runtime belongs only in a transport crate at the network edge, bridged to the engine by channels.
- Optimize only with a measurement in hand. When complexity is added for performance, say so in a doc comment or commit body together with the measurement that justified it.
- Library code must not panic on bad input. Use `expect` only for internal invariants, and write the message as the invariant that was broken.
- `unsafe_code` is forbidden across the workspace. Do not relax it.
- Source files are ASCII only. Write Unicode glyphs as `\u{..}` escapes. `cargo xtask check-ascii` enforces this over `crates/`, `xtask/src`, `plugins/` and `man/`.
- Code that runs at process start runs inside every test that spawns `kage`. Gate destructive or data-moving startup work behind the commands that need it.

## Generated files

These files are generated and committed. Never edit them by hand. Regenerate them in the same change that alters their source:

- The Lua type definitions `plugins/types/kage.lua`: `cargo xtask gen-lua-types` after changing the plugin API.
- The model catalog `crates/kage-provider/src/catalog/generated.rs`: `cargo xtask refresh-models`.
- The man page `man/kage.1`: `cargo build -p kage-cli && ./target/debug/kage gen-manpage --out man/kage.1` after changing CLI flags.

## Comments and documentation

- Every public item has a doc comment. That includes modules, structs, enums, variants, fields, traits, functions, methods, constants and type aliases. Say what it is for and how it behaves. Cover `# Errors` and `# Panics` where they apply.
- No narration and no obvious comments. Never restate what the code does. Avoid `//` comments inside function bodies. Put needed context in the item's doc comment, or make the code say it through names and small helpers. Write an inline comment only when nothing else can carry a non-obvious invariant, a platform quirk or a deliberate trade-off.
- Never reference planning documents, task IDs or roadmap files in code, comments, docs or commit messages.
- User-facing behavior changes update the docs in `docs/` (a VitePress site) and `README.md` when they describe it.
- Do not use em-dashes in any prose, and do not substitute them with `--`, a spaced hyphen or a semicolon. Write complete sentences.

## Tests

- Unit tests live in the module they test, in a `#[cfg(test)] mod tests` block. When they grow large, move them to `foo/tests.rs` for a module `foo.rs`, wired with `#[cfg(test)] mod tests;`.
- Integration tests that exercise a crate through its public API go in `crates/<name>/tests/`.
- Test behavior, not implementation. A test must fail if a user-visible behavior or a documented invariant breaks. Do not write tests for getters, derives, constructors or anything the compiler already guarantees.
- Name each test after the behavior it checks, for example `approval_denied_skips_tool_call`.
- Tests that spawn the `kage` binary must point `HOME` and every `XDG_*` directory at a temporary directory, so they never touch the user's real config or data.
- Tests that spawn a host must not turn features off with `KAGE_*` environment variables, because a config change makes kage skip the shared host under test. Write the test's `config.toml` instead.
- `kage-desktop` snapshot tests use `egui_kittest`. Update baselines with `UPDATE_SNAPSHOTS=1 cargo test -p kage-desktop` and review every changed image on purpose. Never accept snapshots blindly.
- Memory budgets live in `#[ignore]` tests of `kage-cli` and only mean something in release builds. Run them when a change could grow the host's memory.

## Validation gates

Run these before every commit and read their output:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace --exclude kage-desktop --all-targets
cargo xtask check-ascii
```

Add these when the change touches what they cover:

```sh
cargo xtask gen-lua-types --check
cargo xtask refresh-models --check --source file://xtask/fixtures/catalog.json
cargo build -p kage-cli && ./target/debug/kage gen-manpage --out man/kage.1 --check
cargo test -p kage-desktop --all-targets
cargo build --release --target wasm32-unknown-unknown -p kage-desktop --no-default-features
cargo test --release -p kage-cli --test shared_host -- --ignored
```

- Never claim work is done, fixed or passing without running the gates and reading their output. If a gate fails, fix it or report the failure exactly as it appeared. Never silence a lint with `allow` to pass a gate unless the lint is wrong for that spot, and then write the reason next to the `allow`.
- Verify behavior, not only compilation. Run the relevant test, or run kage itself with `HOME` and `XDG_*` pointed at a scratch directory, and check the result.
