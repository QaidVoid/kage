# shared host

On Linux and other Unix systems, every `kage rpc` connects to one
background kage engine per user, the shared host. Editors and the
desktop app start `kage rpc`, so they all drive the same engine: a
session open in Zed and in the desktop app is one live session, and a
turn started in one streams into the other.

Nothing needs to be started by hand. The TUI, print mode and
`kage serve` still run engines of their own, and Windows keeps a
private engine per `kage rpc`.

## how it starts and stops

The first `kage rpc` finds no host and starts `kage host run --detached`
in the background. Later ones connect to it. When several start at
once, one host wins and the rest connect to it.

The host exits 10 minutes after its last client leaves, once no turn is
running and no permission or question is waiting. A running turn keeps
it alive, so work started in an editor finishes even if the editor
closes. A permission or question that turn asks while no client is
connected waits for the next client that opens the session, and keeps
the host running until then. SIGINT and SIGTERM stop it.

Its files live in the runtime directory (`$XDG_RUNTIME_DIR/kage`, else
`$XDG_DATA_HOME/kage/run`, mode 0700):

| File | What it is |
| --- | --- |
| `host.sock` | The socket clients connect to, mode 0600. Only processes of the same user are served. |
| `host.json` | The running host's `pid`, `version`, `protocol`, `socket` and `started` time, written once it accepts clients and removed when it exits. |
| `host.lock` | Held for the host's whole life, so only one runs at a time. |

A background host has no terminal, so it writes to
`$XDG_STATE_HOME/kage/host.log`, which moves aside to `host.log.1` past
1 MiB.

To run the host in the foreground instead, for example to watch its
output:

```sh
kage host run                  # exits after 10 idle minutes
kage host run --idle-secs 60
```

## a private engine

`kage rpc` keeps an engine of its own, as before the shared host, when:

- it gets `--standalone`, or `KAGE_STANDALONE` is set to anything but
  empty, `0`, `false`, `no` or `off`;
- it gets `-m`, `--system`, `--system-file` or `--yolo`, which change
  the engine for one client only;
- the running host is another kage version. It prints a notice and
  uses a private engine until the old host goes idle and exits, so the
  next client starts one of its own version;
- the host cannot start. The reason is in `host.log`.

## the session environment

A shared host was started from one client's shell, but each client's
tools must run in that client's own environment: its virtualenv, its
`direnv` variables and its `PATH`. So every `kage rpc` sends its
environment to the host, and the sessions it opens start these with
it:

- the `shell` tool and `!` commands,
- stdio MCP servers, including restarts,
- plugin `kage.exec` processes, and what `kage.env` reads,
- the `jj` and `git` commands of worktree agents,
- the agent behind an `acp:` model, which also starts in the session's
  directory.

Agents a session starts inherit its environment. The environment is
never saved: a session reopened after it closed uses the environment
of the client that reopens it. A client that opens a session another
client still has open joins it as it is, environment included. Variables that are not valid UTF-8 are left
out, and an environment over 1 MiB is not sent at all, so its sessions
use the host's.

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

A config or credential change made through a client, such as the
desktop's settings, reloads the providers for every client.

## memory

An idle host keeps about 2.5 MiB of private resident memory, and each
`kage rpc` relay under 2 MiB, measured on a Linux release build. CI
fails a release build whose idle host goes over 4 MiB or whose relay
goes over 2 MiB.
