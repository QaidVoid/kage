//! Typed command specification for slash and colon commands.
//!
//! Both the `:` ex-line and the `/` palette share the same command
//! registry. [`CommandSpec`] describes a command's name, aliases,
//! description, argument schema, and which surfaces expose it.
//! [`ParsedArgs`] holds the result of parsing a raw input string
//! against a spec's [`ArgSpec`] list.
//!
//! The registry is consumed by the completion engine (PN.3) and the
//! tokenizer/parser (PN.3), which in turn feed the `CommandLine`
//! widget (PN.4) and the slash palette (PN.6). This module defines
//! only the types; wiring into the TUI happens in later PN tasks.

use std::collections::HashMap;

/// Where a command is exposed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandCategory {
    /// `/` palette only (insert mode).
    Slash,
    /// `:` ex-line only (normal mode).
    Colon,
    /// Both `:` and `/`.
    Both,
}

/// Source for a [`ArgSpec::DynamicChoice`] argument.
#[derive(Clone, Debug)]
pub enum ArgSource {
    /// Provider-colon-model identifiers from the provider registry.
    Models,
    /// Bundled and user-installed theme names.
    Themes,
    /// Resumable session identifiers.
    Sessions,
    /// Names registered by Lua plugins via `kage.register_command`.
    PluginCommands,
    /// Custom closure returning the candidate values. The closure
    /// receives no context in this initial version; PN.3 may widen
    /// the signature to accept a `&CommandCtx`.
    Custom(fn() -> Vec<String>),
}

/// Schema for a single command argument.
#[derive(Clone, Debug)]
pub enum ArgSpec {
    /// Free text consuming the rest of the input line after the
    /// command name (e.g. `:compact <prompt>`).
    Rest {
        /// Parameter name shown in completion hints.
        name: &'static str,
        /// Whether the argument can be omitted.
        optional: bool,
        /// Short placeholder displayed in inline help.
        hint: &'static str,
    },
    /// One token from a fixed set of values (e.g. `:mouse on|off|toggle`).
    Choice {
        /// Parameter name shown in completion hints.
        name: &'static str,
        /// Accepted values.
        values: &'static [&'static str],
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// One token chosen from a runtime-computed list
    /// (e.g. `:model <provider:model>`).
    DynamicChoice {
        /// Parameter name shown in completion hints.
        name: &'static str,
        /// Runtime value source.
        source: ArgSource,
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// File path under the working directory (autocomplete via fs walk).
    Path {
        /// Parameter name shown in completion hints.
        name: &'static str,
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// Session identifier (autocomplete via session lister).
    SessionId {
        /// Parameter name shown in completion hints.
        name: &'static str,
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// Boolean flag (e.g. `:fold all|none`).
    Flag {
        /// Parameter name shown in completion hints.
        name: &'static str,
    },
}

/// Owned counterpart to [`ArgSpec`] for runtime-supplied schemas
/// (currently used for plugin slash commands). The host translates
/// each variant into a leaked `&'static ArgSpec` at registration time
/// so plugin commands participate in the same completion path as
/// the static builtins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnedArgSpec {
    /// See [`ArgSpec::Rest`].
    Text {
        /// Parameter name.
        name: String,
        /// Whether the argument can be omitted.
        optional: bool,
        /// Placeholder for inline help.
        hint: String,
    },
    /// See [`ArgSpec::Choice`].
    Choice {
        /// Parameter name.
        name: String,
        /// Accepted values.
        values: Vec<String>,
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// See [`ArgSpec::Path`].
    Path {
        /// Parameter name.
        name: String,
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// See [`ArgSpec::SessionId`].
    Session {
        /// Parameter name.
        name: String,
        /// Whether the argument can be omitted.
        optional: bool,
    },
    /// See [`ArgSpec::Flag`].
    Flag {
        /// Parameter name.
        name: String,
    },
}

/// One plugin-registered slash command surfaced to the host. The host
/// builds these from the plugin runtime's registered commands and
/// hands them to [`crate::App::set_plugin_commands`], which leaks the
/// owned fields into a `&'static CommandSpec` so they slot into the
/// same registry the builtins use.
#[derive(Clone, Debug)]
pub struct PluginCommand {
    /// Command name without any leading `/` or `:`.
    pub name: String,
    /// Alternate names that resolve to this command.
    pub aliases: Vec<String>,
    /// Registered via `kage.override_command`: may shadow a built-in
    /// and is dispatched ahead of it.
    pub is_override: bool,
    /// One-line description shown in the palette and help output.
    pub description: String,
    /// Argument schema declared by the plugin. Empty means the
    /// command takes no arguments.
    pub args: Vec<OwnedArgSpec>,
}

/// Format the argument schema as a compact hint string for inline
/// help, e.g. `"<id>"` for a required dynamic choice, `"[name]"` for
/// an optional one, `"<on|off|toggle>"` for a required fixed choice.
/// Multi-arg commands join hints with spaces. Returns an empty string
/// when `args` is empty so callers can render argless commands
/// without trailing whitespace.
#[must_use]
pub fn arg_hints_text(args: &[ArgSpec]) -> String {
    args.iter().map(arg_hint_one).collect::<Vec<_>>().join(" ")
}

fn arg_hint_one(arg: &ArgSpec) -> String {
    match arg {
        ArgSpec::Rest { name, optional, .. }
        | ArgSpec::DynamicChoice { name, optional, .. }
        | ArgSpec::Path { name, optional }
        | ArgSpec::SessionId { name, optional } => {
            if *optional {
                format!("[{name}]")
            } else {
                format!("<{name}>")
            }
        }
        ArgSpec::Choice {
            values, optional, ..
        } => {
            let inside = values.join("|");
            if *optional {
                format!("[{inside}]")
            } else {
                format!("<{inside}>")
            }
        }
        ArgSpec::Flag { name } => format!("[{name}]"),
    }
}

impl ArgSpec {
    /// Parameter name for this argument position.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Rest { name, .. }
            | Self::Choice { name, .. }
            | Self::DynamicChoice { name, .. }
            | Self::Path { name, .. }
            | Self::SessionId { name, .. }
            | Self::Flag { name } => name,
        }
    }

    /// Whether the argument can be omitted.
    #[must_use]
    pub fn optional(&self) -> bool {
        match self {
            Self::Rest { optional, .. }
            | Self::Choice { optional, .. }
            | Self::DynamicChoice { optional, .. }
            | Self::Path { optional, .. }
            | Self::SessionId { optional, .. } => *optional,
            Self::Flag { .. } => true,
        }
    }
}

/// Parsed value for a single argument position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgValue {
    /// Free-form text (from [`ArgSpec::Rest`] or unquoted input).
    Text(String),
    /// One value from a fixed set (from [`ArgSpec::Choice`]).
    Choice(String),
    /// A file path (from [`ArgSpec::Path`]).
    Path(String),
    /// A session identifier (from [`ArgSpec::SessionId`]).
    SessionId(String),
    /// A boolean value (from [`ArgSpec::Flag`]).
    Bool(bool),
}

/// Map from argument name to its parsed value.
pub type ParsedArgs = HashMap<&'static str, ArgValue>;

/// Full specification of one command.
///
/// Fields use `&'static str` and `&'static [&'static str]` so the
/// entire registry can live as a `const` slice in the binary. Plugin
/// commands will use a separate heap-allocated variant (PN.8).
#[derive(Debug)]
pub struct CommandSpec {
    /// Primary command name (e.g. `"model"`).
    pub name: &'static str,
    /// Alternate names that dispatch to the same handler
    /// (e.g. `["q"]` for the `quit` command).
    pub aliases: &'static [&'static str],
    /// One-line description shown in the palette and help output.
    pub description: &'static str,
    /// Which surfaces expose this command.
    pub category: CommandCategory,
    /// Ordered argument positions. Empty for argument-less commands.
    pub args: &'static [ArgSpec],
    /// Nested commands that share the parent's namespace. The parser
    /// tries to match the first post-head token against a subcommand
    /// before consuming arguments; the completer suggests subcommand
    /// names at that position alongside the parent's first arg.
    /// Subcommands may themselves carry args and further subcommands,
    /// so the tree can nest arbitrarily.
    pub subcommands: &'static [CommandSpec],
}

impl CommandSpec {
    /// Iterate over all names by which this command can be invoked:
    /// the primary name followed by aliases.
    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }

    /// Look up the first argument spec by parameter name.
    #[must_use]
    pub fn arg_by_name(&self, name: &str) -> Option<&ArgSpec> {
        self.args.iter().find(|a| a.name() == name)
    }

    /// Look up a subcommand by primary name or alias.
    #[must_use]
    pub fn subcommand(&self, name: &str) -> Option<&CommandSpec> {
        self.subcommands
            .iter()
            .find(|sub| sub.names().any(|n| n == name))
    }
}

/// Built-in commands offered by the slash palette and the `:` line.
/// Plugin commands join this set at runtime via
/// [`crate::App::set_plugin_commands`].
pub(crate) static BUILTIN_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "quit",
        aliases: &["q"],
        description: "leave the TUI",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "cancel",
        aliases: &[],
        description: "cancel the in-flight turn",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "model",
        aliases: &[],
        description: "switch to provider:model",
        category: CommandCategory::Both,
        args: &[ArgSpec::DynamicChoice {
            name: "id",
            source: ArgSource::Models,
            optional: false,
        }],
        subcommands: &[],
    },
    CommandSpec {
        name: "fold",
        aliases: &[],
        description: "fold every foldable block",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "scope",
            values: &["all"],
            optional: false,
        }],
        subcommands: &[],
    },
    CommandSpec {
        name: "unfold",
        aliases: &[],
        description: "unfold every foldable block",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "scope",
            values: &["all"],
            optional: false,
        }],
        subcommands: &[],
    },
    CommandSpec {
        name: "theme",
        aliases: &[],
        description: "manage UI palette",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[
            CommandSpec {
                name: "list",
                aliases: &[],
                description: "list every bundled theme",
                category: CommandCategory::Both,
                args: &[],
                subcommands: &[],
            },
            CommandSpec {
                name: "set",
                aliases: &[],
                description: "switch to a bundled theme",
                category: CommandCategory::Both,
                args: &[ArgSpec::DynamicChoice {
                    name: "name",
                    source: ArgSource::Themes,
                    optional: false,
                }],
                subcommands: &[],
            },
            CommandSpec {
                name: "current",
                aliases: &[],
                description: "show the active theme name",
                category: CommandCategory::Both,
                args: &[],
                subcommands: &[],
            },
        ],
    },
    CommandSpec {
        name: "mouse",
        aliases: &[],
        description: "toggle mouse capture (off lets the terminal handle text selection)",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "state",
            values: &["on", "off", "toggle"],
            optional: true,
        }],
        subcommands: &[],
    },
    CommandSpec {
        name: "help",
        aliases: &[],
        description: "show available commands",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        description: "summarize older history into a single message",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "settings",
        aliases: &[],
        description: "open the settings dialog (theme, model, mouse, autocompaction)",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "tree",
        aliases: &[],
        description: "browse the session fork forest (resume, fork, delete)",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "clone",
        aliases: &[],
        description: "duplicate the session to a new id and continue in the clone",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "new",
        aliases: &[],
        description: "start a fresh empty session, keeping the current model",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "export",
        aliases: &[],
        description: "write the session transcript to a Markdown file",
        category: CommandCategory::Both,
        args: &[ArgSpec::Path {
            name: "file",
            optional: true,
        }],
        subcommands: &[],
    },
    CommandSpec {
        name: "clear",
        aliases: &[],
        description: "clear the rendered conversation buffer",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "keybindings",
        aliases: &["keys"],
        description: "list active key bindings (config, plugin, reserved)",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "events",
        aliases: &[],
        description: "list events plugins can hook with kage.on",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "attach",
        aliases: &["img"],
        description: "attach an image (a file path, or the clipboard if omitted)",
        category: CommandCategory::Both,
        args: &[ArgSpec::Path {
            name: "path",
            optional: true,
        }],
        subcommands: &[],
    },
];

/// Find a built-in command by primary name or alias. Returns `None`
/// for unknown command names.
#[must_use]
pub fn find_builtin_command(name: &str) -> Option<&'static CommandSpec> {
    BUILTIN_COMMANDS
        .iter()
        .find(|spec| spec.names().any(|n| n == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: CommandSpec = CommandSpec {
        name: "mouse",
        aliases: &[],
        description: "toggle mouse capture",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "state",
            values: &["on", "off", "toggle"],
            optional: false,
        }],
        subcommands: &[],
    };

    #[test]
    fn names_yields_primary() {
        let names: Vec<&str> = SPEC.names().collect();
        assert_eq!(names, vec!["mouse"]);
    }

    #[test]
    fn names_includes_aliases() {
        const QUIT: CommandSpec = CommandSpec {
            name: "quit",
            aliases: &["q"],
            description: "exit the TUI",
            category: CommandCategory::Both,
            args: &[],
            subcommands: &[],
        };
        let names: Vec<&str> = QUIT.names().collect();
        assert_eq!(names, vec!["quit", "q"]);
    }

    #[test]
    fn arg_by_name_finds_present_arg() {
        let arg = SPEC.arg_by_name("state").expect("should find state");
        assert!(matches!(arg, ArgSpec::Choice { name: "state", .. }));
    }

    #[test]
    fn arg_by_name_returns_none_for_missing() {
        assert!(SPEC.arg_by_name("bogus").is_none());
    }

    #[test]
    fn arg_spec_optional_delegates_to_variant() {
        let spec = ArgSpec::Rest {
            name: "prompt",
            optional: true,
            hint: "free text",
        };
        assert!(spec.optional());
        assert!(
            !ArgSpec::Choice {
                name: "x",
                values: &["a", "b"],
                optional: false,
            }
            .optional()
        );
        assert!(ArgSpec::Flag { name: "v" }.optional());
    }

    #[test]
    fn parsed_args_insert_and_lookup() {
        let mut args = ParsedArgs::new();
        args.insert("state", ArgValue::Choice("on".into()));
        assert_eq!(args.get("state"), Some(&ArgValue::Choice("on".into())));
        assert_eq!(args.get("missing"), None);
    }

    #[test]
    fn builtin_registry_finds_quit_by_primary_name() {
        let spec = find_builtin_command("quit").expect("quit should exist");
        assert_eq!(spec.name, "quit");
        assert!(spec.aliases.contains(&"q"));
    }

    #[test]
    fn builtin_registry_finds_quit_by_alias() {
        let spec = find_builtin_command("q").expect("q alias should resolve");
        assert_eq!(spec.name, "quit");
    }

    #[test]
    fn builtin_registry_finds_model() {
        let spec = find_builtin_command("model").expect("model should exist");
        assert_eq!(spec.args.len(), 1);
        assert!(matches!(
            &spec.args[0],
            ArgSpec::DynamicChoice { name: "id", .. }
        ));
    }

    #[test]
    fn builtin_registry_returns_none_for_unknown() {
        assert!(find_builtin_command("bogus").is_none());
    }

    #[test]
    fn builtin_registry_has_expected_command_count() {
        assert_eq!(BUILTIN_COMMANDS.len(), 18);
    }

    #[test]
    fn builtin_registry_includes_export() {
        let spec = find_builtin_command("export").expect("export should exist");
        assert_eq!(spec.name, "export");
        assert_eq!(spec.args.len(), 1);
        assert!(spec.subcommands.is_empty());
    }

    #[test]
    fn builtin_registry_includes_clone() {
        let spec = find_builtin_command("clone").expect("clone should exist");
        assert_eq!(spec.name, "clone");
        assert_eq!(spec.args.len(), 0);
        assert!(spec.subcommands.is_empty());
    }

    #[test]
    fn builtin_registry_includes_new() {
        let spec = find_builtin_command("new").expect("new should exist");
        assert_eq!(spec.name, "new");
        assert_eq!(spec.args.len(), 0);
        assert!(spec.subcommands.is_empty());
    }

    #[test]
    fn builtin_registry_includes_compact() {
        let spec = find_builtin_command("compact").expect("compact should exist");
        assert_eq!(spec.name, "compact");
        assert_eq!(spec.args.len(), 0);
    }

    #[test]
    fn builtin_registry_includes_settings() {
        let spec = find_builtin_command("settings").expect("settings should exist");
        assert_eq!(spec.name, "settings");
        assert_eq!(spec.args.len(), 0);
        assert_eq!(spec.subcommands.len(), 0);
    }

    #[test]
    fn builtin_registry_includes_tree() {
        let spec = find_builtin_command("tree").expect("tree should exist");
        assert_eq!(spec.name, "tree");
        assert_eq!(spec.args.len(), 0);
        assert_eq!(spec.subcommands.len(), 0);
    }
}
