# install

kage runs on Linux, macOS, and WSL. Download a release, install it
from crates.io, or build it from source.

## release downloads

Every [GitHub release](https://github.com/QaidVoid/kage/releases)
carries the `kage` binary for each platform, in two archives:

| Archive | Holds |
| --- | --- |
| `kage-<platform>.tar.gz` | `kage`, the man page, the license and the readme |
| `kage-<platform>-web.tar.gz` | the same, plus the browser client in a `web/` folder |

`<platform>` is `x86_64-linux`, `aarch64-linux` or `aarch64-macos`.
The Linux binaries are static, so they run on any distribution.

```bash
curl -fsSL https://github.com/QaidVoid/kage/releases/latest/download/kage-x86_64-linux.tar.gz | tar -xz
install -m 755 kage-x86_64-linux/kage ~/.local/bin/kage
```

Take the `-web` archive if you want `kage serve` to hand out the
browser client. It serves the `web/` folder beside the executable
without any flag, so keep the two together: move the whole folder
where you like and link the binary onto your `PATH`. The browser
client also ships on its own as `kage-web.tar.xz`, for an install
that came from crates.io; unpack it anywhere and pass its `web/`
folder to `kage serve --web-dir`. [remote](/editors/remote#web-client)
has the rest.

Every file has a `.sha256` beside it, and every archive a build
provenance attestation that the GitHub CLI checks:

```bash
gh attestation verify kage-x86_64-linux.tar.gz -R QaidVoid/kage
```

## crates.io

```bash
cargo install kage-cli --locked
```

This builds `kage` with your own toolchain, at Rust 1.89 or newer, and
needs a C compiler. It carries no browser client.

## from source

You need a working Rust toolchain at version 1.89 or newer. The
project pins 1.95 in `rust-toolchain.toml`, which rustup installs the
first time you build. You also need a C compiler: the Xcode
command-line tools on macOS, or a C compiler and `pkg-config` on
Linux. Lua 5.4 is vendored and built with kage, so no system Lua is
needed.

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

## desktop client

`kage-desktop` is a window onto the same engine: it starts `kage rpc`
and shows its sessions. Each release carries it too:

| Platform | Files |
| --- | --- |
| Linux, `x86_64` and `aarch64` | `kage-desktop-<arch>-linux.onelf` and `kage-desktop-<arch>-linux.tar.xz` |
| macOS, `aarch64` and `x86_64` | `kage-desktop-<arch>-macos.tar.xz` |
| Windows, `x86_64` | `kage-desktop-x86_64-windows.zip` |

On Linux, the `.onelf` file is one portable executable with its
libraries and a slimmed Vulkan driver stack inside, so it runs on any
distribution. `--onelf-integrate` adds it to your desktop's app menu:

```bash
chmod +x kage-desktop-x86_64-linux.onelf
./kage-desktop-x86_64-linux.onelf --onelf-integrate
```

The `.tar.xz` holds the bare binary instead, which loads the system's
display libraries and needs glibc 2.35 or newer. The macOS builds are
not signed yet, so clear the quarantine flag once:

```bash
xattr -d com.apple.quarantine kage-desktop
```

The desktop client needs `kage` itself: it runs the one on your
`PATH`, or the one Settings > Connection > kage binary names.

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
plugins and each MCP server. It exits non-zero if any
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
