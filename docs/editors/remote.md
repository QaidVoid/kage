# remote

`kage serve` speaks the same Agent Client Protocol as `kage rpc`, but
over WebSocket instead of stdio. Editors and ACP clients on other
machines drive the same kage, on the same sessions and the same
engine, through one endpoint guarded by a token.

    editor (any machine)  --wss or ssh tunnel-->  kage serve  -->  one engine

## start the server

```sh
kage serve
```

The startup output on stderr carries everything a client needs:

    kage serve: token file /home/you/.local/share/kage/remote-token
    kage serve: connect: ws://127.0.0.1:7433/acp?token=<64 hex chars>
    kage serve: web UI: http://127.0.0.1:7433/

The `connect:` line is the URL clients open, and the only place the
token is ever printed. Copy it into the client's server URL field.
The `web UI:` line appears when the server is also serving the browser
client (below) and names the page URL, never the token.

Flags:

- `-m, --model <provider/model>` pins the model, exactly as for
  `kage rpc`.
- `--system <text>` overrides the system-prompt role.
- `--host <addr>` changes the bind address. The default `127.0.0.1`
  listens on loopback only.
- `--port <n>` changes the TCP port. The default is 7433, and
  `--port 0` picks a free port and shows it in the connect URL.
- `--rotate-token` replaces the stored token before serving.
- `--web-dir <dir>` serves the browser client bundle at `/` from
  `dir`. The default is a `web/` directory beside the executable; a
  missing or empty directory only disables the web UI, `/acp` keeps
  working.

Credentials resolve as for the TUI and `kage rpc` (OS keyring,
`kage auth login`, or an API-key env var). With no provider configured
`kage serve` prints a message and exits non-zero.

Stop the server with ctrl-c or SIGTERM: every running prompt is
cancelled, the session files are closed, and the process exits with
status 0. A second signal exits at once without waiting.

## the token

Every request must present a 256-bit token, in one of three forms.
All three are accepted on every connection; pick whichever the client
supports.

| form                     | where it goes                                                    |
| ------------------------ | ---------------------------------------------------------------- |
| bearer header            | `Authorization: Bearer <token>`                                   |
| subprotocol entry        | `Sec-WebSocket-Protocol: kage.<token>` (or `acp.<token>`)         |
| query parameter          | `?token=<token>` appended to the URL                              |

The connect URL printed at startup uses the query form, which works
in every browser client. A subprotocol entry is echoed back in the
`101` response. Browsers cannot set headers on a WebSocket, which is
why the subprotocol and query forms exist.

The token lives in `$XDG_DATA_HOME/kage/remote-token` with mode 0600,
next to `auth.json`. It survives restarts, so the connect URL keeps
working. `kage serve --rotate-token` replaces it with a fresh one and
prints a new connect URL; every client holding the old URL must be
sent the new one. Rotate when a URL has leaked or a collaborator
leaves.

## loopback default and the warning

By default the server binds `127.0.0.1`, so only processes on the
same machine can connect. A remote machine reaches it through an SSH
tunnel (below), which needs no exposure at all.

When `--host` names a non-loopback address, kage prints a plain
warning on stderr: the connection has no TLS, so anyone who learns
the connect URL can drive kage on that machine, with its credentials
and files. Keep the token secret and prefer a tunnel or a
TLS-terminating proxy.

## ssh tunnel

The simplest way in from another machine. On the machine running
kage:

```sh
kage serve
```

On the client machine:

```sh
ssh -N -L 7433:127.0.0.1:7433 you@kage-host
```

Then point the client at the local end of the tunnel. The token
protects the endpoint, and SSH encrypts everything between the
machines.

    client  -->  ws://127.0.0.1:7433/acp?token=...  (local)  --ssh-->  kage

## reverse proxy for wss

Clients that require `wss://` (browsers on HTTPS pages, some hosted
UIs) connect through a reverse proxy that terminates TLS and forwards
to kage. A minimal nginx site:

```nginx
server {
    listen 443 ssl;
    server_name kage.example.com;

    ssl_certificate     /etc/letsencrypt/live/kage.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/kage.example.com/privkey.pem;

    location /acp {
        proxy_pass http://127.0.0.1:7433;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
    }
}
```

kage still checks the token on every connection, so the proxy needs
no auth of its own. Run `kage serve` on loopback as above, and let
the proxy own the certificate. The URL clients use becomes
`wss://kage.example.com/acp?token=...`.

## attach, reconnect and replay

A session is not tied to the connection that opened it. When a client
loads a session another client has open (`session/load`), it attaches
to the live one instead of reopening the file, and it catches up in
order: the recorded transcript, then the turn in progress (partial
text and the tool call in flight), then the open permission asks.
Nothing arrives twice, and nothing that happens after the attach is
withheld.

So a dropped connection loses nothing: reconnect, load the same
session id, and continue from what is on screen. Editors that list
sessions show the same thread with its title and history.

## several clients on one session

Any number of clients can watch one session at once. Every attached
client sees the streamed replies, tool calls and usage. When a tool
needs approval, every attached client is asked, and the first answer
wins: the tool runs (or is refused) once, and the clients that did
not answer get their dialogs dismissed. Answer twice and the second
answer changes nothing.

## one prompt at a time

A session runs one prompt at a time. While a run is out, a prompt
from another client fails with a busy message instead of interleaving
with the run; cancel the running prompt (`session/cancel`) or wait
for it to end. Prompts from the client that owns the run are queued
the same way as in `kage rpc`.

## the TUI joins a session serve hosts

A session open in `kage serve` is locked to it, so another kage
process cannot append to its file. `kage resume <id>` (or
`kage resume --last`) on such a session does not fail on the lock:
the TUI finds the serve hosting it and attaches, showing "attached to
kage serve (pid N)". It is one more client of the session, like an
editor: it gets the transcript and the turn in progress, its prompts
and approvals go to serve, and the busy and first-answer rules above
apply to it too.

Serve listens for this on a unix socket only its own user can reach:
`serve-<pid>.sock` beside a `serve-<pid>.json` record, in
`$XDG_RUNTIME_DIR/kage` (or `~/.local/share/kage/run` without a
runtime directory). Both go away when serve stops. There is no token;
the socket checks that the TUI runs as the same user.

While attached, Lua runs in serve, so the TUI loads no plugins and no
`init.lua` (keymaps, options and themes from `config.toml` still
apply). Starting a new session, cloning, switching to a fork or
resuming another session is refused with a notice; quit and start
kage again for those. When serve stops, the TUI shows "serve stopped;
session detached" and takes no more prompts. If no running serve
hosts the locked session, the TUI says the session is open in another
kage process, as before.

This works on unix systems only.

## idle sessions close

A session no client is attached to, with no run in flight and no open
approval, is closed by the server: its file is already on disk, and
the engine frees the MCP servers and plugin runtimes it held. Loading
the session again reopens it from the file with a full replay. This
keeps a long-lived `kage serve` from accumulating per-session
resources nobody is using.

## threat model, in plain words

The token is the whole of the security. Anyone who has it can drive
kage: read and write files the process can read and write, run shell
commands under your user, and spend your provider credits, subject
only to your permission settings. Anyone without it can learn only
that the port exists.

- The connection is **not encrypted**. On a shared network, an
  observer can read everything, token included. Use loopback, an SSH
  tunnel, or a TLS-terminating proxy; do not point `--host` at a
  routable address across an untrusted network.
- The token file is mode 0600, and the token is compared in constant
  time. The token appears in exactly one place: the connect URL at
  startup. Log lines name peer addresses, never tokens.
- URLs with tokens leak through shell history, pastebins, and browser
  sync. Rotate after sharing ends, and prefer the header or
  subprotocol forms where the client supports them.
- There is no per-client identity: every holder of the token is the
  same user as far as kage is concerned. For a second person, prefer
  a separate `kage serve` under their own account.

In short: treat the connect URL like a password to your machine,
because it is one.

## limits

- **No TLS in kage itself.** Encryption comes from SSH or a reverse
  proxy, never from kage.
- **No Streamable HTTP.** The only endpoint is the WebSocket upgrade
  at `/acp`; plain HTTP requests to it are refused with `405`.
- **16 concurrent connections.** The next connection is refused with
  `503` until one closes.
- One shared engine. A runaway prompt in one session does not block
  others, but a very heavy session competes for the same machine.

## what the server logs

Every connection is logged on stderr prefixed `kage serve:`: connect,
attach, disconnect, and refusals with the reason (a missing or wrong
token is `401`, a non-upgrade request on `/acp` is `405`, an unknown
path is `404`, a path that tries to escape the web directory is a
`404` logged as a traversal, a full server is `503`). Refusals name
the peer address, never the value presented.

## web client

`kage serve` can serve the repository's own browser client, so a
browser needs nothing but the page URL and the token. It serves the
`web/` folder beside the `kage` executable, or the folder `--web-dir`
names.

The client ships as `kage-web.tar.xz` in the `kage-desktop-v<version>`
releases, which version apart from `kage`. Take it from the newest
one on the [releases page](https://github.com/QaidVoid/kage/releases).
Unpack it beside the `kage` executable to serve it with no flag, or
anywhere and name the folder:

```sh
version=0.1.0   # the newest kage-desktop release
curl -fsSL "https://github.com/QaidVoid/kage/releases/download/kage-desktop-v$version/kage-web.tar.xz" | tar -xJ
kage serve --web-dir web
```

The client works with any `kage` from 0.1.0 on. When the engine is
older than the client needs, or lacks a feature it uses, the client
says so in a banner.

To build the bundle yourself (the full commands and caveats are in
`gui/SPIKE.md`):

```sh
rustup target add --toolchain nightly wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129   # match gui/Cargo.lock
cd gui
cargo +nightly build --release --locked --target wasm32-unknown-unknown
wasm-bindgen --out-dir web --target web \
    target/wasm32-unknown-unknown/release/kage_desktop.wasm
mkdir -p web/assets/icons
cp "$(ls -d ~/.local/share/cargo/registry/src/*/gpui-kit-assets-0.7.0/assets/icons)"/*.svg \
    web/assets/icons/
cd ..
cargo run --bin kage -- serve --web-dir gui/web
```

Then open the page URL from the startup output:

    kage serve: web UI: http://127.0.0.1:7433/

The page shows a small form before the app boots: the server URL,
prefilled with `/acp` on the page's own origin, and the token. Enter
the token from the connect line and press Connect. The client dials
`/acp` on the same origin and presents the token as the
`kage.<token>` `Sec-WebSocket-Protocol` entry, so the token never
rides any URL; the serve log shows no query-string token. The three
token forms above stay available for non-browser clients.

The serving origin answers only from the bundle directory: `GET /`
with the page, `GET /<file>` with a file under `--web-dir`, everything
else `404`. A path is served only when it stays inside the directory
after its %-escapes are decoded, so `GET /../Cargo.toml` and its
encoded forms are refused.

What the headers on every asset response protect:

- `Content-Security-Policy` with `default-src 'none'`: the page may
  load scripts, styles, images and connections from its own origin
  only, WebAssembly compilation is allowed (`'wasm-unsafe-eval'`),
  and inline script, framing, form actions and base hijacking are
  closed off. A compromised asset cannot phone home.
- `X-Content-Type-Options: nosniff` and an explicit content type keep
  the browser from reinterpreting a file.
- `Referrer-Policy: no-referrer` keeps page URLs (and anything typed
  into them) out of other servers' logs.
- `Cache-Control: no-store` on the page so a rebuilt bundle is picked
  up on reload; the unhashed module and glue are cached for five
  minutes at most.
- `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp` on the page and the
  WebAssembly module isolate the browsing context, ready for a future
  build that uses `SharedArrayBuffer` threads.

The bundle makes zero non-self requests: the page loads `boot.js`,
the wasm-bindgen glue and the module from the serving origin, fonts
are embedded in the module, and the icon SVGs are fetched same-origin
on first use. There is no CDN, webfont, remote image or analytics
fetch, so the browser talks to one host, and that host is the kage
server the token belongs to.

## clients

The browser client of the repository is served by `kage serve`
itself; see "web client" above. See [zed](/editors/zed) for the stdio
setup; over the network the same client connects by URL. Any ACP
client that speaks WebSocket can connect: give it the connect URL
and, where it cannot set headers, enter the token as the client
directs.
