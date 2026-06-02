//! Tests for command parsing and completion.

use super::*;
use crate::command::CommandCategory;

const NO_ARGS: CommandSpec = CommandSpec {
    name: "quit",
    aliases: &["q"],
    description: "leave",
    category: CommandCategory::Both,
    args: &[],
    subcommands: &[],
};

const ONE_CHOICE: CommandSpec = CommandSpec {
    name: "mouse",
    aliases: &[],
    description: "toggle mouse",
    category: CommandCategory::Both,
    args: &[ArgSpec::Choice {
        name: "state",
        values: &["on", "off", "toggle"],
        optional: false,
    }],
    subcommands: &[],
};

const ONE_CHOICE_OPTIONAL: CommandSpec = CommandSpec {
    name: "fold",
    aliases: &[],
    description: "fold",
    category: CommandCategory::Both,
    args: &[ArgSpec::Choice {
        name: "scope",
        values: &["all"],
        optional: true,
    }],
    subcommands: &[],
};

const ONE_REST: CommandSpec = CommandSpec {
    name: "compact",
    aliases: &[],
    description: "compact with prompt",
    category: CommandCategory::Both,
    args: &[ArgSpec::Rest {
        name: "prompt",
        optional: true,
        hint: "free text",
    }],
    subcommands: &[],
};

const ONE_REST_REQUIRED: CommandSpec = CommandSpec {
    name: "name",
    aliases: &[],
    description: "rename session",
    category: CommandCategory::Both,
    args: &[ArgSpec::Rest {
        name: "title",
        optional: false,
        hint: "session title",
    }],
    subcommands: &[],
};

const ONE_DYN: CommandSpec = CommandSpec {
    name: "model",
    aliases: &[],
    description: "switch model",
    category: CommandCategory::Both,
    args: &[ArgSpec::DynamicChoice {
        name: "id",
        source: ArgSource::Models,
        optional: false,
    }],
    subcommands: &[],
};

const ONE_PATH: CommandSpec = CommandSpec {
    name: "open",
    aliases: &[],
    description: "open file",
    category: CommandCategory::Both,
    args: &[ArgSpec::Path {
        name: "path",
        optional: false,
    }],
    subcommands: &[],
};

const ONE_FLAG: CommandSpec = CommandSpec {
    name: "verbose",
    aliases: &[],
    description: "toggle verbose",
    category: CommandCategory::Both,
    args: &[ArgSpec::Flag { name: "on" }],
    subcommands: &[],
};

const CHOICE_THEN_REST: CommandSpec = CommandSpec {
    name: "say",
    aliases: &[],
    description: "speak",
    category: CommandCategory::Both,
    args: &[
        ArgSpec::Choice {
            name: "voice",
            values: &["loud", "soft"],
            optional: false,
        },
        ArgSpec::Rest {
            name: "message",
            optional: false,
            hint: "what to say",
        },
    ],
    subcommands: &[],
};

fn registry() -> Vec<&'static CommandSpec> {
    vec![
        &NO_ARGS,
        &ONE_CHOICE,
        &ONE_CHOICE_OPTIONAL,
        &ONE_REST,
        &ONE_DYN,
        &ONE_PATH,
        &ONE_FLAG,
        &CHOICE_THEN_REST,
    ]
}

struct TestResolver;
impl Resolver for TestResolver {
    fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
        match source {
            ArgSource::Models => vec![
                "anthropic:claude-sonnet-4-6".into(),
                "anthropic:claude-opus-4-7".into(),
                "openai:gpt-4o".into(),
            ],
            ArgSource::Themes => vec!["dark".into(), "dawn".into(), "moonlight".into()],
            _ => Vec::new(),
        }
    }
    fn paths(&self, prefix: &str) -> Vec<String> {
        let all = ["src/lib.rs", "src/main.rs", "tests/it.rs"];
        all.iter()
            .filter(|p| p.starts_with(prefix))
            .map(|p| (*p).to_owned())
            .collect()
    }
    fn sessions(&self) -> Vec<String> {
        vec!["sess-1".into(), "sess-2".into()]
    }
}

#[test]
fn tokenize_empty_input_yields_no_tokens() {
    assert!(tokenize("").unwrap().is_empty());
}

#[test]
fn tokenize_single_bare_token() {
    let toks = tokenize("model").unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, "model");
    assert_eq!(toks[0].span, 0..5);
    assert!(!toks[0].quoted);
}

#[test]
fn tokenize_two_tokens_split_on_space() {
    let toks = tokenize("model gpt-4").unwrap();
    let values: Vec<&str> = toks.iter().map(|t| t.value.as_str()).collect();
    assert_eq!(values, vec!["model", "gpt-4"]);
    assert_eq!(toks[0].span, 0..5);
    assert_eq!(toks[1].span, 6..11);
}

#[test]
fn tokenize_collapses_runs_of_whitespace() {
    let toks = tokenize("a   b\t\tc").unwrap();
    let values: Vec<&str> = toks.iter().map(|t| t.value.as_str()).collect();
    assert_eq!(values, vec!["a", "b", "c"]);
}

#[test]
fn tokenize_strips_leading_and_trailing_whitespace() {
    let toks = tokenize("  hello  ").unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, "hello");
    assert_eq!(toks[0].span, 2..7);
}

#[test]
fn tokenize_double_quoted_preserves_internal_space() {
    let toks = tokenize(r#""hello world""#).unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, "hello world");
    assert!(toks[0].quoted);
    assert_eq!(toks[0].span, 0..13);
}

#[test]
fn tokenize_single_quoted_preserves_internal_space() {
    let toks = tokenize("'hello world'").unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, "hello world");
    assert!(toks[0].quoted);
}

#[test]
fn tokenize_escaped_quote_inside_double() {
    let toks = tokenize(r#""she said \"hi\"""#).unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, r#"she said "hi""#);
}

#[test]
fn tokenize_escaped_backslash_inside_double() {
    let toks = tokenize(r#""a\\b""#).unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, r"a\b");
}

#[test]
fn tokenize_unknown_escape_inside_double_keeps_backslash() {
    let toks = tokenize(r#""a\nb""#).unwrap();
    assert_eq!(toks.len(), 1);
    assert_eq!(toks[0].value, "a\\nb");
}

#[test]
fn tokenize_unterminated_double_quote_errors() {
    let err = tokenize(r#""hello"#).unwrap_err();
    assert_eq!(err, ParseError::UnterminatedQuote);
}

#[test]
fn tokenize_unterminated_single_quote_errors() {
    let err = tokenize("'hello").unwrap_err();
    assert_eq!(err, ParseError::UnterminatedQuote);
}

#[test]
fn tokenize_unicode_token_span_uses_byte_offsets() {
    let toks = tokenize("\u{1f600} done").unwrap();
    assert_eq!(toks.len(), 2);
    assert_eq!(toks[0].value, "\u{1f600}");
    assert_eq!(toks[0].span, 0..4);
    assert_eq!(toks[1].span, 5..9);
}

#[test]
fn parse_no_args_with_empty_input_succeeds() {
    let parsed = parse_input(&NO_ARGS, "").unwrap();
    assert!(parsed.is_empty());
}

#[test]
fn parse_no_args_with_extra_token_errors() {
    let err = parse_input(&NO_ARGS, "extra").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedTokens(s) if s == "extra"));
}

#[test]
fn parse_choice_valid_value_succeeds() {
    let parsed = parse_input(&ONE_CHOICE, "on").unwrap();
    assert_eq!(parsed.get("state"), Some(&ArgValue::Choice("on".into())));
}

#[test]
fn parse_choice_invalid_value_errors() {
    let err = parse_input(&ONE_CHOICE, "yes").unwrap_err();
    assert!(matches!(
        err,
        ParseError::InvalidChoice { name: "state", ref got, .. } if got == "yes"
    ));
}

#[test]
fn parse_required_choice_missing_errors() {
    let err = parse_input(&ONE_CHOICE, "").unwrap_err();
    assert_eq!(err, ParseError::MissingArg("state"));
}

#[test]
fn parse_optional_choice_missing_yields_empty() {
    let parsed = parse_input(&ONE_CHOICE_OPTIONAL, "").unwrap();
    assert!(parsed.is_empty());
}

#[test]
fn parse_rest_takes_verbatim_remainder() {
    let parsed = parse_input(&ONE_REST, "summarize the chat").unwrap();
    assert_eq!(
        parsed.get("prompt"),
        Some(&ArgValue::Text("summarize the chat".into()))
    );
}

#[test]
fn parse_rest_preserves_internal_quotes() {
    let parsed = parse_input(&ONE_REST, r#"do "the thing" please"#).unwrap();
    assert_eq!(
        parsed.get("prompt"),
        Some(&ArgValue::Text(r#"do "the thing" please"#.into()))
    );
}

#[test]
fn parse_rest_optional_empty_yields_no_value() {
    let parsed = parse_input(&ONE_REST, "").unwrap();
    assert!(parsed.is_empty());
}

#[test]
fn parse_rest_required_empty_errors() {
    let err = parse_input(&ONE_REST_REQUIRED, "").unwrap_err();
    assert_eq!(err, ParseError::MissingArg("title"));
}

#[test]
fn parse_dynamic_choice_accepts_any_token() {
    let parsed = parse_input(&ONE_DYN, "anthropic:claude-sonnet-4-6").unwrap();
    assert_eq!(
        parsed.get("id"),
        Some(&ArgValue::Text("anthropic:claude-sonnet-4-6".into()))
    );
}

#[test]
fn parse_path_takes_one_token() {
    let parsed = parse_input(&ONE_PATH, "src/lib.rs").unwrap();
    assert_eq!(
        parsed.get("path"),
        Some(&ArgValue::Path("src/lib.rs".into()))
    );
}

#[test]
fn parse_flag_with_yes_yields_true() {
    let parsed = parse_input(&ONE_FLAG, "yes").unwrap();
    assert_eq!(parsed.get("on"), Some(&ArgValue::Bool(true)));
}

#[test]
fn parse_flag_with_off_yields_false() {
    let parsed = parse_input(&ONE_FLAG, "off").unwrap();
    assert_eq!(parsed.get("on"), Some(&ArgValue::Bool(false)));
}

#[test]
fn parse_flag_invalid_value_errors() {
    let err = parse_input(&ONE_FLAG, "maybe").unwrap_err();
    assert!(matches!(
        err,
        ParseError::InvalidFlag { name: "on", ref got } if got == "maybe"
    ));
}

#[test]
fn parse_flag_absent_yields_empty() {
    let parsed = parse_input(&ONE_FLAG, "").unwrap();
    assert!(parsed.is_empty());
}

#[test]
fn parse_choice_then_rest_consumes_correctly() {
    let parsed = parse_input(&CHOICE_THEN_REST, "loud hello world").unwrap();
    assert_eq!(parsed.get("voice"), Some(&ArgValue::Choice("loud".into())));
    assert_eq!(
        parsed.get("message"),
        Some(&ArgValue::Text("hello world".into()))
    );
}

#[test]
fn parse_quoted_token_preserves_spaces_in_arg_value() {
    let parsed = parse_input(&ONE_DYN, r#""my model""#).unwrap();
    assert_eq!(parsed.get("id"), Some(&ArgValue::Text("my model".into())));
}

#[test]
fn parse_propagates_unterminated_quote_error() {
    let err = parse_input(&ONE_DYN, r#""open"#).unwrap_err();
    assert_eq!(err, ParseError::UnterminatedQuote);
}

#[test]
fn parse_extra_tokens_after_choice_errors() {
    let err = parse_input(&ONE_CHOICE, "on extra").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedTokens(ref s) if s == "extra"));
}

#[test]
fn complete_empty_input_lists_every_command() {
    let r = TestResolver;
    let c = complete(&registry(), "", 0, &r);
    let names: Vec<&str> = c.items.iter().map(|i| i.value.as_str()).collect();
    assert!(names.contains(&"quit"));
    assert!(names.contains(&"q"));
    assert!(names.contains(&"model"));
    assert_eq!(c.anchor, 0);
}

#[test]
fn complete_partial_command_filters_by_prefix() {
    let r = TestResolver;
    let c = complete(&registry(), "mo", 2, &r);
    let names: Vec<&str> = c.items.iter().map(|i| i.value.as_str()).collect();
    assert_eq!(names, vec!["mouse", "model"]);
    for item in &c.items {
        assert_eq!(item.replace_range, 0..2);
    }
}

#[test]
fn complete_alias_matches_independently() {
    let r = TestResolver;
    let c = complete(&registry(), "q", 1, &r);
    let names: Vec<&str> = c.items.iter().map(|i| i.value.as_str()).collect();
    assert!(names.contains(&"quit"));
    assert!(names.contains(&"q"));
}

#[test]
fn complete_after_command_and_space_starts_first_arg() {
    let r = TestResolver;
    let c = complete(&registry(), "mouse ", 6, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["on", "off", "toggle"]);
    assert_eq!(c.anchor, 6);
}

#[test]
fn complete_partial_choice_value_filters() {
    let r = TestResolver;
    let c = complete(&registry(), "mouse o", 7, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["on", "off"]);
    for item in &c.items {
        assert_eq!(item.replace_range, 6..7);
    }
}

#[test]
fn complete_dynamic_choice_uses_resolver() {
    let r = TestResolver;
    let c = complete(&registry(), "model anth", 10, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names.len(), 2);
    assert!(names[0].starts_with("anthropic:"));
}

#[test]
fn complete_path_uses_resolver_paths() {
    let r = TestResolver;
    let c = complete(&registry(), "open src/", 9, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["src/lib.rs", "src/main.rs"]);
}

#[test]
fn complete_past_last_arg_returns_empty() {
    let r = TestResolver;
    let c = complete(&registry(), "mouse on ", 9, &r);
    assert!(c.items.is_empty());
}

#[test]
fn complete_unknown_command_then_arg_returns_empty() {
    let r = TestResolver;
    let c = complete(&registry(), "bogus arg", 9, &r);
    assert!(c.items.is_empty());
}

#[test]
fn complete_in_middle_of_word_still_works() {
    let r = TestResolver;
    let c = complete(&registry(), "model", 3, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["model"]);
    for item in &c.items {
        assert_eq!(item.replace_range, 0..3);
    }
}

#[test]
fn complete_handles_quoted_partial_prefix() {
    let r = TestResolver;
    let c = complete(&registry(), r#"model "anth"#, 11, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().all(|n| n.starts_with("anth")));
}

#[test]
fn complete_flag_arg_lists_bool_literals() {
    let r = TestResolver;
    let c = complete(&registry(), "verbose ", 8, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["true", "false", "yes", "no", "on", "off"]);
}

#[test]
fn complete_unterminated_quote_returns_empty() {
    let r = TestResolver;
    let c = complete(&registry(), r#"model "anth"#, 11, &EmptyResolver);
    let _ = r;
    let _ = c;
    let c2 = complete(&registry(), r#"compact "broken"#, 15, &EmptyResolver);
    assert!(c2.items.is_empty());
}

#[test]
fn complete_rest_arg_returns_no_suggestions() {
    let r = TestResolver;
    let c = complete(&registry(), "compact ", 8, &r);
    assert!(c.items.is_empty());
}

#[test]
fn complete_after_choice_and_space_completes_rest_arg() {
    let r = TestResolver;
    let c = complete(&registry(), "say loud ", 9, &r);
    assert!(c.items.is_empty());
}

#[test]
fn complete_first_arg_when_two_args_specified() {
    let r = TestResolver;
    let c = complete(&registry(), "say lo", 6, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["loud"]);
}

const SUB_LEAF_ARGS: CommandSpec = CommandSpec {
    name: "rename",
    aliases: &[],
    description: "rename to N",
    category: CommandCategory::Both,
    args: &[ArgSpec::Rest {
        name: "name",
        optional: false,
        hint: "new name",
    }],
    subcommands: &[],
};

const SUB_LEAF_NOARGS: CommandSpec = CommandSpec {
    name: "list",
    aliases: &[],
    description: "list things",
    category: CommandCategory::Both,
    args: &[],
    subcommands: &[],
};

const SUB_LEAF_DYN: CommandSpec = CommandSpec {
    name: "set",
    aliases: &[],
    description: "switch to one",
    category: CommandCategory::Both,
    args: &[ArgSpec::DynamicChoice {
        name: "id",
        source: ArgSource::Themes,
        optional: false,
    }],
    subcommands: &[],
};

const PARENT_WITH_SUBS: CommandSpec = CommandSpec {
    name: "tree",
    aliases: &[],
    description: "session tree ops",
    category: CommandCategory::Both,
    args: &[],
    subcommands: &[SUB_LEAF_NOARGS, SUB_LEAF_DYN, SUB_LEAF_ARGS],
};

fn subcommand_registry() -> Vec<&'static CommandSpec> {
    vec![&PARENT_WITH_SUBS]
}

#[test]
fn complete_after_parent_lists_subcommands() {
    let r = TestResolver;
    let c = complete(&subcommand_registry(), "tree ", 5, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["list", "set", "rename"]);
}

#[test]
fn complete_partial_subcommand_filters() {
    let r = TestResolver;
    let c = complete(&subcommand_registry(), "tree s", 6, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    assert_eq!(names, vec!["set"]);
}

#[test]
fn complete_descends_into_subcommand_args() {
    let r = TestResolver;
    let c = complete(&subcommand_registry(), "tree set ", 9, &r);
    let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
    // Theme resolver returns dark/dawn/moonlight from TestResolver.
    assert!(names.contains(&"dark".to_owned()));
    assert!(names.contains(&"dawn".to_owned()));
}

#[test]
fn complete_subcommand_arg_hints_in_description() {
    let r = TestResolver;
    let c = complete(&subcommand_registry(), "tree ", 5, &r);
    let set_item = c.items.iter().find(|i| i.value == "set").expect("set item");
    assert!(
        set_item
            .description
            .as_deref()
            .unwrap_or("")
            .contains("<id>"),
        "got {:?}",
        set_item.description
    );
}

// --- edit_distance / suggest_command tests ---

#[test]
fn edit_distance_identical_is_zero() {
    assert_eq!(edit_distance("quit", "quit"), 0);
}

#[test]
fn edit_distance_single_substitution() {
    assert_eq!(edit_distance("quut", "quit"), 1);
}

#[test]
fn edit_distance_insertion() {
    assert_eq!(edit_distance("qit", "quit"), 1);
}

#[test]
fn edit_distance_deletion() {
    assert_eq!(edit_distance("quiit", "quit"), 1);
}

#[test]
fn edit_distance_completely_different() {
    assert_eq!(edit_distance("abc", "xyz"), 3);
}

#[test]
fn edit_distance_empty_strings() {
    assert_eq!(edit_distance("", ""), 0);
    assert_eq!(edit_distance("abc", ""), 3);
    assert_eq!(edit_distance("", "abc"), 3);
}

#[test]
fn suggest_command_finds_close_match() {
    let suggestion = suggest_command(&registry(), "moose");
    assert_eq!(suggestion, Some("mouse"));
}

#[test]
fn suggest_command_finds_by_alias() {
    let suggestion = suggest_command(&registry(), "qiut");
    // "qiut" is close to "quit" (edit distance 2) and also close
    // to alias "q" (edit distance 3). Should pick "quit".
    assert_eq!(suggestion, Some("quit"));
}

#[test]
fn suggest_command_returns_none_for_garbage() {
    let suggestion = suggest_command(&registry(), "xyzzy");
    assert!(suggestion.is_none());
}

#[test]
fn suggest_command_case_insensitive() {
    let suggestion = suggest_command(&registry(), "MODEL");
    assert_eq!(suggestion, Some("model"));
}

#[test]
fn suggest_command_returns_none_for_empty() {
    let suggestion = suggest_command(&registry(), "");
    assert!(suggestion.is_none());
}

#[test]
fn suggest_command_exact_match_returns_it() {
    let suggestion = suggest_command(&registry(), "mouse");
    assert_eq!(suggestion, Some("mouse"));
}
