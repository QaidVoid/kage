//! Generated-stub sub-table declarations.

use super::Table;

pub(super) const TABLES: &[Table] = &[
    Table {
        path: "kage.json",
        class_doc: "JSON encode and decode helpers.",
    },
    Table {
        path: "kage.ui",
        class_doc: "Interactive UI surface.",
    },
    Table {
        path: "kage.session",
        class_doc: "Session inspection and control.",
    },
    Table {
        path: "kage.fs",
        class_doc: "Workdir-scoped filesystem access.",
    },
    Table {
        path: "kage.http",
        class_doc: "Outbound HTTP, gated behind the `net` capability.",
    },
    Table {
        path: "kage.store",
        class_doc: "Per-plugin persistent key-value state.",
    },
    Table {
        path: "kage.acp",
        class_doc: "Declarative ACP agent config.",
    },
    Table {
        path: "kage.mcp",
        class_doc: "Declarative MCP server config.",
    },
    Table {
        path: "kage.theme",
        class_doc: "Theme inspection and switching.",
    },
];
