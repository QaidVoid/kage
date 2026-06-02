//! Capability-gated function bindings.

use super::{Field, Func, GatedFunc};

pub(super) const GATED: &[GatedFunc] = &[
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Metadata for every entry in the current session, in",
                "order, each `{ id, kind, role?, ts }`. Use it to find",
                "a rewind point. Requires the `session_write`",
                "capability.",
            ],
            path: "kage.session.entries",
            params: &[],
            ret: Some("{ id: string, kind: string, role: string?, ts: string }[]"),
        },
    },
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Fork the current session at entry-id prefix `at` (or",
                "the latest entry when omitted) and reseat the live",
                "conversation onto the new fork between turns. This is",
                "the rewind move: base `fork` branches and stays;",
                "`fork_to` branches and goes there. Requires",
                "`session_write`.",
            ],
            path: "kage.session.fork_to",
            params: &[Field {
                name: "at?",
                ty: "string",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Reseat the live conversation onto an existing session",
                "(an id or path from `kage.session.list()`). The host",
                "validates and applies it between turns, consulting the",
                "`session_before_switch` veto. Requires `session_write`.",
            ],
            path: "kage.session.switch",
            params: &[Field {
                name: "target",
                ty: "string",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "exec",
        func: Func {
            doc: &[
                "Run a subprocess rooted at the workdir, no shell.",
                "Captures stdout/stderr and blocks until the process",
                "exits. `cwd` may not escape the workdir. Requires the",
                "`exec` capability.",
            ],
            path: "kage.exec",
            params: &[Field {
                name: "spec",
                ty: "kage.ExecSpec",
                doc: "",
            }],
            ret: Some("kage.ExecResult"),
        },
    },
    GatedFunc {
        cap: "env",
        func: Func {
            doc: &[
                "Read a process environment variable. Returns the value",
                "or `nil` when unset. Requires the `env` capability.",
            ],
            path: "kage.env",
            params: &[Field {
                name: "name",
                ty: "string",
                doc: "",
            }],
            ret: Some("string?"),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "HTTP GET. `opts` may carry headers and a body cap. Only",
                "SSRF filtering applies: the scheme must be http(s) and",
                "the host must resolve to a routable address; there is no",
                "host allow-list. Requires the `net` capability.",
            ],
            path: "kage.http.get",
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
            ],
            ret: Some(
                "{ status: integer, body: string, content_type: string, truncated: boolean }",
            ),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "HTTP POST. `opts` carries headers and either `body`",
                "(string) or `json` (table; auto-serialized with",
                "`Content-Type: application/json`). The two are mutually",
                "exclusive. Same SSRF rules as GET. Requires `net`.",
            ],
            path: "kage.http.post",
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
            ],
            ret: Some(
                "{ status: integer, body: string, content_type: string, truncated: boolean }",
            ),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "HTTP DELETE. `opts` carries headers (and optionally",
                "body, though most servers ignore it). Same SSRF rules as",
                "GET. Requires `net`.",
            ],
            path: "kage.http.delete",
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
            ],
            ret: Some(
                "{ status: integer, body: string, content_type: string, truncated: boolean }",
            ),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "Streaming HTTP POST. The response is read frame-by-frame",
                "as Server-Sent Events and `on_event({event, data})` is",
                "called once per blank-line-terminated frame. Multi-line",
                "`data:` lines join with `\\n`. Returns when the stream",
                "ends. Same SSRF rules as GET. Requires `net`.",
            ],
            path: "kage.http.post_stream",
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
                Field {
                    name: "on_event",
                    ty: "fun(ev: { event: string, data: string })",
                    doc: "",
                },
            ],
            ret: Some("{ status: integer, content_type: string }"),
        },
    },
];
