# External ACP session selection

Persistent external ACP sessions are under development. The host does not
currently advertise or accept external session creation because its
cancellation and shutdown paths need further work. The desktop will not
send an external selection unless the host advertises support.

The existing `acp:<name>` provider starts a fresh upstream process and
session on each turn. It does not preserve upstream conversation context.
An external process can run its own tools with its own privileges, so this
is not a sandbox.
