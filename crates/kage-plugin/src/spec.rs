//! Single source of truth for the `kage` Lua plugin surface.
//!
//! Every alias, record class, sub-table, and function a plugin can see
//! is described here, in Rust, next to the crate that implements it.
//! `cargo xtask gen-lua-types` renders [`surface`] into the
//! `lua-language-server` stub `plugins/types/kage.lua`; the CI drift
//! gate re-renders and diffs, so the shipped stub cannot drift from
//! this description, and this description cannot drift from the crate
//! that owns it (the [`tests`] module asserts every declared function
//! path resolves in a freshly built [`crate::PluginRuntime`]).
//!
//! This replaces the previously hand-maintained spec that lived in
//! `xtask/src/luatypes.rs`: that copy could (and did) fall out of step
//! with the Rust bindings. Keeping the description in this crate makes
//! adding a binding a single edit instead of two.

/// One `---@param` (on a function) or `---@field` (on a class).
#[derive(Clone, Copy, Debug)]
pub struct Field {
    /// Field name. A trailing `?` marks it optional; the renderer
    /// strips the `?` from the displayed name and emits the emmylua
    /// optional marker instead.
    pub name: &'static str,
    /// emmylua type expression (e.g. `string`, `kage.ToolSpec`,
    /// `fun(x: integer): string`).
    pub ty: &'static str,
    /// One-line trailing doc. Empty renders no trailing text.
    pub doc: &'static str,
}

/// A `---@class` record type.
#[derive(Clone, Copy, Debug)]
pub struct Class {
    /// Fully qualified class name (e.g. `kage.ToolSpec`).
    pub name: &'static str,
    /// Doc lines emitted above the class. An empty entry renders a
    /// bare `---` separator line.
    pub doc: &'static [&'static str],
    /// Record fields, in declaration order.
    pub fields: &'static [Field],
}

/// A `---@alias` sum type. Variants render inline when there are five
/// or fewer, otherwise as the multi-line `---| "x"` form.
#[derive(Clone, Copy, Debug)]
pub struct Alias {
    /// Fully qualified alias name (e.g. `kage.Event`).
    pub name: &'static str,
    /// Doc lines emitted above the alias.
    pub doc: &'static [&'static str],
    /// String-literal variants, in order.
    pub variants: &'static [&'static str],
}

/// A single function binding.
#[derive(Clone, Copy, Debug)]
pub struct Func {
    /// Doc lines emitted above the function.
    pub doc: &'static [&'static str],
    /// Dotted path, e.g. `kage.ui.select`.
    pub path: &'static str,
    /// Parameters, in order.
    pub params: &'static [Field],
    /// emmylua return type, or `None` for a function that returns
    /// nothing.
    pub ret: Option<&'static str>,
}

/// A sub-table to declare (`kage.ui = {}`) before its first function.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    /// Dotted path, e.g. `kage.ui`.
    pub path: &'static str,
    /// One-line doc emitted above the table declaration.
    pub class_doc: &'static str,
}

/// A function that is only present when the plugin has been granted
/// `cap` (see [`crate::capabilities`]). It is rendered into the stub
/// like any function but is not on the base surface, so it resolves
/// only on a granted plugin's `kage` proxy, never the default one.
#[derive(Clone, Copy, Debug)]
pub struct GatedFunc {
    /// Wire name of the capability that unlocks this function.
    pub cap: &'static str,
    /// The function binding itself.
    pub func: Func,
}

/// The complete declarative description of the `kage` Lua surface.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// `---@alias` sum types.
    pub aliases: &'static [Alias],
    /// `---@class` record types.
    pub classes: &'static [Class],
    /// Sub-tables declared before their first function.
    pub tables: &'static [Table],
    /// Base function bindings, present for every plugin.
    pub funcs: &'static [Func],
    /// Capability-gated functions, present only on a plugin granted
    /// the named capability.
    pub gated: &'static [GatedFunc],
}

/// The single source of truth: the full `kage` plugin surface.
#[must_use]
pub fn surface() -> Surface {
    Surface {
        aliases: ALIASES,
        classes: CLASSES,
        tables: TABLES,
        funcs: FUNCS,
        gated: GATED,
    }
}

const ALIASES: &[Alias] = &[
    Alias {
        name: "kage.LogLevel",
        doc: &["Levels accepted by `kage.log`."],
        variants: &["trace", "debug", "info", "warn", "error"],
    },
    Alias {
        name: "kage.NotifyLevel",
        doc: &["Levels accepted by `kage.ui.notify` / `kage.notify`."],
        variants: &["info", "warning", "error"],
    },
    Alias {
        name: "kage.ToolRisk",
        doc: &["Permission tier for a registered tool."],
        variants: &["read", "write", "network"],
    },
    Alias {
        name: "kage.ArgKind",
        doc: &["A declared command argument kind."],
        variants: &["text", "choice", "path", "session", "flag"],
    },
    Alias {
        name: "kage.Capability",
        doc: &[
            "An elevated capability a plugin may request. Granted",
            "per-plugin in `[plugins.capabilities]`.",
        ],
        variants: &["session_write", "exec", "env", "net"],
    },
    Alias {
        name: "kage.Event",
        doc: &[
            "Every event name `kage.on` accepts. Notification events",
            "ignore the handler return; transform events chain it;",
            "predicate and session-op events interpret it.",
        ],
        variants: &[
            "before_agent_start",
            "agent_start",
            "agent_end",
            "turn_start",
            "turn_end",
            "message_start",
            "message_update",
            "message_end",
            "after_provider_response",
            "tool_call",
            "tool_update",
            "tool_result",
            "session_open",
            "session_close",
            "model_select",
            "thinking_level_select",
            "user_bash",
            "transform_context",
            "before_provider_request",
            "compact_prepare",
            "should_stop_after_turn",
            "session_before_switch",
            "session_before_fork",
            "session_before_tree",
            "resources_discover",
        ],
    },
];

const CLASSES: &[Class] = &[
    Class {
        name: "kage.ToolResult",
        doc: &["One result a tool may return instead of a bare string."],
        fields: &[
            Field {
                name: "text",
                ty: "string",
                doc: "Output text shown to the agent.",
            },
            Field {
                name: "is_error?",
                ty: "boolean",
                doc: "Mark the call as failed.",
            },
            Field {
                name: "structured?",
                ty: "table",
                doc: "Machine-readable detail.",
            },
        ],
    },
    Class {
        name: "kage.ToolSpec",
        doc: &["Spec passed to `kage.register_tool` / `kage.override_tool`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Tool name the agent calls.",
            },
            Field {
                name: "description",
                ty: "string",
                doc: "One-line description for the model.",
            },
            Field {
                name: "schema",
                ty: "table",
                doc: "JSON schema for the input object.",
            },
            Field {
                name: "risk?",
                ty: "kage.ToolRisk",
                doc: "Permission tier; defaults to read.",
            },
            Field {
                name: "execute",
                ty: "fun(input: table): string|kage.ToolResult|boolean|number|nil",
                doc: "Tool body.",
            },
        ],
    },
    Class {
        name: "kage.CommandArg",
        doc: &["One declared command argument."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Becomes `args.<name>`.",
            },
            Field {
                name: "kind",
                ty: "kage.ArgKind",
                doc: "Required.",
            },
            Field {
                name: "optional?",
                ty: "boolean",
                doc: "Defaults to false.",
            },
            Field {
                name: "choices?",
                ty: "string[]",
                doc: "Required when kind == choice.",
            },
            Field {
                name: "hint?",
                ty: "string",
                doc: "Placeholder for kind == text; defaults to value.",
            },
        ],
    },
    Class {
        name: "kage.CommandSpec",
        doc: &[
            "Spec passed to `kage.register_command`. The handler runs",
            "through the coroutine bridge, so it may call the blocking",
            "`kage.ui.*` dialogs directly.",
        ],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "No leading / or :.",
            },
            Field {
                name: "aliases?",
                ty: "string[]",
                doc: "Alternate names that resolve to this command.",
            },
            Field {
                name: "description",
                ty: "string",
                doc: "Shown in the palette and :help.",
            },
            Field {
                name: "args?",
                ty: "kage.CommandArg[]",
                doc: "",
            },
            Field {
                name: "handler",
                ty: "fun(raw: string, ctx: table, args: table): string|kage.CommandResult|nil",
                doc: "raw text, host ctx, parsed args by name.",
            },
        ],
    },
    Class {
        name: "kage.WidgetSpec",
        doc: &["Spec passed to `kage.register_widget`."],
        fields: &[
            Field {
                name: "key",
                ty: "string",
                doc: "Re-registering replaces in place.",
            },
            Field {
                name: "render",
                ty: "fun(width: integer): string?",
                doc: "Runs once per redraw.",
            },
        ],
    },
    Class {
        name: "kage.CompletionItem",
        doc: &["One autocomplete candidate."],
        fields: &[
            Field {
                name: "value",
                ty: "string",
                doc: "Replacement text (required).",
            },
            Field {
                name: "label?",
                ty: "string",
                doc: "Row label; defaults to value.",
            },
            Field {
                name: "detail?",
                ty: "string",
                doc: "Dim annotation.",
            },
            Field {
                name: "range?",
                ty: "integer[]",
                doc: "{ from, to } 0-based byte span to replace.",
            },
        ],
    },
    Class {
        name: "kage.AutocompleteSpec",
        doc: &["Spec passed to `kage.add_autocomplete_provider`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Re-adding replaces in place.",
            },
            Field {
                name: "complete",
                ty: "fun(prefix: string, ctx: { text: string, cursor: integer }): kage.CompletionItem[]",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.KeyEvent",
        doc: &["Key descriptor handed to a `kage.on_terminal_input` handler."],
        fields: &[
            Field {
                name: "code",
                ty: "string",
                doc: "char|enter|esc|tab|backspace|up|down|left|right|home|end|pageup|pagedown|delete|insert|f1..f12|other.",
            },
            Field {
                name: "char?",
                ty: "string",
                doc: "Present only when code == char.",
            },
            Field {
                name: "ctrl",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "alt",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "shift",
                ty: "boolean",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.SendOpts",
        doc: &["Options for `kage.send_message`."],
        fields: &[
            Field {
                name: "trigger_turn?",
                ty: "boolean",
                doc: "Default true.",
            },
            Field {
                name: "deliver_as?",
                ty: "\"user\"",
                doc: "In v0.1 only user is wired.",
            },
        ],
    },
    Class {
        name: "kage.Usage",
        doc: &[
            "Snapshot returned by `kage.context_usage`. The host fills",
            "this in; the fields below are the conventional keys and",
            "may vary by host version.",
        ],
        fields: &[
            Field {
                name: "model",
                ty: "string",
                doc: "",
            },
            Field {
                name: "input_tokens",
                ty: "integer",
                doc: "",
            },
            Field {
                name: "output_tokens",
                ty: "integer",
                doc: "",
            },
            Field {
                name: "context_window",
                ty: "integer",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.CommandResult",
        doc: &["Rich result a command handler may return instead of a string."],
        fields: &[
            Field {
                name: "text?",
                ty: "string",
                doc: "Output text shown to the user.",
            },
            Field {
                name: "is_error?",
                ty: "boolean",
                doc: "Mark the invocation as failed.",
            },
            Field {
                name: "structured?",
                ty: "table",
                doc: "Machine-readable detail.",
            },
        ],
    },
    Class {
        name: "kage.AcpAgentSpec",
        doc: &["Spec passed to `kage.acp.add_agent`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Agent id; required.",
            },
            Field {
                name: "command",
                ty: "string",
                doc: "Executable to spawn; required.",
            },
            Field {
                name: "args?",
                ty: "string[]",
                doc: "Command arguments.",
            },
            Field {
                name: "env?",
                ty: "table",
                doc: "String-to-string environment overrides.",
            },
        ],
    },
    Class {
        name: "kage.McpServerSpec",
        doc: &["Spec passed to `kage.mcp.add_server`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Server id; required.",
            },
            Field {
                name: "command",
                ty: "string",
                doc: "Executable to spawn; required.",
            },
            Field {
                name: "args?",
                ty: "string[]",
                doc: "Command arguments.",
            },
            Field {
                name: "env?",
                ty: "table",
                doc: "String-to-string environment overrides.",
            },
            Field {
                name: "disabled?",
                ty: "boolean",
                doc: "Declare but do not spawn; defaults to false.",
            },
        ],
    },
    Class {
        name: "kage.ProviderSpec",
        doc: &["Spec passed to `kage.register_provider`."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Provider id; required.",
            },
            Field {
                name: "display_name?",
                ty: "string",
                doc: "Defaults to id.",
            },
            Field {
                name: "supports_caching?",
                ty: "boolean",
                doc: "Defaults to false.",
            },
            Field {
                name: "supports_thinking?",
                ty: "boolean",
                doc: "Defaults to false.",
            },
            Field {
                name: "supports_tool_use?",
                ty: "boolean",
                doc: "Defaults to true.",
            },
            Field {
                name: "preserves_thinking?",
                ty: "boolean",
                doc: "Skip flatten-thinking when replaying history. Defaults to false.",
            },
            Field {
                name: "models?",
                ty: "kage.ProviderModel[]",
                doc: "Models the provider advertises in the picker.",
            },
            Field {
                name: "stream",
                ty: "fun(req: table): table[]|fun(): table?",
                doc: "Yields provider event tables; required.",
            },
        ],
    },
    Class {
        name: "kage.ProviderModel",
        doc: &["One model entry surfaced in the picker."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Model id, used as the part after `provider:`.",
            },
            Field {
                name: "name?",
                ty: "string",
                doc: "Display name; defaults to id.",
            },
            Field {
                name: "context?",
                ty: "integer",
                doc: "Context window in tokens, for the modeline percent.",
            },
            Field {
                name: "max_output?",
                ty: "integer",
                doc: "Per-turn max output tokens forwarded to the provider.",
            },
        ],
    },
    Class {
        name: "kage.ExecSpec",
        doc: &["Spec passed to `kage.exec`."],
        fields: &[
            Field {
                name: "cmd",
                ty: "string",
                doc: "Executable; resolved via PATH. No shell.",
            },
            Field {
                name: "args?",
                ty: "string[]",
                doc: "Arguments, passed verbatim.",
            },
            Field {
                name: "cwd?",
                ty: "string",
                doc: "Workdir-relative dir; defaults to the workdir.",
            },
        ],
    },
    Class {
        name: "kage.ExecResult",
        doc: &["Result returned by `kage.exec`."],
        fields: &[
            Field {
                name: "code",
                ty: "integer",
                doc: "Exit code; -1 if killed by a signal.",
            },
            Field {
                name: "stdout",
                ty: "string",
                doc: "Captured standard output.",
            },
            Field {
                name: "stderr",
                ty: "string",
                doc: "Captured standard error.",
            },
        ],
    },
    Class {
        name: "kage.HttpRequestOpts",
        doc: &[
            "Options accepted by `kage.http.post`, `kage.http.delete`,",
            "and `kage.http.post_stream`.",
        ],
        fields: &[
            Field {
                name: "headers?",
                ty: "table<string, string>",
                doc: "Request headers.",
            },
            Field {
                name: "body?",
                ty: "string",
                doc: "Raw request body. Mutually exclusive with `json`.",
            },
            Field {
                name: "json?",
                ty: "table",
                doc: "Body encoded as JSON; sets Content-Type to application/json.",
            },
            Field {
                name: "max_bytes?",
                ty: "integer",
                doc: "Response body cap. Defaults: 2 MB simple, 32 MB streamed.",
            },
        ],
    },
];

const TABLES: &[Table] = &[
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

const FUNCS: &[Func] = &[
    Func {
        doc: &["Wall-clock milliseconds since the Unix epoch."],
        path: "kage.now_ms",
        params: &[],
        ret: Some("integer"),
    },
    Func {
        doc: &[
            "Sleep the calling thread for `ms` milliseconds. Capped at",
            "500 ms per call so a host-side cancel never has to wait a",
            "multi-second sleep; loop the call to wait longer.",
        ],
        path: "kage.sleep_ms",
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
            "the returned table does not propagate back to the host.",
        ],
        path: "kage.config",
        params: &[],
        ret: Some("table"),
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
            "stack; the most recently added wins. Runs synchronously in",
            "the shared Lua mutex, so keep it cheap.",
        ],
        path: "kage.add_autocomplete_provider",
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
        ],
        path: "kage.on",
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
        ret: None,
    },
    Func {
        doc: &[
            "Register a new LLM provider implementation. Advanced; see",
            "the example plugins for a realistic shape.",
        ],
        path: "kage.register_provider",
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
        params: &[],
        ret: Some("{ id: string, value: string }[]"),
    },
    Func {
        doc: &[
            "Fork the current session at entry-id prefix `at` (or the",
            "latest entry when omitted). Performed between turns.",
        ],
        path: "kage.session.fork",
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
        params: &[],
        ret: Some("kage.Usage|nil"),
    },
    Func {
        doc: &[
            "Ask the host to run a compaction pass. `prompt` is",
            "advisory; the compact_prepare event is the precise hook.",
        ],
        path: "kage.compact",
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
        params: &[],
        ret: Some("string[]"),
    },
    Func {
        doc: &[
            "Ask the host to restart a declared MCP server. Applied",
            "between turns against the live manager.",
        ],
        path: "kage.mcp.restart",
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
        params: &[],
        ret: Some("string"),
    },
    Func {
        doc: &["Theme names that may be passed to `kage.theme.set`."],
        path: "kage.theme.list",
        params: &[],
        ret: Some("string[]"),
    },
    Func {
        doc: &[
            "Request a theme switch. The host validates and applies it",
            "between turns. Errors on a non-string or empty name.",
        ],
        path: "kage.theme.set",
        params: &[Field {
            name: "name",
            ty: "string",
            doc: "",
        }],
        ret: None,
    },
];

const GATED: &[GatedFunc] = &[
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginRuntime;

    /// Walk a dotted path (`kage.ui.select`) through the built runtime
    /// and confirm it resolves to a Lua function. This is the
    /// anti-drift guarantee the old hand-maintained spec lacked: a
    /// declared binding that is not actually installed fails CI here.
    #[test]
    fn every_declared_func_resolves_in_a_built_runtime() {
        let rt = PluginRuntime::new().expect("runtime builds");
        let lua = rt.lock_lua();
        for f in surface().funcs {
            let mut segments = f.path.split('.');
            let root = segments.next().expect("path has a root");
            let mut value: mlua::Value = lua
                .globals()
                .get(root)
                .unwrap_or_else(|e| panic!("global `{root}` missing: {e}"));
            for seg in segments {
                let table = match value {
                    mlua::Value::Table(t) => t,
                    other => panic!("{}: `{seg}` parent is {other:?}, not a table", f.path),
                };
                value = table
                    .get(seg)
                    .unwrap_or_else(|e| panic!("{}: segment `{seg}` missing: {e}", f.path));
            }
            assert!(
                matches!(value, mlua::Value::Function(_)),
                "{} resolved to {value:?}, expected a function",
                f.path
            );
        }
    }

    #[test]
    fn surface_has_no_duplicate_func_paths() {
        let s = surface();
        let mut seen = std::collections::BTreeSet::new();
        for path in s
            .funcs
            .iter()
            .map(|f| f.path)
            .chain(s.gated.iter().map(|g| g.func.path))
        {
            assert!(seen.insert(path), "duplicate function path {path}");
        }
    }

    /// The anti-drift guarantee extended to capability-gated funcs:
    /// granted, they resolve on that plugin's proxy; ungranted, they
    /// are absent (per-plugin isolation, not a runtime error).
    #[test]
    fn gated_funcs_resolve_only_when_capability_granted() {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert(
            "trusted".to_owned(),
            vec![
                "session_write".to_owned(),
                "exec".to_owned(),
                "env".to_owned(),
                "net".to_owned(),
            ],
        );
        let rt = PluginRuntime::builder()
            .capabilities(caps)
            .build()
            .expect("runtime builds");

        for g in surface().gated {
            let req = format!(
                "kage.request_capabilities({{'{}'}}); return type({}) == 'function'",
                g.cap, g.func.path
            );
            let granted = rt.eval_plugin("trusted", &req).expect("granted eval");
            assert_eq!(
                granted.as_boolean(),
                Some(true),
                "{} should resolve when {} is granted",
                g.func.path,
                g.cap
            );

            let ungranted = rt
                .eval_plugin("other", &format!("return {} == nil", g.func.path))
                .expect("ungranted eval");
            assert_eq!(
                ungranted.as_boolean(),
                Some(true),
                "{} must be absent without {}",
                g.func.path,
                g.cap
            );
        }
    }
}
