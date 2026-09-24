//! Option registry and value store.
//!
//! [`OPTIONS`] lists every option with its TOML path, type, default and
//! doc. An [`OptionStore`] holds the current values, seeded from a
//! loaded [`Config`] and then written by Lua or the TUI. Every set
//! records its source, queues an [`OptionChange`] for the host to
//! apply, and bumps a generation counter. The plugin runtime fires the
//! `option_set` event for each set.

use crate::config::{Config, EditorMode};
use crate::keymap;

/// Type, bounds and default of an option.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OptionKind {
    /// A boolean.
    Bool {
        /// Default value.
        default: bool,
    },
    /// An integer in `min..=max`.
    Int {
        /// Smallest valid value.
        min: i64,
        /// Largest valid value.
        max: i64,
        /// Default value.
        default: i64,
    },
    /// A number from 0 to 1.
    Fraction {
        /// Default value.
        default: f64,
    },
    /// One string out of a fixed set.
    Choice {
        /// Valid values.
        values: &'static [&'static str],
        /// Default value.
        default: &'static str,
    },
    /// A non-empty string. The host applies any further check, such as
    /// a known theme name.
    Str {
        /// Default value.
        default: &'static str,
    },
    /// A single key in keymap notation.
    Key {
        /// Default value.
        default: &'static str,
    },
}

/// One entry of [`OPTIONS`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OptionDef {
    /// Name, as in `kage.opt.<name>`.
    pub name: &'static str,
    /// Dotted path of the matching `config.toml` key.
    pub toml: &'static str,
    /// Type, bounds and default.
    pub kind: OptionKind,
    /// One-line description.
    pub doc: &'static str,
    /// Plugin API generation that introduced the option.
    pub since: u32,
    /// Whether a change applies while running. Other options apply at
    /// the next session start.
    pub live: bool,
}

/// Values [`OptionKind::Choice`] accepts for `thinking_level`: the
/// thinking ladder, or empty for no configured level.
const THINKING_LEVELS: &[&str] = &["", "off", "minimal", "low", "medium", "high", "xhigh"];

/// Every option.
pub const OPTIONS: &[OptionDef] = &[
    OptionDef {
        name: "theme",
        toml: "ui.theme",
        kind: OptionKind::Str { default: "default" },
        doc: "Color theme, bundled or from the themes directory.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "mouse",
        toml: "ui.mouse",
        kind: OptionKind::Bool { default: true },
        doc: "Capture mouse events.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "editor",
        toml: "ui.editor",
        kind: OptionKind::Choice {
            values: &["vim", "modeless"],
            default: "modeless",
        },
        doc: "Prompt editing style.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "input_min_lines",
        toml: "ui.input_min_lines",
        kind: OptionKind::Int {
            min: 1,
            max: 64,
            default: 1,
        },
        doc: "Minimum content rows of the input box.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "input_max_lines",
        toml: "ui.input_max_lines",
        kind: OptionKind::Int {
            min: 1,
            max: 64,
            default: 8,
        },
        doc: "Content rows the input box grows to before it scrolls.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "transcript_on_exit",
        toml: "ui.transcript_on_exit",
        kind: OptionKind::Choice {
            values: &["full", "last", "none"],
            default: "full",
        },
        doc: "What prints after exit: the whole transcript, from the last prompt on, or only the session path.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "thinking_level",
        toml: "ui.thinking_level",
        kind: OptionKind::Choice {
            values: THINKING_LEVELS,
            default: "",
        },
        doc: "Thinking level for new sessions, or empty for the default.",
        since: 2,
        live: false,
    },
    OptionDef {
        name: "compaction_threshold",
        toml: "loop.compaction_threshold",
        kind: OptionKind::Fraction { default: 0.8 },
        doc: "Fraction of the context window that triggers compaction. 0 turns compaction off.",
        since: 2,
        live: false,
    },
    OptionDef {
        name: "leader",
        toml: "keybindings.leader",
        kind: OptionKind::Key { default: "\\" },
        doc: "The key `<leader>` expands to when a mapping is set.",
        since: 2,
        live: true,
    },
    OptionDef {
        name: "timeoutlen",
        toml: "keybindings.timeoutlen",
        kind: OptionKind::Int {
            min: 0,
            max: 5000,
            default: 1000,
        },
        doc: "Milliseconds a mapping that is also a prefix waits for more keys.",
        since: 2,
        live: true,
    },
];

/// Look up an option by name.
#[must_use]
pub fn find(name: &str) -> Option<&'static OptionDef> {
    OPTIONS.iter().find(|def| def.name == name)
}

/// Look up an option by name, or fail with [`OptionError::Unknown`]
/// listing every valid name.
pub fn lookup(name: &str) -> Result<&'static OptionDef, OptionError> {
    find(name).ok_or_else(|| unknown(name))
}

fn unknown(name: &str) -> OptionError {
    OptionError::Unknown {
        name: name.to_owned(),
        valid: OPTIONS
            .iter()
            .map(|def| def.name)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

impl OptionDef {
    /// The default value.
    #[must_use]
    pub fn default_value(&self) -> OptionValue {
        match self.kind {
            OptionKind::Bool { default } => OptionValue::Bool(default),
            OptionKind::Int { default, .. } => OptionValue::Int(default),
            OptionKind::Fraction { default } => OptionValue::Float(default),
            OptionKind::Choice { default, .. }
            | OptionKind::Str { default }
            | OptionKind::Key { default } => OptionValue::Str(default.to_owned()),
        }
    }

    /// Check `value` against the option's type and bounds. An integer
    /// is accepted where a fraction is expected.
    pub fn validate(&self, value: OptionValue) -> Result<OptionValue, OptionError> {
        let valid = match (&self.kind, value) {
            (OptionKind::Bool { .. }, v @ OptionValue::Bool(_)) => Some(v),
            (OptionKind::Int { min, max, .. }, OptionValue::Int(n))
                if (*min..=*max).contains(&n) =>
            {
                Some(OptionValue::Int(n))
            }
            (OptionKind::Fraction { .. }, OptionValue::Float(x)) => fraction(x),
            (OptionKind::Fraction { .. }, OptionValue::Int(n)) => {
                i32::try_from(n).ok().map(f64::from).and_then(fraction)
            }
            (OptionKind::Choice { values, .. }, OptionValue::Str(s))
                if values.contains(&s.as_str()) =>
            {
                Some(OptionValue::Str(s))
            }
            (OptionKind::Str { .. }, OptionValue::Str(s)) if !s.is_empty() => {
                Some(OptionValue::Str(s))
            }
            (OptionKind::Key { .. }, OptionValue::Str(s)) if keymap::parse_key(&s).is_ok() => {
                Some(OptionValue::Str(s))
            }
            _ => None,
        };
        valid.ok_or_else(|| OptionError::Invalid {
            name: self.name,
            expected: self.expected(),
        })
    }

    /// What the option accepts, as used in error messages.
    #[must_use]
    pub fn expected(&self) -> String {
        match self.kind {
            OptionKind::Bool { .. } => "a boolean".to_owned(),
            OptionKind::Int { min, max, .. } => format!("an integer from {min} to {max}"),
            OptionKind::Fraction { .. } => "a number from 0 to 1".to_owned(),
            OptionKind::Choice { values, .. } => {
                let quoted: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
                format!("one of {}", quoted.join(", "))
            }
            OptionKind::Str { .. } => "a non-empty string".to_owned(),
            OptionKind::Key { .. } => "a single key, such as `,` or `<Space>`".to_owned(),
        }
    }
}

fn fraction(x: f64) -> Option<OptionValue> {
    (0.0..=1.0).contains(&x).then_some(OptionValue::Float(x))
}

/// An option value.
#[derive(Clone, Debug, PartialEq)]
pub enum OptionValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A number.
    Float(f64),
    /// A string.
    Str(String),
}

impl OptionValue {
    /// The boolean, if this is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The integer, if this is one.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The number, if this is one.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(x) => Some(*x),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Where the current value of an option came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionSource {
    /// The registry default.
    Default,
    /// `config.toml` or a `KAGE_` environment variable.
    Toml,
    /// `init.lua` or a plugin.
    Lua,
    /// The TUI: a command or the settings dialog.
    Runtime,
}

impl OptionSource {
    /// Lowercase name, as reported to Lua.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Toml => "toml",
            Self::Lua => "lua",
            Self::Runtime => "runtime",
        }
    }
}

/// One set, queued for the host to apply.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionChange {
    /// Option name.
    pub name: &'static str,
    /// Value before the set.
    pub old: OptionValue,
    /// Value after the set.
    pub new: OptionValue,
    /// Who set it.
    pub source: OptionSource,
}

/// Current option values, their sources and the pending change queue.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionStore {
    values: Vec<(OptionValue, OptionSource)>,
    changes: Vec<OptionChange>,
    generation: u64,
}

impl Default for OptionStore {
    fn default() -> Self {
        Self {
            values: OPTIONS
                .iter()
                .map(|def| (def.default_value(), OptionSource::Default))
                .collect(),
            changes: Vec::new(),
            generation: 0,
        }
    }
}

impl OptionStore {
    /// Seed a store from a loaded config. Values that differ from the
    /// default get source [`OptionSource::Toml`]. An invalid value
    /// keeps the default and is returned as an error. Nothing is
    /// queued.
    #[must_use]
    pub fn from_config(config: &Config) -> (Self, Vec<OptionError>) {
        let mut store = Self::default();
        let mut errors = Vec::new();
        for (def, slot) in OPTIONS.iter().zip(&mut store.values) {
            let Some(value) = config_value(def.name, config) else {
                continue;
            };
            if value == slot.0 {
                continue;
            }
            match def.validate(value) {
                Ok(value) => *slot = (value, OptionSource::Toml),
                Err(err) => errors.push(err),
            }
        }
        (store, errors)
    }

    /// Current value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OptionValue> {
        index(name).map(|i| &self.values[i].0)
    }

    /// Source of the current value of `name`.
    #[must_use]
    pub fn source(&self, name: &str) -> Option<OptionSource> {
        index(name).map(|i| self.values[i].1)
    }

    /// Validate and set `name`, queue the change and bump the
    /// generation. Returns the queued change.
    pub fn set(
        &mut self,
        name: &str,
        value: OptionValue,
        source: OptionSource,
    ) -> Result<OptionChange, OptionError> {
        let i = index(name).ok_or_else(|| unknown(name))?;
        let def = &OPTIONS[i];
        let new = def.validate(value)?;
        let (old, _) = std::mem::replace(&mut self.values[i], (new.clone(), source));
        let change = OptionChange {
            name: def.name,
            old,
            new,
            source,
        };
        self.changes.push(change.clone());
        self.generation += 1;
        Ok(change)
    }

    /// Drain the queued changes, oldest first.
    pub fn take_changes(&mut self) -> Vec<OptionChange> {
        std::mem::take(&mut self.changes)
    }

    /// Counter bumped on every set.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

fn index(name: &str) -> Option<usize> {
    OPTIONS.iter().position(|def| def.name == name)
}

fn config_value(name: &str, config: &Config) -> Option<OptionValue> {
    let ui = &config.ui;
    Some(match name {
        "theme" => OptionValue::Str(ui.theme.clone()),
        "mouse" => OptionValue::Bool(ui.mouse),
        "editor" => OptionValue::Str(
            match ui.editor {
                EditorMode::Vim => "vim",
                EditorMode::Modeless => "modeless",
            }
            .to_owned(),
        ),
        "input_min_lines" => OptionValue::Int(i64::from(ui.input_min_lines)),
        "input_max_lines" => OptionValue::Int(i64::from(ui.input_max_lines)),
        "transcript_on_exit" => OptionValue::Str(ui.transcript_on_exit.clone()),
        "thinking_level" => OptionValue::Str(ui.thinking_level.clone().unwrap_or_default()),
        "compaction_threshold" => {
            OptionValue::Float(widen(config.loop_settings.compaction_threshold))
        }
        "leader" => OptionValue::Str(config.keybindings.leader.clone()),
        "timeoutlen" => OptionValue::Int(i64::from(config.keybindings.timeoutlen)),
        _ => return None,
    })
}

/// Widen through the shortest decimal form, so `0.8f32` reads as `0.8`
/// instead of `0.800000011920929`.
fn widen(x: f32) -> f64 {
    x.to_string().parse().unwrap_or(f64::from(x))
}

/// Errors from [`OptionStore::set`] and [`OptionDef::validate`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OptionError {
    /// No option has this name.
    #[error("unknown option `{name}` (valid: {valid})")]
    Unknown {
        /// The name as given.
        name: String,
        /// Every option name, comma-separated.
        valid: String,
    },
    /// The value does not fit the option.
    #[error("option `{name}` expects {expected}")]
    Invalid {
        /// Option name.
        name: &'static str,
        /// What the option accepts.
        expected: String,
    },
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use crate::test_support::process_globals;

    fn load(toml: &str) -> Config {
        let _globals = process_globals();
        let mut out = None;
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", toml)?;
            out = Some(Config::load(jail.directory().join("config.toml").as_path()).unwrap());
            Ok(())
        });
        out.unwrap()
    }

    #[test]
    fn defaults_validate_and_seed_the_default_store() {
        let store = OptionStore::default();
        for def in OPTIONS {
            assert_eq!(def.validate(def.default_value()), Ok(def.default_value()));
            assert_eq!(store.get(def.name), Some(&def.default_value()));
            assert_eq!(store.source(def.name), Some(OptionSource::Default));
        }
    }

    #[test]
    fn default_config_seeds_defaults_only() {
        let (store, errors) = OptionStore::from_config(&Config::default());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(store, OptionStore::default());
    }

    #[test]
    fn toml_paths_exist_in_serialized_config() {
        let mut cfg = Config::default();
        cfg.ui.thinking_level = Some("high".to_owned());
        cfg.keybindings.leader = ",".to_owned();
        cfg.keybindings.timeoutlen = 300;
        let value = toml::Value::try_from(&cfg).unwrap();
        for def in OPTIONS {
            let found = def
                .toml
                .split('.')
                .try_fold(&value, |node, part| node.get(part));
            assert!(found.is_some(), "`{}` is not in Config", def.toml);
        }
    }

    #[test]
    fn toml_values_seed_the_store_with_source_toml() {
        for (name, literal, expected) in [
            (
                "theme",
                "\"tokyo-night\"",
                OptionValue::Str("tokyo-night".into()),
            ),
            ("mouse", "false", OptionValue::Bool(false)),
            ("editor", "\"vim\"", OptionValue::Str("vim".into())),
            ("input_min_lines", "2", OptionValue::Int(2)),
            ("input_max_lines", "12", OptionValue::Int(12)),
            (
                "transcript_on_exit",
                "\"last\"",
                OptionValue::Str("last".into()),
            ),
            (
                "thinking_level",
                "\"high\"",
                OptionValue::Str("high".into()),
            ),
            ("compaction_threshold", "0.6", OptionValue::Float(0.6)),
            ("leader", "\"<Space>\"", OptionValue::Str("<Space>".into())),
            ("timeoutlen", "300", OptionValue::Int(300)),
        ] {
            let def = find(name).unwrap();
            let (table, key) = def.toml.rsplit_once('.').unwrap();
            let cfg = load(&format!("[{table}]\n{key} = {literal}\n"));
            let (store, errors) = OptionStore::from_config(&cfg);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(store.get(name), Some(&expected), "{name}");
            assert_eq!(store.source(name), Some(OptionSource::Toml), "{name}");
        }
    }

    #[test]
    fn env_override_wins_over_toml() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", "[ui]\ntheme = \"tokyo-night\"\n")?;
            jail.set_env("KAGE_UI__THEME", "catppuccin-mocha");
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let (store, _) = OptionStore::from_config(&cfg);
            assert_eq!(
                store.get("theme"),
                Some(&OptionValue::Str("catppuccin-mocha".into()))
            );
            assert_eq!(store.source("theme"), Some(OptionSource::Toml));
            Ok(())
        });
    }

    #[test]
    fn invalid_toml_value_keeps_default_and_reports() {
        let cfg = load("[ui]\ninput_max_lines = 100\nthinking_level = \"huge\"\n");
        let (store, errors) = OptionStore::from_config(&cfg);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(store.get("input_max_lines"), Some(&OptionValue::Int(8)));
        assert_eq!(store.source("input_max_lines"), Some(OptionSource::Default));
    }

    #[test]
    fn set_validates_enums_and_ranges() {
        let mut store = OptionStore::default();
        let err = store
            .set(
                "editor",
                OptionValue::Str("emacs".into()),
                OptionSource::Lua,
            )
            .unwrap_err();
        assert!(err.to_string().contains("\"vim\", \"modeless\""), "{err}");
        for bad in [0, 65] {
            assert!(
                store
                    .set("input_min_lines", OptionValue::Int(bad), OptionSource::Lua)
                    .is_err()
            );
        }
        let err = store
            .set("input_min_lines", OptionValue::Int(65), OptionSource::Lua)
            .unwrap_err();
        assert!(err.to_string().contains("from 1 to 64"), "{err}");
        assert!(
            store
                .set("input_min_lines", OptionValue::Int(64), OptionSource::Lua)
                .is_ok()
        );
        assert!(
            store
                .set("mouse", OptionValue::Int(1), OptionSource::Lua)
                .is_err()
        );
        for bad in [-0.5, 1.5, f64::NAN] {
            assert!(
                store
                    .set(
                        "compaction_threshold",
                        OptionValue::Float(bad),
                        OptionSource::Lua
                    )
                    .is_err()
            );
        }
        assert_eq!(
            store
                .set(
                    "compaction_threshold",
                    OptionValue::Int(1),
                    OptionSource::Lua
                )
                .unwrap()
                .new,
            OptionValue::Float(1.0)
        );
        assert!(
            store
                .set("leader", OptionValue::Str("ab".into()), OptionSource::Lua)
                .is_err()
        );
        assert!(
            store
                .set(
                    "leader",
                    OptionValue::Str("<Space>".into()),
                    OptionSource::Lua
                )
                .is_ok()
        );
        assert!(
            store
                .set("theme", OptionValue::Str(String::new()), OptionSource::Lua)
                .is_err()
        );
        assert!(
            store
                .set(
                    "thinking_level",
                    OptionValue::Str(String::new()),
                    OptionSource::Lua
                )
                .is_ok()
        );
        let err = store
            .set("nope", OptionValue::Bool(true), OptionSource::Lua)
            .unwrap_err();
        assert!(err.to_string().contains("theme, mouse"), "{err}");
    }

    #[test]
    fn transcript_on_exit_takes_its_three_values() {
        let mut store = OptionStore::default();
        assert_eq!(
            store.get("transcript_on_exit"),
            Some(&OptionValue::Str("full".into()))
        );
        for value in ["full", "last", "none"] {
            assert!(
                store
                    .set(
                        "transcript_on_exit",
                        OptionValue::Str(value.into()),
                        OptionSource::Lua
                    )
                    .is_ok(),
                "{value}"
            );
        }
        let err = store
            .set(
                "transcript_on_exit",
                OptionValue::Str("all".into()),
                OptionSource::Lua,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("\"full\", \"last\", \"none\""),
            "{err}"
        );
    }

    #[test]
    fn set_records_source_and_queues_changes() {
        let mut store = OptionStore::default();
        let change = store
            .set("mouse", OptionValue::Bool(false), OptionSource::Lua)
            .unwrap();
        assert_eq!(
            change,
            OptionChange {
                name: "mouse",
                old: OptionValue::Bool(true),
                new: OptionValue::Bool(false),
                source: OptionSource::Lua,
            }
        );
        store
            .set(
                "editor",
                OptionValue::Str("vim".into()),
                OptionSource::Runtime,
            )
            .unwrap();
        assert_eq!(store.source("mouse"), Some(OptionSource::Lua));
        assert_eq!(store.source("editor"), Some(OptionSource::Runtime));
        assert_eq!(store.generation(), 2);
        let names: Vec<&str> = store.take_changes().iter().map(|c| c.name).collect();
        assert_eq!(names, ["mouse", "editor"]);
        assert!(store.take_changes().is_empty());
        assert!(
            store
                .set("mouse", OptionValue::Int(0), OptionSource::Lua)
                .is_err()
        );
        assert_eq!(store.generation(), 2);
    }

    #[test]
    fn thinking_levels_match_the_ladder() {
        for level in &THINKING_LEVELS[1..] {
            assert!(
                crate::ThinkingLevel::parse(level).is_some(),
                "{level} is not a thinking level"
            );
        }
    }
}
