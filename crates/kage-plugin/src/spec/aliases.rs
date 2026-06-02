//! Generated-stub alias (`---@alias`) declarations.

use super::Alias;

pub(super) const ALIASES: &[Alias] = &[
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
