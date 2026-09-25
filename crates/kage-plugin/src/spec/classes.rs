//! Generated-stub class (`---@class`) record declarations.

use super::{Class, Field};

pub(super) const CLASSES: &[Class] = &[
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
                doc: "Only user is wired.",
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
    Class {
        name: "kage.AutocmdOpts",
        doc: &["Options for `kage.api.autocmd_create`."],
        fields: &[
            Field {
                name: "callback",
                ty: "fun(ev: kage.AutocmdEvent): any",
                doc: "Called when the event fires.",
            },
            Field {
                name: "group?",
                ty: "integer|string",
                doc: "Group id or name from `kage.api.augroup_create`.",
            },
            Field {
                name: "pattern?",
                ty: "string|string[]",
                doc: "Exact match values; `*` (the default) matches all.",
            },
            Field {
                name: "once?",
                ty: "boolean",
                doc: "Delete the autocmd before its first call.",
            },
            Field {
                name: "desc?",
                ty: "string",
                doc: "Shown in error messages.",
            },
        ],
    },
    Class {
        name: "kage.AutocmdEvent",
        doc: &[
            "What an autocmd callback receives. `match` is the tool name",
            "for `tool_call` and `tool_result`, the new value for",
            "`model_select` and `thinking_level_select`, the option name",
            "for `option_set`, the theme name for `color_scheme`, and the",
            "exec pattern for `user`.",
        ],
        fields: &[
            Field {
                name: "id",
                ty: "integer",
                doc: "Autocmd id.",
            },
            Field {
                name: "event",
                ty: "kage.Event",
                doc: "Event name.",
            },
            Field {
                name: "match?",
                ty: "string",
                doc: "Value the patterns matched against.",
            },
            Field {
                name: "group?",
                ty: "integer",
                doc: "Group id.",
            },
            Field {
                name: "data",
                ty: "any",
                doc: "Event payload.",
            },
        ],
    },
    Class {
        name: "kage.KeymapOpts",
        doc: &["Options for `kage.keymap.set` and `kage.api.keymap_set`."],
        fields: &[
            Field {
                name: "desc?",
                ty: "string",
                doc: "Shown in `?` help. Mappings without one are hidden there.",
            },
            Field {
                name: "group?",
                ty: "string",
                doc: "Help section. Defaults to `other`.",
            },
        ],
    },
    Class {
        name: "kage.HlSpec",
        doc: &[
            "A highlight group. Colors are `#rrggbb`, a color name",
            "(`red`, `lightblue`, ...) or a palette index `0` to `255`.",
        ],
        fields: &[
            Field {
                name: "fg?",
                ty: "string",
                doc: "Foreground color.",
            },
            Field {
                name: "bg?",
                ty: "string",
                doc: "Background color.",
            },
            Field {
                name: "bold?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "italic?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "underline?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "dim?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "reverse?",
                ty: "boolean",
                doc: "Swap foreground and background.",
            },
            Field {
                name: "link?",
                ty: "string",
                doc: "Group to follow. Wins over every other field.",
            },
        ],
    },
    Class {
        name: "kage.Span",
        doc: &[
            "A styled run of text in chrome rows and block renderers.",
            "Styles resolve when the row is painted, so they follow",
            "theme switches.",
        ],
        fields: &[
            Field {
                name: "text",
                ty: "string",
                doc: "",
            },
            Field {
                name: "hl?",
                ty: "string",
                doc: "Highlight group whose colors and attributes apply first.",
            },
            Field {
                name: "fg?",
                ty: "string",
                doc: "A group name (its fg), a theme role name, or a color.",
            },
            Field {
                name: "bg?",
                ty: "string",
                doc: "A group name (its bg), a theme role name, or a color.",
            },
            Field {
                name: "bold?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "dim?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "italic?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "underline?",
                ty: "boolean",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.Action",
        doc: &["A Rust action from `kage.action`, used as a mapping rhs."],
        fields: &[],
    },
    Class {
        name: "kage.SlotSpec",
        doc: &[
            "The layout of a slot. `header`, `activity`, `input_pill`",
            "and `footer` take `left`, `right` and `sep`; `start` takes",
            "`lines`. The `header` and `activity` rows collapse while",
            "they paint nothing. An item is a built-in component name, a",
            "`kage.Span`, or a `kage.SlotComponent`.",
        ],
        fields: &[
            Field {
                name: "left?",
                ty: "(kage.Component|kage.Span|kage.SlotComponent)[]",
                doc: "Painted from the left edge.",
            },
            Field {
                name: "right?",
                ty: "(kage.Component|kage.Span|kage.SlotComponent)[]",
                doc: "Painted against the right edge.",
            },
            Field {
                name: "sep?",
                ty: "string",
                doc: "Painted between two items that both have output.",
            },
            Field {
                name: "lines?",
                ty: "(kage.Component|kage.Span|kage.SlotComponent)[]",
                doc: "One line per item, for `start`.",
            },
        ],
    },
    Class {
        name: "kage.SlotComponent",
        doc: &[
            "A slot component rendered by Lua. Its output is kept and",
            "recomputed only when a listed event fires, its interval",
            "elapses, `kage.api.redraw` is called, its slot is set, or",
            "the terminal width changes.",
        ],
        fields: &[
            Field {
                name: "render",
                ty: "fun(ctx: kage.SlotContext): any|nil",
                doc: "Returns the same shape as a block renderer.",
            },
            Field {
                name: "events?",
                ty: "string[]",
                doc: "Event names, each optionally followed by a space and a pattern.",
            },
            Field {
                name: "interval?",
                ty: "integer",
                doc: "Recompute every this many milliseconds (at least 50).",
            },
            Field {
                name: "hl?",
                ty: "string",
                doc: "Highlight group applied under the spans' own styles.",
            },
        ],
    },
    Class {
        name: "kage.SlotContext",
        doc: &["What a `kage.SlotComponent` render receives."],
        fields: &[
            Field {
                name: "width",
                ty: "integer",
                doc: "Terminal width in columns.",
            },
            Field {
                name: "model",
                ty: "string",
                doc: "Active `provider:model` id.",
            },
            Field {
                name: "thinking",
                ty: "string",
                doc: "Thinking level the next run sends, `off` when it sends none.",
            },
            Field {
                name: "thinking_auto",
                ty: "boolean",
                doc: "Whether the level is automatic rather than chosen.",
            },
            Field {
                name: "permission_mode?",
                ty: "string",
                doc: "Session permission override, if any.",
            },
            Field {
                name: "working",
                ty: "boolean",
                doc: "Whether a run is in flight.",
            },
            Field {
                name: "usage?",
                ty: "table",
                doc: "`{ total = { input, output, cache_read, cache_write }, context_used, context_window, cost }`.",
            },
            Field {
                name: "session",
                ty: "{ id: string, title?: string }",
                doc: "Active session.",
            },
            Field {
                name: "cwd",
                ty: "string",
                doc: "Working directory.",
            },
            Field {
                name: "mode",
                ty: "string",
                doc: "Editor mode: `normal`, `insert` or `visual`.",
            },
        ],
    },
];
