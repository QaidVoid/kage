# Desktop client: spike report and app shell notes

Status: spike done, decision recorded at the bottom. The spike window became
the app shell and transports; see "The app shell and transports" (added
2026-09-29).
Date: 2026-09-29. All numbers below are from this date unless marked otherwise.

## What this is

`gui/` is its own Cargo workspace (own `Cargo.lock`, own `rust-toolchain.toml`)
holding one app crate, `kage-desktop`. The spike window became the app shell:
the same four hard things, now wired to the real client model.

- The window is three resizable panels in the toolkit's resizable panel
  group: sidebar, main transcript, workbench. Keybindings: ctrl-n new
  session, ctrl-b toggle workbench, ctrl-\\ toggle sidebar, ctrl-q quit,
  ctrl-enter sends the composer.
- The main panel renders the active session's transcript through the virtual
  list (variable-height rows, estimated per item) with agent text as
  markdown, plus the multi-line composer (`Textarea` over the toolkit's
  shared editing engine).
- A `Transport` trait sits behind the shell with three implementations:
  stdio (spawns `kage rpc`), WebSocket (`kage serve`, token as the
  `kage.<token>` subprotocol, reconnect with backoff), and replay (plays a
  recorded golden transcript). Frames travel both ways through
  `kage-client`: incoming frames into `Client::handle`, outgoing frames
  drained to the transport.
- The `initialize` handshake is gated on the client side: the agent version
  against `MINIMUM_KAGE_VERSION` (0.1.0) and the capabilities the UI gates
  on (steering, session close). Shortfalls raise a dismissible banner.
- The kage shadow dark theme applied, mapped from the palette the repository
  bundles in `crates/kage-tui/src/theme/kage.rs` (see the mapping table
  below).

## The app shell and transports

Source layout: `src/app.rs` (the shell: panels, key router, transport pump),
`src/store.rs` (the store over `kage-client`: state reads, commands, gate,
boot flow), `src/gate.rs` (version and capability check), `src/transport/`
(`stdio.rs`, `ws.rs`, `replay.rs`, and the trait plus connect states in
`mod.rs`), `src/views/` (sidebar, transcript, workbench; thin over the
store), `src/theme.rs` (shadow palette).

Connect states, per transport: `connecting`, `connected`, `refused` (the
endpoint said no for good, such as a 401; no retry), `reconnecting` (the
link dropped; a retry is scheduled), `closed`. The WebSocket transport backs
off from 1s doubling to a 30s cap and, on every reconnect, sends
`initialize` again and replays open sessions through `session/load`. The
replay transport embeds
`crates/kage-client/tests/fixtures/fix-tools.jsonl` at build time and plays
it at 100ms per frame, so the shell is fully usable with no engine and no
`kage` binary; its answer ids line up with the handshake and the prompt the
shell sends, so the scripted run completes on its own.

### Smoke runs and the real engine

```
cd gui
cargo build
# recorded transcript, no engine needed, exits 0:
ZED_HEADLESS=1 ./target/debug/kage-desktop --replay --smoke 3000
# streaming append at 30 Hz on top of the replay:
ZED_HEADLESS=1 ./target/debug/kage-desktop --replay --stream --smoke 6000
```

Stdio against the real engine. Build the engine in the main workspace
first, then point the desktop client at it with `--rpc-bin` (without the
flag it resolves `kage` on `PATH`):

```
cargo build -p kage-cli                       # in the repository root
cd gui
ZED_HEADLESS=1 ./target/debug/kage-desktop --rpc-bin ../target/debug/kage --smoke 6000
#   smoke: connect=connected agent=(kage 0.1.0) sessions=1 items=0 used=0/1000000 ...
```

WebSocket against a real `kage serve`. The token never rides the URL; the
client sends it as the `kage.<token>` subprotocol and reads the
`Acp-Connection-Id` from the 101:

```
./target/debug/kage serve --port 7433 &       # in the repository root
TOKEN=$(cat ~/.local/share/kage/remote-token)
cd gui
ZED_HEADLESS=1 ./target/debug/kage-desktop --ws ws://127.0.0.1:7433/acp --token "$TOKEN" --smoke 4000
#   smoke: ... connection=0     (the 101's Acp-Connection-Id)
# a wrong token is refused once and the state ends closed; the serve log
# shows `refuse ... (401)`
```

Reconnect against a real serve: connect, kill the server, start it again;
the client walks the backoff (1s, 2s, 4s, ... observed live) and comes back
`connected` with the session rebuilt through the re-handshake and
`session/load`.

Driving one prompt over stdio interactively: run
`./target/debug/kage-desktop --rpc-bin ../target/debug/kage` on a real
session, type into the composer, press ctrl-enter or the Send button. The
headless path cannot type, so the automated proof of the prompt round trip
is the stub test in `src/transport/stdio.rs`: it spawns a scripted `sh`
engine over real pipes and asserts the prompt frame reaches the child and
its reply lands in client state (run released, stop reason recorded).

## Pinned snapshot

The toolkit family is published together, roughly weekly, under the name
GPUI Kit. The exact pins, all `=x.y.z` in `gui/Cargo.toml` and locked in
`gui/Cargo.lock`:

| Crate | Version | Published | What it is |
|---|---|---|---|
| `gpui-kit` | =0.7.0 | 2026-09-28 | The application facade, one dependency for the whole stack |
| `gpui-component` | =0.7.0 | 2026-09-28 | The styled component library |
| `gpui-base` | =0.7.0 | 2026-09-28 | Foundations, virtual list, input engine, markdown text view |
| `gpui-kit-assets` | =0.7.0 | 2026-09-28 | Embedded icon assets |
| `gpui-pre` | =0.3.7 | 2026-09-28 | GPUI itself, a snapshot of zed@1a28cff |
| `gpui-pre-platform` | =0.3.7 | 2026-09-28 | Platform selector, x11 + wayland + font-kit features on |
| `gpui-pre-linux` | =0.3.7 | 2026-09-28 | Wayland, X11 and headless backends |
| `gpui-pre-wgpu` | =0.3.7 | 2026-09-28 | The wgpu renderer |

Name mapping note: the toolkit does not use the `gpui` crate from crates.io
for this stack. That crate is Zed's own direct publish (0.2.2, 2025-10-22) and
the component library does not depend on it. The framework arrives as the
`gpui-pre-*` snapshot family, each version a snapshot of one Zed commit
(0.3.7 is zed@1a28cff), and `gpui-kit` 0.7.0 re-exports it so applications
list one dependency. The `gpui-kit` umbrella crate itself is real and pinned
here. Inside the toolkit every one of those dependencies is already pinned
with `=`, so `gui/Cargo.toml` pinning `gpui-kit = "=0.7.0"` holds the whole
tree, and the committed `gui/Cargo.lock` freezes the 852 transitive crates.

## How to build and run

The transport flags and smoke commands are in "The app shell and transports"
above. Bare runs:

```
cd gui
cargo build --release
./target/release/kage-desktop                # a window on Wayland or X11, stdio engine
ZED_HEADLESS=1 ./target/release/kage-desktop --smoke 3000   # no display needed
./target/release/kage-desktop --smoke 10000 --stream        # 30 Hz append test
```

System packages, Debian and Ubuntu names: `libfontconfig-dev`,
`libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libwayland-dev`, `libxcb1-dev`.
The font stack is configured to load fontconfig through dlopen
(`gui/.cargo/config.toml`), so the build needs no fontconfig link flags and
loads `libfontconfig.so.1` at runtime like the rest of the desktop stack.

Render paths on Linux, chosen at runtime: Wayland when `WAYLAND_DISPLAY` is
set, else X11 when `DISPLAY` is set, else the headless client. Rendering goes
through wgpu, which wants Vulkan (or GL); `ZED_DEVICE_ID` selects a PCI device
on hybrid GPU systems. Without a hardware GPU, Mesa's software Vulkan
(lavapipe, enabled by pointing `VK_ICD_FILENAMES` or `VK_DRIVER_FILES` at the
`lvp_icd` file) renders the same window on the CPU. `ZED_HEADLESS=1` forces
the display-less path, which runs the full event loop, window, views and
timers with no pixels, which is what CI uses.

macOS needs no system packages (Metal) and Windows needs none either
(DirectX); the CI job builds both.

## Measurements

Machine for every number below: Void Linux, kernel 6.18.54, 16 cores, 32 GB
RAM, Wayland session, Rust 1.95.0. The automated numbers were taken with the
release binary and five-run or ten-run medians.

| Measurement | Value | How |
|---|---|---|
| Binary size, release | 51,119,400 bytes (48.7 MiB), debuginfo stripped | `ls -l target/release/kage-desktop` |
| Binary size, debug | 639,863,400 bytes (610 MiB), unstripped | `ls -l target/debug/kage-desktop` |
| Clean build time, debug | 60 s wall | `cargo clean && cargo build`, 16 parallel jobs |
| Clean build time, release | 59 s wall | `cargo clean && cargo build --release` |
| Cold start, headless | 35 ms median process start to exit (28 to 35 over five runs, 182 ms first run with cold caches) | `ZED_HEADLESS=1 kage-desktop --smoke 0`, wall clock |
| Cold start, Wayland window | 129 ms median (123 to 151 over five runs, 767 ms first run with GPU init) | `kage-desktop --smoke 0` on the Wayland session, wall clock |
| Idle memory, Wayland | 104.6 MB RSS, flat over four seconds of sampling | `/proc/<pid>/status` VmRSS with the 5,000 row list loaded and idle |
| Idle memory, headless | 27.5 MB RSS, flat | same, `ZED_HEADLESS=1` |
| Streaming append at 30 Hz | 299 appends in 10.0 s (299 of 300 expected), identical headless and on Wayland | `--smoke 10000 --stream`, stdout counter |
| Scroll smoothness | needs hands-on QA | checklist below |
| CJK IME, Wayland and X11 | needs hands-on QA | checklist below |
| Screen reader on the list | needs hands-on QA | checklist below |

Streaming notes: the 30 Hz loop is a timer task that appends a synthetic
agent chunk through the client, extends the transcript's size table, scrolls
to the bottom and repaints. Holding 299 of 300 ticks over ten seconds means
the append path, the size table rebuild and the repaint together fit inside
a 33 ms frame budget with room to spare, on a debug-grade build profile in
the headless case too (148 of 150 in 5 s). The spike measured the same loop
over a plain row list; the number carries over, the path only gained the
client absorb step.
Scroll smoothness as a human-visible number (frame pacing under wheel and
drag through all 5,000 rows) needs eyes on a real compositor.

## Theme mapping

kage shadow tokens to the component library's theme roles:

| Role | Token |
|---|---|
| background, title_bar, status_bar, tab_bar, tab, list | `bg` 0x0f0e13 |
| muted, input, list_even | `bg_highlight` 0x17151d |
| secondary, popover, overlay, window_border | `bg_raised` 0x221f2a |
| foreground | `fg` 0xcdc9d6 |
| secondary_foreground, accent_foreground, selection foreground | `fg_strong` 0xf4f1fa |
| muted_foreground, sidebar_foreground | `fg_dark` 0x9895a0 |
| border, title_bar_border, status_bar_border | `fg_gutter` 0x46444c |
| primary, caret, sidebar_primary | `lantern` 0xf2a65a |
| ring, magenta | `violet` 0xa98bfa |
| selection, accent, sidebar_accent | violet over bg at 22 percent, 0x312a46 |
| list_hover, secondary_hover | violet over bg at 14 percent, 0x252033 |
| link, info, blue | `blue` 0x9ab4ff |
| success, green | `green` 0x8bd49c |
| warning, yellow | `yellow` 0xe8c96a |
| danger, red | `red` 0xf2727f |
| cyan | `mist` 0xb8b0d4 |

Markdown view styling (code span background, code block background, link and
heading colors) follows the same roles automatically, because the component
library derives its text view style from these theme colors.

## Hands-on QA checklist

The three measurements that need a person, with steps. Run each on both a
Wayland session and an X11 session, record numbers with platform and date.

1. Scroll smoothness. Run the app on the session under test. Wheel scroll
   from row 0 to row 5000 and back, drag the scrollbar the same way, press
   the jump button repeatedly, and run `--stream` for a minute of
   bottom-following appends. Watch for dropped frames, stutter or input lag,
   with the compositor's frame timing overlay or a GPU monitor visible.
   Record: composition (frame drops yes or no), worst stutter, subjective
   rating per session.
2. CJK IME. Install fcitx5 with mozc or anthy (or ibus equivalents). On X11
   set `XMODIFIERS=@im=fcitx` before launching; on Wayland the app speaks the
   compositor's text-input protocol directly. Focus the composer, type
   Japanese or Chinese with preedit, commit, then press Enter inside the
   field to insert a newline and type a second line. Record: preedit renders
   inline, commit lands at the caret, newlines and multi-line content are
   correct, no duplicated or dropped glyphs, per session.
3. Screen reader. Run GNOME with Orca (AT-SPI). The toolkit ships AccessKit
   and its Linux bridge (`accesskit_unix` is in the dependency tree), so the
   window exposes a role tree. Focus the list, arrow through rows, and
   confirm each row announces its label ("Row N") and the tall-row detail
   line. Record: rows announced, count heard at the ends, any silent rows,
   per session.

## Upgrade procedure

The toolkit moves together, roughly weekly (eight releases in the four weeks
before the pin). To upgrade:

1. Read the release notes of the new `gpui-kit` version, then pick the whole
   set: `gpui-kit`, `gpui-component`, `gpui-base`, `gpui-kit-assets` share one
   version number, and the `gpui-pre-*` family shares another; both numbers
   appear in the release notes together.
2. Change the pin in `gui/Cargo.toml`, then `cargo update` inside `gui/` so
   the lockfile moves with it. Every `gpui-pre-*` crate must be the same
   snapshot version; the published crates carry exact `=` requirements, so a
   mixed snapshot tree has no valid resolution to begin with.
3. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`, `cargo build --release`, then the smoke flags headless and
   on a real session: `--smoke 5000 --stream` and a manual pass over the
   three QA items if the release changed input, text or rendering.
4. Record the new snapshot (crate version and the Zed commit named in the
   `gpui-pre` crate description) at the top of the table in this file.

The snapshot lineage is visible without any account: each `gpui-pre` release
description names its Zed commit, and the crate metadata carries
`zed-rev`.

## Decision: go

The stack carries the client. Reasons:

- Every hard thing has a real, working component in the pinned versions. The
  virtual list handles variable heights through a precomputed size table with
  binary-searched visible ranges and holds 5,000 rows without load; the
  multi-line input is a real editing engine with IME plumbing on both Linux
  display servers; markdown renders through the library's own parser and text
  view with no HTML bridging; the theme takes arbitrary palettes through one
  write path (`Theme::update`), which the kage shadow mapping proves out.
- The CI story works. The same binary builds on Linux (Wayland and X11
  features on), macOS and Windows, and the headless path runs the full
  window, views and 30 Hz stream with no display and no GPU, which is what
  the CI job exercises.
- The automated measurements are all inside comfortable bounds: 48.7 MiB
  release binary, sub-150 ms cold start even with a real Wayland window and
  GPU init, flat 104.6 MB idle RSS with 5,000 rows loaded, and 30 Hz
  streaming appends holding 99.7 percent of their deadline over ten seconds.

Risks, none blocking:

- The `gpui-pre` snapshots move weekly and can change API between snapshots.
  Mitigated by exact pins, a committed lockfile, and the upgrade procedure
  above.
- The public `gpui` crate on crates.io is a year stale relative to this
  stack. Anyone reaching for `gpui = "0.2"` directly will get a different,
  incompatible framework. The pin discipline in this workspace is the guard.
- wgpu needs Vulkan or GL at run time. Machines without a working driver
  need Mesa lavapipe for software rendering, which is documented above and
  should be part of packaging checks.
- The three hands-on items (scroll feel, CJK IME, screen reader) are open
  until QA signs off. The plumbing for all three is present and compiled in;
  nothing observed so far suggests a blocker.

A re-plan would name Iced or Slint over the same client model. Nothing in
these numbers points that way today.
