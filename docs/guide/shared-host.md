# shared host

On Linux and other Unix systems, every `kage rpc` connects to one
background kage engine per user, the shared host. Editors and the
desktop app start `kage rpc`, so they all drive the same engine: a
session open in Zed and in the desktop app is one live session, and a
turn started in one streams into the other.

Nothing needs to be started by hand. The TUI joins the same host, so
a session open in the terminal is one live session with the ones an
editor sees. Print mode and `kage serve` still run engines of their
own, and Windows keeps a private engine per `kage rpc`.

## how it starts and stops

The first `kage rpc` finds no host and starts `kage host run --detached`
in the background. Later ones connect to it. When several start at
once, one host wins and the rest connect to it.

The host exits 10 minutes (`[host] idle_timeout_secs`) after its last
client leaves, once no turn is running and no permission or question
is waiting. A running turn keeps it alive, so work started in an
editor finishes even if the editor closes. A permission or question
that turn asks while no client is connected waits for the next client
that opens the session, and keeps the host running until then. If no
client comes within 12 hours of the last one leaving, the host declines
it: a permission is denied and a question is answered with no answers,
so the turn goes on, and the host can then go idle. SIGINT and SIGTERM
stop it.

Its files live in the runtime directory (`$XDG_RUNTIME_DIR/kage`, else
`$XDG_DATA_HOME/kage/run`, mode 0700):

| File | What it is |
| --- | --- |
| `host.sock` | The socket clients connect to, mode 0600. Only processes of the same user are served. |
| `host.json` | The running host's `pid`, `version`, `protocol`, `socket` and `started` time, and `web_error` when the web endpoint could not start. Written once it accepts clients and removed when it exits. |
| `host.lock` | Held for the host's whole life, so only one runs at a time. `kage serve` holds it too, so a serve and a host cannot run alongside each other. |

A background host has no terminal, so it writes to
`$XDG_STATE_HOME/kage/host.log`, which moves aside to `host.log.1` past
1 MiB.

## managing it

```sh
kage host status    # pid, version, start time, socket, log and web errors, or exit 1
kage host stop      # SIGTERM, then SIGKILL after 10 seconds
kage host logs      # print host.log
kage host run       # run it in the foreground, to watch its output
```

`kage host stop` signals the pid in `host.json` only when that process
runs `kage host run`, which it reads from the process's command line
(`/proc` on Linux, `ps` elsewhere). A host killed with SIGKILL or by the
out-of-memory killer leaves its socket and record behind, and the pid
can later belong to an unrelated process; the command then reports a
stale record and signals nothing. The check trusts the command line, so
it does not tell a host from another process started with the same
arguments. The next host or `kage serve` removes the stale files.

Stopping the host disconnects its clients and cancels running turns.
The next `kage rpc` starts a fresh one. `kage host run --idle-secs 60`
overrides the idle timeout for that host.

While a `kage serve` holds the lock instead, `kage host status` and
`kage host stop` say so, since a serve cannot be stopped that way, and
a `kage rpc` that would start a host falls back to a private engine
after a moment, because a serve and a host cannot run alongside each
other.

## configuration

The user config's `[host]` table sets:

```toml
[host]
share = true             # false: every kage rpc runs a private engine
idle_timeout_secs = 600  # how long an unused host stays up
web = false              # true: the host also serves the web endpoint
port = 7433              # the web endpoint's TCP port, when web = true
bind = "127.0.0.1"       # the web endpoint's address, when web = true
```

A project's `.kage/config.toml` never sets it.

## the web endpoint

With `[host] web = true`, the host also serves what `kage serve`
serves: the WebSocket endpoint at `/acp` and the web client's assets,
with the same token. It is off by default, and the default bind is
loopback. Setting `bind` to another address serves the endpoint to
other machines, which the host warns about in its log, and the token
is all that guards it.

If the endpoint cannot start, because the port is taken or the bind
address is not usable, the host logs the reason and runs on without it,
so local clients are not affected. `kage host status` shows
`web      not running: <reason>`, and so does `web_error` in `host.json`.
Set `[host] port` to a free port and restart the host to get the
endpoint.

The endpoint lives only while the host does. Nothing starts a host for
a web client, and the host still exits after `idle_timeout_secs` with
no clients and no work, which closes the endpoint. A web client
connected at that moment counts as a client and keeps the host up, but
one that dials in as the host exits is refused. To keep the endpoint up,
start `kage host run` yourself with a long `--idle-secs`, or raise
`idle_timeout_secs`; `kage serve` has no idle exit.

A web client connects like a serve client: the token lives in
`$XDG_DATA_HOME/kage/remote-token`, and the ready-to-open URL beside
it in `connect.txt`. A web client holds the host like a local one, and
every client, local and web, shares the host's budget of 64
connections. That is not the 16 of `kage serve`, which serves web
clients only. Past the budget the host refuses the next local client,
and the web endpoint answers `503`.

The web client bundle is not installed by default. The host serves
whatever lies under `$XDG_DATA_HOME/kage/web`; to fill it, run
`kage serve --fetch-web` while no host or serve holds the lock, stop
that serve, and start the host again.

`kage serve` stays as the explicit foreground host for remote and
headless machines, and neither can run while the other holds the
instance lock.

## a private engine

`kage rpc` keeps an engine of its own, as before the shared host, when:

- it gets `--standalone`, or `KAGE_STANDALONE` is set to anything but
  empty, `0`, `false`, `no` or `off`;
- the user config sets `[host] share = false`;
- it gets `-m`, `--system`, `--system-file` or `--yolo`, which change
  the engine for one client only;
- its environment carries `KAGE_*` variables that change the
  configuration, such as `KAGE_PERMISSIONS__TOOLS__READ__DEFAULT`. A
  host reads its own process environment once at startup, so it could
  not honor another client's overrides, and silently ignoring them
  would be wrong; such a client runs an engine of its own, which reads
  them. Variables that name no configuration key, such as
  `KAGE_STANDALONE` or `KAGE_HOST_IDLE_SECS`, do not count;
- the running host is another kage version. It prints a notice and
  uses a private engine until the old host goes idle and exits, so the
  next client starts one of its own version;
- the host cannot start. The reason is in `host.log`.

The TUI uses the shared host too, and a private host inside its own
process with `--standalone`, `[host] share = false`, a host of
another kage version, one that cannot start, or where the shared host
cannot run.

## the session environment

A shared host was started from one client's shell, but each client's
tools must run in that client's own environment: its virtualenv, its
`direnv` variables and its `PATH`. So every client sends its
environment to the host (`kage rpc` over its stdio, the TUI over the
socket), and the sessions it opens start these with it:

- the `shell` tool and `!` commands,
- stdio MCP servers, including restarts,
- plugin `kage.exec` processes, and what `kage.env` reads,
- the `jj` and `git` commands of worktree agents,
- the agent behind an `acp:` model, which also starts in the session's
  directory.

Agents a session starts inherit its environment. The environment is
never saved: a session reopened after it closed uses the environment
of the client that reopens it. A client that opens a session another
client still has open joins it as it is, environment included.
Variables that are not valid UTF-8 are left out, and an environment
over 1 MiB is not sent at all, so its sessions use the host's.

Only clients on the local socket can set it. A `kage serve` connection
cannot.

## what the host shares

Some settings are read once, when the host starts, and apply to every
client:

- provider credentials from the credential store, and API keys in
  environment variables of the client that started the host. Save keys
  with `kage auth login` so every client finds them;
- tool renames and plugin providers, from the user config only. A
  project `.kage/config.toml` still applies to sessions opened in that
  project, as before.

Your plugins and `init.lua` load once per project on the host, and
every session in that project shares them: their tools, hooks, MCP
servers and engine options apply whichever client opened the session.
A session that opens after a plugin, `init.lua`, the config or the
project's trust changed gets them freshly loaded, while sessions already
open keep what they started with. A project's plugins unload a minute
after its last session closes. A project with nothing to load, no
plugin under the plugins directory and no `init.lua` or `lua/` modules
of the user, builds no runtime for its sessions. Two things still come
from the plugins the host loaded at startup, not from `init.lua`:
providers a plugin registers, and `kage.acp` agents and permission
handlers.

A config or credential change made through a client, such as the
desktop's settings, reloads the providers for every client.

## memory

An idle host keeps about 2.2 MiB of private resident memory, and each
`kage rpc` relay under 2 MiB, measured on a Linux x86_64 release build. A
host that served sessions sheds back to within about a mebibyte of
that once its last client leaves: closed sessions' state drops at
once, and a project's runtime a minute after its last session. CI
fails a release build whose idle host goes over 4 MiB, whose relay
goes over 2 MiB, or that serves sessions and stays over 4 MiB.
