//! Tool permission rules (`[permissions]`): allow / ask / deny.
//!
//! Built-in tools are allowed unless configured otherwise, which
//! preserves the historical auto-approve behavior.
//! A `[permissions.tools.<name>]` entry opts one tool into rules;
//! within an entry the `deny` patterns are checked first, then the
//! `allow` patterns, and the `default` action applies when neither
//! matches. Patterns are globs matched against the tool's "command
//! line": the `command` string for shell-style tools, otherwise the
//! compact JSON encoding of the whole input.
//!
//! MCP tools (`<server>__<tool>`) are the exception to the allow
//! default: a call to one without a `[permissions.tools.<name>]` entry
//! resolves through `[permissions.mcp]`, where unlisted servers ask.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::config_error;

/// One step on the permission ladder.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    /// Run the tool without prompting.
    #[default]
    Allow,
    /// Ask the host for an interactive decision before running.
    Ask,
    /// Refuse the call outright.
    Deny,
}

/// Rules for a single tool (`[permissions.tools.<name>]`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolPermissionRules {
    /// Action when neither `deny` nor `allow` matches. Defaults to
    /// `allow` so an entry can add `deny` patterns without opting
    /// the whole tool into prompts.
    pub default: PermissionAction,
    /// Glob patterns; a match runs the tool without prompting.
    pub allow: Vec<String>,
    /// Glob patterns; a match refuses the call.
    pub deny: Vec<String>,
}

/// The `[permissions]` table.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Opt-in path confinement: route the built-in file tools
    /// through escape-checked resolution so reads and writes stay
    /// under the working directory. Defaults to `false` (the
    /// historical behavior: paths resolve against the workdir but
    /// may escape it).
    pub confine_paths: bool,
    /// Per-tool rules, keyed by literal tool name (`bash`,
    /// `write`, `github__create_issue`, ...). No glob keys in
    /// v1: ordering overlapping patterns deterministically is not
    /// worth the confusion yet.
    pub tools: BTreeMap<String, ToolPermissionRules>,
    /// Action for tools of an MCP server, keyed by server name, when
    /// the tool has no `[permissions.tools.<name>]` entry. Servers
    /// not listed here ask.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, PermissionAction>,
}

impl PermissionsConfig {
    /// Whether the table carries no configuration at all.
    #[must_use]
    pub fn is_default(&self) -> bool {
        !self.confine_paths && self.tools.is_empty() && self.mcp.is_empty()
    }

    /// The action for a tool of MCP server `server` that has no
    /// per-tool entry: the `[permissions.mcp]` entry, or `ask` when
    /// the server is not listed.
    #[must_use]
    pub fn mcp_action(&self, server: &str) -> PermissionAction {
        self.mcp
            .get(server)
            .copied()
            .unwrap_or(PermissionAction::Ask)
    }

    /// Reject structurally broken configuration so `kage` refuses to
    /// start on rules it would silently misapply: empty tool names
    /// and server names, and empty or uncompilable glob patterns.
    ///
    /// # Errors
    ///
    /// A [`crate::error::Error`] describing the first problem found.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        if self.mcp.contains_key("") {
            return Err(config_error(
                "[permissions.mcp] keys must be non-empty server names".to_owned(),
            ));
        }
        for (tool, rules) in &self.tools {
            if tool.is_empty() {
                return Err(config_error(
                    "[permissions.tools] keys must be non-empty tool names".to_owned(),
                ));
            }
            for pattern in rules.allow.iter().chain(&rules.deny) {
                if pattern.is_empty() {
                    return Err(config_error(format!(
                        "[permissions.tools.{tool}] patterns must be non-empty"
                    )));
                }
                if let Err(e) = globset::Glob::new(pattern) {
                    return Err(config_error(format!(
                        "[permissions.tools.{tool}] pattern `{pattern}` does not compile: {e}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Evaluate the rules for `tool` against `subject` (see the
    /// module docs for extraction). Tools without an entry are
    /// allowed; otherwise `deny` wins over `allow`, and `default`
    /// applies when neither list matches.
    #[must_use]
    pub fn check(&self, tool: &str, subject: &str) -> PermissionAction {
        let Some(rules) = self.tools.get(tool) else {
            return PermissionAction::Allow;
        };
        if matches_any(&rules.deny, subject) {
            return PermissionAction::Deny;
        }
        if matches_any(&rules.allow, subject) {
            return PermissionAction::Allow;
        }
        rules.default
    }

    /// The string glob patterns match against for one tool input:
    /// the `command` field when the input carries one (shell-style
    /// tools), otherwise the compact JSON encoding of the whole
    /// input.
    #[must_use]
    pub fn subject_for(input: &serde_json::Value) -> String {
        match input.get("command").and_then(serde_json::Value::as_str) {
            Some(command) => command.to_owned(),
            None => input.to_string(),
        }
    }
}

/// Whether any pattern in `patterns` glob-matches `subject`.
fn matches_any(patterns: &[String], subject: &str) -> bool {
    patterns.iter().any(|p| match globset::Glob::new(p) {
        Ok(g) => g.compile_matcher().is_match(subject),
        Err(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(default: PermissionAction, allow: &[&str], deny: &[&str]) -> PermissionsConfig {
        PermissionsConfig {
            confine_paths: false,
            mcp: BTreeMap::new(),
            tools: [(
                "bash".to_owned(),
                ToolPermissionRules {
                    default,
                    allow: allow.iter().map(|s| (*s).to_owned()).collect(),
                    deny: deny.iter().map(|s| (*s).to_owned()).collect(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn missing_tool_entry_allows() {
        let cfg = PermissionsConfig::default();
        assert_eq!(cfg.check("bash", "rm -rf /"), PermissionAction::Allow);
        assert!(cfg.is_default());
    }

    #[test]
    fn deny_beats_allow() {
        let cfg = rules(PermissionAction::Allow, &["git *"], &["git push *", "rm *"]);
        assert_eq!(cfg.check("bash", "git status"), PermissionAction::Allow);
        assert_eq!(cfg.check("bash", "git push origin"), PermissionAction::Deny);
        assert_eq!(cfg.check("bash", "rm -rf /tmp/x"), PermissionAction::Deny);
    }

    #[test]
    fn default_applies_when_no_pattern_matches() {
        let cfg = rules(PermissionAction::Ask, &["cargo *"], &[]);
        assert_eq!(cfg.check("bash", "cargo test"), PermissionAction::Allow);
        assert_eq!(cfg.check("bash", "make all"), PermissionAction::Ask);
    }

    #[test]
    fn entry_default_allow_keeps_yolo_for_unmatched() {
        let cfg = rules(PermissionAction::Allow, &[], &["curl *"]);
        assert_eq!(cfg.check("bash", "echo hi"), PermissionAction::Allow);
        assert_eq!(cfg.check("bash", "curl http://x"), PermissionAction::Deny);
    }

    #[test]
    fn rules_are_per_tool() {
        let cfg = rules(PermissionAction::Deny, &[], &[]);
        assert_eq!(cfg.check("bash", "echo hi"), PermissionAction::Deny);
        assert_eq!(cfg.check("write", "{}"), PermissionAction::Allow);
    }

    #[test]
    fn subject_prefers_command_field() {
        let input = serde_json::json!({"command": "cargo test", "timeout": 30});
        assert_eq!(PermissionsConfig::subject_for(&input), "cargo test");
        let input = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(
            PermissionsConfig::subject_for(&input),
            r#"{"path":"src/main.rs"}"#
        );
    }

    #[test]
    fn validate_rejects_bad_glob() {
        let cfg = rules(PermissionAction::Allow, &[], &["[unclosed"]);
        assert!(cfg.validate().is_err());
        let cfg = rules(PermissionAction::Allow, &[], &["ok *"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn action_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&PermissionAction::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionAction::Ask).unwrap(),
            "\"ask\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionAction::Deny).unwrap(),
            "\"deny\""
        );
    }

    #[test]
    fn toml_roundtrip() {
        // Parsed standalone, so the `[permissions]` wrapper header is
        // absent: these tables are the module's own root.
        let src = r#"
            confine_paths = true

            [tools.bash]
            default = "ask"
            allow = ["git *"]
            deny = ["rm -rf *"]
        "#;
        let cfg: PermissionsConfig = toml::from_str(src).unwrap();
        assert!(cfg.confine_paths);
        assert_eq!(cfg.check("bash", "rm -rf /"), PermissionAction::Deny);
        assert_eq!(cfg.check("bash", "git status"), PermissionAction::Allow);
        assert_eq!(cfg.check("bash", "ls"), PermissionAction::Ask);
        assert!(!cfg.is_default());
    }

    #[test]
    fn mcp_action_defaults_to_ask_and_honours_entries() {
        let mut cfg = PermissionsConfig::default();
        assert_eq!(cfg.mcp_action("github"), PermissionAction::Ask);
        cfg.mcp.insert("github".to_owned(), PermissionAction::Allow);
        cfg.mcp.insert("shell".to_owned(), PermissionAction::Deny);
        assert_eq!(cfg.mcp_action("github"), PermissionAction::Allow);
        assert_eq!(cfg.mcp_action("shell"), PermissionAction::Deny);
        assert_eq!(cfg.mcp_action("other"), PermissionAction::Ask);
        assert!(!cfg.is_default());
    }

    #[test]
    fn mcp_table_parses_from_toml() {
        let src = r#"
            [mcp]
            github = "allow"
            shell = "deny"
        "#;
        let cfg: PermissionsConfig = toml::from_str(src).unwrap();
        assert_eq!(cfg.mcp_action("github"), PermissionAction::Allow);
        assert_eq!(cfg.mcp_action("shell"), PermissionAction::Deny);
        assert_eq!(cfg.mcp_action("fs"), PermissionAction::Ask);
    }
}
