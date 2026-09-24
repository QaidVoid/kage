//! Base (always-present) function bindings.

use super::{Field, Func};

pub(super) const FUNCS: &[Func] = &[
    Func {
        doc: &["Wall-clock milliseconds since the Unix epoch."],
        path: "kage.now_ms",
        since: 1,
        params: &[],
        ret: Some("integer"),
    },
    Func {
        doc: &[
            "Integer generation of the `kage` plugin API surface. Bumped",
            "when a binding is added or removed; pair with",
            "`kage.requires` to guard against an incompatible host.",
        ],
        path: "kage.api_version",
        since: 1,
        params: &[],
        ret: Some("integer"),
    },
    Func {
        doc: &["Host crate version string (semver), e.g. \"0.1.0\"."],
        path: "kage.host_version",
        since: 1,
        params: &[],
        ret: Some("string"),
    },
    Func {
        doc: &[
            "Assert host compatibility at load time. `spec.api` is the",
            "minimum `kage.api_version` the plugin needs; an older host",
            "raises a clear error so a stale plugin fails loudly instead",
            "of part-way through a missing binding.",
        ],
        path: "kage.requires",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "{ api: integer? }",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Sleep the calling thread for `ms` milliseconds. Capped at",
            "500 ms per call so a host-side cancel never has to wait a",
            "multi-second sleep; loop the call to wait longer.",
        ],
        path: "kage.sleep_ms",
        since: 1,
        params: &[Field {
            name: "ms",
            ty: "integer",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Decode a JSON string into the equivalent Lua table or value."],
        path: "kage.json.decode",
        since: 1,
        params: &[Field {
            name: "raw",
            ty: "string",
            doc: "",
        }],
        ret: Some("any"),
    },
    Func {
        doc: &["Encode a Lua value as a JSON string."],
        path: "kage.json.encode",
        since: 1,
        params: &[Field {
            name: "value",
            ty: "any",
            doc: "",
        }],
        ret: Some("string"),
    },
    Func {
        doc: &["Record a structured log line at `level`."],
        path: "kage.log",
        since: 1,
        params: &[
            Field {
                name: "level",
                ty: "kage.LogLevel",
                doc: "",
            },
            Field {
                name: "message",
                ty: "string",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "A copy of the host-supplied configuration table. Mutating",
            "the returned table does not propagate back to the host or to",
            "disk; use `kage.store` for state that must persist.",
        ],
        path: "kage.config",
        since: 1,
        params: &[],
        ret: Some("table"),
    },
    Func {
        doc: &[
            "This plugin's own settings from `[plugins.config.<stem>]` in",
            "config.toml, as a table (empty when none are set). A plugin",
            "sees only its own slice, never another plugin's. Mutating",
            "the returned table does not propagate back to the host or to",
            "disk; use `kage.store` to persist state.",
        ],
        path: "kage.plugin_config",
        since: 1,
        params: &[],
        ret: Some("table"),
    },
    Func {
        doc: &[
            "Read a value previously saved with `kage.store.set`, or",
            "`nil` when the key is unset. State is private to this plugin",
            "and persists across reloads and restarts.",
        ],
        path: "kage.store.get",
        since: 1,
        params: &[Field {
            name: "key",
            ty: "string",
            doc: "",
        }],
        ret: Some("any"),
    },
    Func {
        doc: &["Persist `value` (any JSON-serializable value) under `key`."],
        path: "kage.store.set",
        since: 1,
        params: &[
            Field {
                name: "key",
                ty: "string",
                doc: "",
            },
            Field {
                name: "value",
                ty: "any",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &["Remove `key` from this plugin's store. A no-op when unset."],
        path: "kage.store.delete",
        since: 1,
        params: &[Field {
            name: "key",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["List the keys currently held in this plugin's store."],
        path: "kage.store.keys",
        since: 1,
        params: &[],
        ret: Some("string[]"),
    },
    Func {
        doc: &[
            "Request elevated capabilities for this plugin. Returns a",
            "`{ name = granted }` table: a capability is true only if",
            "the user granted it to this plugin in",
            "`[plugins.capabilities]`. Granted APIs are attached to",
            "this plugin alone. Call once at load and degrade when a",
            "capability is missing. Unknown names raise an error.",
        ],
        path: "kage.request_capabilities",
        since: 1,
        params: &[Field {
            name: "caps",
            ty: "kage.Capability[]",
            doc: "",
        }],
        ret: Some("table<string, boolean>"),
    },
    Func {
        doc: &["Back-compat alias for `kage.ui.notify`."],
        path: "kage.notify",
        since: 1,
        params: &[
            Field {
                name: "message",
                ty: "string",
                doc: "",
            },
            Field {
                name: "level?",
                ty: "kage.NotifyLevel",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "Show a transient toast (stderr in print mode). Non-info",
            "levels are also logged. Unknown level raises an error.",
        ],
        path: "kage.ui.notify",
        since: 1,
        params: &[
            Field {
                name: "message",
                ty: "string",
                doc: "",
            },
            Field {
                name: "level?",
                ty: "kage.NotifyLevel",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "Open a fuzzy picker. Each item is a string or",
            "`{ label, value?, detail? }`. Suspends the calling",
            "coroutine; returns the chosen value, or nil if cancelled.",
        ],
        path: "kage.ui.select",
        since: 1,
        params: &[
            Field {
                name: "title",
                ty: "string",
                doc: "",
            },
            Field {
                name: "items",
                ty: "(string|{ label: string, value?: string, detail?: string })[]",
                doc: "",
            },
        ],
        ret: Some("string|nil"),
    },
    Func {
        doc: &["Open a yes/no overlay. Cancelling counts as false."],
        path: "kage.ui.confirm",
        since: 1,
        params: &[
            Field {
                name: "title",
                ty: "string",
                doc: "",
            },
            Field {
                name: "message",
                ty: "string",
                doc: "",
            },
        ],
        ret: Some("boolean"),
    },
    Func {
        doc: &[
            "Open a single-line input. Returns the string, or nil if",
            "cancelled. `placeholder` is dim help, not part of result.",
        ],
        path: "kage.ui.input",
        since: 1,
        params: &[
            Field {
                name: "title",
                ty: "string",
                doc: "",
            },
            Field {
                name: "placeholder?",
                ty: "string",
                doc: "",
            },
        ],
        ret: Some("string|nil"),
    },
    Func {
        doc: &[
            "Open a multi-line editor seeded with `prefill`. Ctrl+S",
            "submits, Esc cancels. Returns the buffer, or nil.",
        ],
        path: "kage.ui.editor",
        since: 1,
        params: &[
            Field {
                name: "title",
                ty: "string",
                doc: "",
            },
            Field {
                name: "prefill?",
                ty: "string",
                doc: "",
            },
        ],
        ret: Some("string|nil"),
    },
    Func {
        doc: &[
            "Take over the top status row. `fn(width)` runs each redraw",
            "and returns a string, a span table, or an array of those.",
            "Pass nil to restore the built-in status bar.",
        ],
        path: "kage.ui.set_header",
        since: 1,
        params: &[Field {
            name: "fn",
            ty: "fun(width: integer): any|nil",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Take over the bottom modeline row. Same shape as set_header."],
        path: "kage.ui.set_footer",
        since: 1,
        params: &[Field {
            name: "fn",
            ty: "fun(width: integer): any|nil",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Register a tool the agent can call like a built-in."],
        path: "kage.register_tool",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.ToolSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Like `register_tool` but replaces the existing tool by",
            "name. The host logs a warning if no such tool existed.",
        ],
        path: "kage.override_tool",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.ToolSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Register a slash / colon command."],
        path: "kage.register_command",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.CommandSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Like `register_command`, but allowed to shadow a built-in",
            "command of the same name and dispatched ahead of it.",
        ],
        path: "kage.override_command",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.CommandSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Own how a block of `kind` is drawn. `render` gets a block",
            "table (`{ kind, width, ... }`) and returns the same shape",
            "as `kage.ui.set_header`. A custom kind re-skins that block",
            "type; the reserved names `user`/`assistant`/`thinking`/",
            "`tool_call`/`tool_result`/`custom` override a built-in.",
            "Pass `nil` to remove a renderer.",
        ],
        path: "kage.register_block_renderer",
        since: 1,
        params: &[
            Field {
                name: "kind",
                ty: "string",
                doc: "Custom block kind to take over.",
            },
            Field {
                name: "render",
                ty: "fun(block: table): any|nil",
                doc: "Gets { kind, text, width }; nil unregisters.",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "Bind a chord to a handler. `spec` is a chord string or",
            "`{ key, description? }`. The handler runs through the",
            "coroutine bridge, so it may open `kage.ui.*` dialogs.",
        ],
        path: "kage.register_keybinding",
        since: 1,
        params: &[
            Field {
                name: "spec",
                ty: "string|{ key: string, description?: string }",
                doc: "",
            },
            Field {
                name: "handler",
                ty: "fun(): string?",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "Add a prompt-input autocomplete provider. Providers form a",
            "stack; the most recently added wins. Runs synchronously on",
            "the Lua thread, so keep it cheap.",
        ],
        path: "kage.add_autocomplete_provider",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.AutocompleteSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Observe every key before any modal layer. A truthy return",
            "consumes the event. Returns an `off` function that",
            "unregisters this handler (idempotent). Prefer",
            "register_keybinding unless you must swallow arbitrary keys.",
        ],
        path: "kage.on_terminal_input",
        since: 1,
        params: &[Field {
            name: "handler",
            ty: "fun(ev: kage.KeyEvent): boolean",
            doc: "",
        }],
        ret: Some("fun()"),
    },
    Func {
        doc: &["Register a status-bar widget."],
        path: "kage.register_widget",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.WidgetSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Push or clear a transient status entry. Nil/empty clears."],
        path: "kage.set_status",
        since: 1,
        params: &[
            Field {
                name: "key",
                ty: "string",
                doc: "",
            },
            Field {
                name: "text",
                ty: "string|nil",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &["Clear a transient status entry."],
        path: "kage.clear_status",
        since: 1,
        params: &[Field {
            name: "key",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Subscribe to an event. Handlers fire in registration",
            "order; a raising handler is logged and skipped.",
            "Notification events ignore the return; transform events",
            "chain it; predicate / session-op events interpret it.",
            "Returns `off`, which removes this subscription. Calling",
            "it again, or from inside a handler, is safe. An unknown",
            "event name logs one warning and subscribes to nothing.",
            "An alias over `kage.api.autocmd_create`.",
        ],
        path: "kage.on",
        since: 1,
        params: &[
            Field {
                name: "event",
                ty: "kage.Event",
                doc: "",
            },
            Field {
                name: "handler",
                ty: "fun(payload: any): any",
                doc: "",
            },
        ],
        ret: Some("fun()"),
    },
    Func {
        doc: &[
            "Register a new LLM provider implementation. Advanced; see",
            "the example plugins for a realistic shape.",
        ],
        path: "kage.register_provider",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.ProviderSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Sessions the host knows about: `{ id, value }` each."],
        path: "kage.session.list",
        since: 1,
        params: &[],
        ret: Some("{ id: string, value: string }[]"),
    },
    Func {
        doc: &[
            "Fork the current session at entry-id prefix `at` (or the",
            "latest entry when omitted). Performed between turns.",
        ],
        path: "kage.session.fork",
        since: 1,
        params: &[Field {
            name: "at?",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Append a custom entry to the session JSONL. `kind` is a",
            "namespaced string; `data` is any table (defaults to {}).",
        ],
        path: "kage.session.append_entry",
        since: 1,
        params: &[
            Field {
                name: "kind",
                ty: "string",
                doc: "",
            },
            Field {
                name: "data?",
                ty: "table",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &["Write a label pointing at entry id `anchor`. Nil clears."],
        path: "kage.session.set_label",
        since: 1,
        params: &[
            Field {
                name: "anchor",
                ty: "string",
                doc: "",
            },
            Field {
                name: "label?",
                ty: "string",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &["Queue a synthetic message delivered between turns."],
        path: "kage.send_message",
        since: 1,
        params: &[
            Field {
                name: "text",
                ty: "string",
                doc: "",
            },
            Field {
                name: "opts?",
                ty: "kage.SendOpts",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "Snapshot per-turn token usage. Nil until the host has run",
            "at least one turn.",
        ],
        path: "kage.context_usage",
        since: 1,
        params: &[],
        ret: Some("kage.Usage|nil"),
    },
    Func {
        doc: &[
            "Ask the host to run a compaction pass. `prompt` is",
            "advisory; the compact_prepare event is the precise hook.",
        ],
        path: "kage.compact",
        since: 1,
        params: &[Field {
            name: "prompt?",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Read a file relative to the session workdir. Paths outside",
            "the workdir tree raise an error.",
        ],
        path: "kage.fs.read",
        since: 1,
        params: &[Field {
            name: "path",
            ty: "string",
            doc: "",
        }],
        ret: Some("string"),
    },
    Func {
        doc: &["Write a file under the workdir. Same restriction as read."],
        path: "kage.fs.write",
        since: 1,
        params: &[
            Field {
                name: "path",
                ty: "string",
                doc: "",
            },
            Field {
                name: "content",
                ty: "string",
                doc: "",
            },
        ],
        ret: None,
    },
    Func {
        doc: &[
            "Declare an upstream ACP agent at runtime, mirroring",
            "`[acp.agents.<name>]` in config.toml. Core spawns it.",
        ],
        path: "kage.acp.add_agent",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.AcpAgentSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Register the single policy callback consulted when an",
            "upstream ACP agent asks to run a tool. It must return a",
            "boolean and must not open a dialog (no coroutine suspend):",
            "it is policy, not UI. No handler, or a non-boolean or",
            "erroring handler, denies.",
        ],
        path: "kage.on_acp_permission",
        since: 1,
        params: &[Field {
            name: "handler",
            ty: "fun(req: table): boolean",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Declare an MCP server at runtime, mirroring",
            "`[mcp.servers.<name>]` in config.toml. Core spawns it.",
        ],
        path: "kage.mcp.add_server",
        since: 1,
        params: &[Field {
            name: "spec",
            ty: "kage.McpServerSpec",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["Names of the plugin-declared MCP servers, sorted."],
        path: "kage.mcp.list_servers",
        since: 1,
        params: &[],
        ret: Some("string[]"),
    },
    Func {
        doc: &[
            "Ask the host to restart a declared MCP server. Applied",
            "between turns against the live manager.",
        ],
        path: "kage.mcp.restart",
        since: 1,
        params: &[Field {
            name: "name",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &["The active theme name, or \"\" if none is set yet."],
        path: "kage.theme.current",
        since: 1,
        params: &[],
        ret: Some("string"),
    },
    Func {
        doc: &["Theme names that may be passed to `kage.theme.set`."],
        path: "kage.theme.list",
        since: 1,
        params: &[],
        ret: Some("string[]"),
    },
    Func {
        doc: &[
            "Request a theme switch. The host validates and applies it",
            "between turns. Errors on a non-string or empty name.",
        ],
        path: "kage.theme.set",
        since: 1,
        params: &[Field {
            name: "name",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Run `fn` on the Lua thread once, right after the current",
            "host call and before the next queued one. A raising `fn` is",
            "logged. Reload drops it if it has not run yet.",
        ],
        path: "kage.schedule",
        since: 2,
        params: &[Field {
            name: "fn",
            ty: "fun()",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Run `fn` on the Lua thread once, `ms` milliseconds from now.",
            "Returns `stop`, which cancels it if it has not run yet. A",
            "raising `fn` is logged. Reload cancels it.",
        ],
        path: "kage.defer",
        since: 2,
        params: &[
            Field {
                name: "fn",
                ty: "fun()",
                doc: "",
            },
            Field {
                name: "ms",
                ty: "integer",
                doc: "",
            },
        ],
        ret: Some("fun()"),
    },
    Func {
        doc: &[
            "Run `fn` on the Lua thread every `ms` milliseconds (at least",
            "50) until the returned `stop` is called. A raising `fn` is",
            "logged once and the timer stops. Reload cancels it.",
        ],
        path: "kage.timer",
        since: 2,
        params: &[
            Field {
                name: "fn",
                ty: "fun()",
                doc: "",
            },
            Field {
                name: "ms",
                ty: "integer",
                doc: "",
            },
        ],
        ret: Some("fun()"),
    },
    Func {
        doc: &[
            "Create an autocmd for `event` and return its id. Raises on",
            "an unknown event, an unknown group, or a pattern other than",
            "`*` for an event without a match key.",
        ],
        path: "kage.api.autocmd_create",
        since: 2,
        params: &[
            Field {
                name: "event",
                ty: "kage.Event",
                doc: "",
            },
            Field {
                name: "opts",
                ty: "kage.AutocmdOpts",
                doc: "",
            },
        ],
        ret: Some("integer"),
    },
    Func {
        doc: &["Delete the autocmd with `id`. A missing id is ignored."],
        path: "kage.api.autocmd_del",
        since: 2,
        params: &[Field {
            name: "id",
            ty: "integer",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Create the group `name`, or return the existing one, and",
            "return its id. With `clear = true` (the default) an",
            "existing group loses its autocmds, so re-running the same",
            "setup does not register twice.",
        ],
        path: "kage.api.augroup_create",
        since: 2,
        params: &[
            Field {
                name: "name",
                ty: "string",
                doc: "",
            },
            Field {
                name: "opts?",
                ty: "{ clear: boolean? }",
                doc: "",
            },
        ],
        ret: Some("integer"),
    },
    Func {
        doc: &[
            "Delete a group, by id or name, with all its autocmds. An",
            "unknown group is ignored.",
        ],
        path: "kage.api.augroup_del",
        since: 2,
        params: &[Field {
            name: "group",
            ty: "integer|string",
            doc: "",
        }],
        ret: None,
    },
    Func {
        doc: &[
            "Fire `event` now. `pattern` is the match autocmd patterns",
            "compare against, and `data` arrives as `ev.data`. Raising",
            "callbacks are logged and skipped. Nesting deeper than 16",
            "raises.",
        ],
        path: "kage.api.autocmd_exec",
        since: 2,
        params: &[
            Field {
                name: "event",
                ty: "kage.Event",
                doc: "",
            },
            Field {
                name: "opts?",
                ty: "{ pattern: string?, data: any }",
                doc: "",
            },
        ],
        ret: None,
    },
];
