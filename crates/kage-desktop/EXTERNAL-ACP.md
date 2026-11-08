# External ACP session selection

Picking `acp:<name>` on the new session screen opens a persistent session
on the configured external agent when the host advertises
`_meta.kage.externalSessions`. The desktop sends the selection only then.
After that the session takes prompts, cancellation and permission answers.
Its agent owns models, modes, history and files, so the desktop refuses
the other session actions with a notice.

The `acp:<name>` provider used from the CLI and the standalone TUI starts
a fresh upstream process and session on each turn. It does not preserve
upstream conversation context.

An external process can run its own tools with its own privileges, so this
is not a sandbox.
