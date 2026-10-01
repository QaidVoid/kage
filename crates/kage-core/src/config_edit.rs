//! Editing one entry of a `config.toml` in place from JSON, so a
//! settings client can replace or remove a table such as
//! `[mcp.servers.github]` without touching the comments and layout
//! around it.
//!
//! [`edited`] only renders the new text: the caller validates it with
//! [`parse`] before it writes anything.

use std::path::Path;

use figment::Figment;
use figment::providers::{Format, Serialized, Toml};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::config::Config;
use crate::error::{Error, Result};

/// The text of the TOML file at `path` with the entry at `keys`
/// replaced by `value`, or removed when `value` is `None`. Missing
/// tables on the way are created; a missing file reads as empty. A
/// replaced table keeps the comments above it. Nothing is written.
///
/// # Errors
///
/// The file cannot be read or parsed, `keys` is empty, a key on the
/// way is not a table, or `value` holds something TOML cannot (a
/// `null` inside an array).
pub fn edited(path: &Path, keys: &[&str], value: Option<&serde_json::Value>) -> Result<String> {
    let mut doc = read(path)?;
    let Some((last, parents)) = keys.split_last() else {
        return Err(Error::ConfigWrite("no key to edit".to_owned()));
    };
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    let mut inline = false;
    for (depth, key) in parents.iter().enumerate() {
        if value.is_none() && table.get(key).is_none() {
            return Ok(doc.to_string());
        }
        let item = table.entry(key).or_insert_with(|| {
            if inline {
                Item::Value(Value::InlineTable(InlineTable::new()))
            } else {
                let mut new = Table::new();
                new.set_implicit(true);
                Item::Table(new)
            }
        });
        inline = item.is_inline_table();
        table = item.as_table_like_mut().ok_or_else(|| {
            Error::ConfigWrite(format!("`{}` is not a table", parents[..=depth].join(".")))
        })?;
    }
    let Some(value) = value else {
        table.remove(last);
        return Ok(doc.to_string());
    };
    match table.get_mut(last) {
        Some(Item::Table(old)) if value.is_object() => merge(old, value)?,
        Some(Item::Value(old)) => {
            let mut new = value_like(value, Some(old))?;
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        _ => {
            // Inside an inline table the new entry stays inline too.
            let item = if inline {
                Item::Value(to_value(value)?)
            } else {
                to_item(value)?
            };
            table.insert(last, item);
        }
    }
    Ok(doc.to_string())
}

/// Makes `old` hold the object `new` while keeping its key order, the
/// comments on its keys and its own header comments: a key `new` lacks
/// goes, a key both hold changes in place, a key written inline stays
/// inline, and a new key goes last.
fn merge(old: &mut Table, new: &serde_json::Value) -> Result<()> {
    let Some(new) = new.as_object() else {
        return Ok(());
    };
    let gone: Vec<String> = old
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| new.get(key).is_none_or(serde_json::Value::is_null))
        .collect();
    for key in gone {
        old.remove(&key);
    }
    for (key, value) in new {
        if value.is_null() {
            continue;
        }
        match old.get_mut(key) {
            Some(Item::Table(table)) if value.is_object() => merge(table, value)?,
            Some(Item::Value(held)) => {
                let mut fresh = value_like(value, Some(held))?;
                *fresh.decor_mut() = held.decor().clone();
                *held = fresh;
            }
            Some(Item::ArrayOfTables(tables)) if value.is_array() => {
                let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
                if !items.iter().all(serde_json::Value::is_object) {
                    old.insert(key, to_item(value)?);
                    continue;
                }
                while tables.len() > items.len() {
                    tables.remove(tables.len() - 1);
                }
                for (ix, item) in items.iter().enumerate() {
                    match tables.get_mut(ix) {
                        Some(table) => merge(table, item)?,
                        None => {
                            if let Item::Table(table) = to_item(item)? {
                                tables.push(table);
                            }
                        }
                    }
                }
            }
            _ => {
                old.insert(key, to_item(value)?);
            }
        }
    }
    Ok(())
}

/// `value` as an inline TOML value shaped like `old`: an inline table
/// keeps the keys `old` holds in their order and their spacing, and an
/// array follows `old` element by element.
fn value_like(value: &serde_json::Value, old: Option<&Value>) -> Result<Value> {
    match (value, old) {
        (serde_json::Value::Object(map), Some(Value::InlineTable(held))) => {
            let mut table = InlineTable::new();
            for (key, old) in held {
                if let Some(value) = map.get(key).filter(|value| !value.is_null()) {
                    let mut fresh = value_like(value, Some(old))?;
                    *fresh.decor_mut() = old.decor().clone();
                    table.insert(key, fresh);
                }
            }
            for (key, value) in map {
                if !value.is_null() && !held.contains_key(key) {
                    table.insert(key, to_value(value)?);
                }
            }
            *table.decor_mut() = held.decor().clone();
            Ok(Value::InlineTable(table))
        }
        (serde_json::Value::Array(items), Some(Value::Array(held))) => {
            let mut array = Array::new();
            for (ix, item) in items.iter().enumerate() {
                match held.get(ix) {
                    Some(old) => {
                        let mut fresh = value_like(item, Some(old))?;
                        *fresh.decor_mut() = old.decor().clone();
                        array.push_formatted(fresh);
                    }
                    None => array.push(to_value(item)?),
                }
            }
            Ok(Value::Array(array))
        }
        (value, _) => to_value(value),
    }
}

/// The entry at `keys` in the TOML file at `path`, as JSON. `None` when
/// the file or the entry is missing.
///
/// # Errors
///
/// The file cannot be read or parsed.
pub fn current(path: &Path, keys: &[&str]) -> Result<Option<serde_json::Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let root: toml::Table = toml::from_str(&text).map_err(|e| Error::ConfigWrite(e.to_string()))?;
    let mut at = &toml::Value::Table(root);
    for key in keys {
        match at.get(key) {
            Some(next) => at = next,
            None => return Ok(None),
        }
    }
    serde_json::to_value(at)
        .map(Some)
        .map_err(|e| Error::ConfigWrite(e.to_string()))
}

/// Parses config text the way [`Config::load`] reads a file, without
/// environment overrides.
///
/// # Errors
///
/// The text is not TOML or does not fit the config's shape.
pub fn parse(text: &str) -> Result<Config> {
    Ok(Figment::new()
        .merge(Serialized::defaults(Config::default()))
        .merge(Toml::string(text))
        .extract()?)
}

fn read(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .map_err(|e| Error::ConfigWrite(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e.into()),
    }
}

/// `value` as a TOML item: an object becomes a `[table]`, an array of
/// objects an `[[array of tables]]`, anything else a value.
fn to_item(value: &serde_json::Value) -> Result<Item> {
    match value {
        serde_json::Value::Object(map) => {
            let mut table = Table::new();
            for (key, value) in map {
                if !value.is_null() {
                    table.insert(key, to_item(value)?);
                }
            }
            Ok(Item::Table(table))
        }
        serde_json::Value::Array(items)
            if !items.is_empty() && items.iter().all(serde_json::Value::is_object) =>
        {
            let mut array = ArrayOfTables::new();
            for item in items {
                if let Item::Table(table) = to_item(item)? {
                    array.push(table);
                }
            }
            Ok(Item::ArrayOfTables(array))
        }
        value => Ok(Item::Value(to_value(value)?)),
    }
}

/// `value` as an inline TOML value.
fn to_value(value: &serde_json::Value) -> Result<Value> {
    Ok(match value {
        serde_json::Value::Null => {
            return Err(Error::ConfigWrite("TOML has no null".to_owned()));
        }
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(n) => Value::from(n),
            None => Value::from(n.as_f64().unwrap_or_default()),
        },
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(to_value(item)?);
            }
            Value::Array(array)
        }
        serde_json::Value::Object(map) => {
            let mut table = InlineTable::new();
            for (key, value) in map {
                if !value.is_null() {
                    table.insert(key, to_value(value)?);
                }
            }
            Value::InlineTable(table)
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{current, edited, parse};

    fn file(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn a_table_is_replaced_and_the_rest_keeps_its_comments() {
        let (_dir, path) = file(
            "# mine\n[ui]\ntheme = \"kage-dawn\" # keep\n\n# the hub\n[mcp.servers.hub]\ncommand = \"old\"\n",
        );
        let value = json!({ "command": "npx", "args": ["-y", "hub"], "env": { "TOKEN": "t" } });
        let text = edited(&path, &["mcp", "servers", "hub"], Some(&value)).unwrap();
        assert!(
            text.contains("# mine") && text.contains("theme = \"kage-dawn\" # keep"),
            "{text}"
        );
        assert!(text.contains("# the hub\n[mcp.servers.hub]"), "{text}");
        assert!(!text.contains("old"), "{text}");
        let config = parse(&text).unwrap();
        let hub = &config.mcp.servers["hub"];
        assert_eq!(hub.command.as_deref(), Some("npx"));
        assert_eq!(hub.args, ["-y", "hub"]);
        assert_eq!(hub.env["TOKEN"], "t");
    }

    #[test]
    fn a_replaced_table_keeps_its_key_order_and_comments() {
        let (_dir, path) = file(
            "[permissions.tools.shell]\ndefault = \"ask\" # careful\nallow = [\"ls\"]\nold = 1\n",
        );
        let value = json!({ "default": "deny", "allow": ["ls", "pwd"], "deny": ["rm *"] });
        let text = edited(&path, &["permissions", "tools", "shell"], Some(&value)).unwrap();
        assert_eq!(
            text,
            "[permissions.tools.shell]\ndefault = \"deny\" # careful\nallow = [\"ls\", \"pwd\"]\ndeny = [\"rm *\"]\n"
        );
    }

    #[test]
    fn an_inline_key_of_a_replaced_table_stays_inline() {
        let (_dir, path) = file(
            "[providers.custom.lab]\nbase_url = \"http://lab\"\nheaders = { X = \"1\" }\nmodels = [{ id = \"a\", name = \"A\" }]\n",
        );
        let value = json!({
            "base_url": "http://lab/v2",
            "headers": { "X": "2" },
            "models": [{ "id": "a", "name": "A" }, { "id": "b", "name": "B" }],
        });
        let text = edited(&path, &["providers", "custom", "lab"], Some(&value)).unwrap();
        assert!(text.contains("headers = { X = \"2\" }"), "{text}");
        assert!(
            text.contains("models = [{ id = \"a\", name = \"A\" }, { id = \"b\", name = \"B\" }]"),
            "{text}"
        );
        assert_eq!(
            parse(&text).unwrap().providers.custom["lab"].models.len(),
            2
        );
    }

    #[test]
    fn an_inline_model_list_keeps_its_key_order() {
        let (_dir, path) = file(
            "[providers.custom.lab]\nbase_url = \"http://l\"\nmodels = [{ id = \"tiny\", name = \"Lab Tiny\", context = 32000 }]\n",
        );
        let value = json!({
            "base_url": "http://l",
            "models": [
                { "context": 64_000, "id": "tiny", "name": "Lab Tiny" },
                { "id": "b", "name": "B" },
            ],
        });
        let text = edited(&path, &["providers", "custom", "lab"], Some(&value)).unwrap();
        assert!(
            text.contains(
                "models = [{ id = \"tiny\", name = \"Lab Tiny\", context = 64000 }, { id = \"b\", name = \"B\" }]"
            ),
            "{text}"
        );
    }

    #[test]
    fn models_become_an_array_of_tables() {
        let (_dir, path) = file("");
        let value = json!({
            "base_url": "http://lab:8080/v1",
            "models": [{ "id": "m1", "name": "One", "context": 128_000 }],
        });
        let text = edited(&path, &["providers", "custom", "lab"], Some(&value)).unwrap();
        assert!(text.contains("[[providers.custom.lab.models]]"), "{text}");
        let config = parse(&text).unwrap();
        let lab = &config.providers.custom["lab"];
        assert_eq!(lab.models[0].context, Some(128_000));
    }

    #[test]
    fn a_removed_entry_leaves_its_neighbours() {
        let (_dir, path) = file(
            "[permissions.tools.shell]\ndefault = \"ask\"\n\n[permissions.tools.write]\ndefault = \"allow\"\n",
        );
        let text = edited(&path, &["permissions", "tools", "shell"], None).unwrap();
        assert!(
            !text.contains("shell") && text.contains("[permissions.tools.write]"),
            "{text}"
        );
        let untouched = edited(&path, &["mcp", "servers", "none"], None).unwrap();
        assert!(!untouched.contains("mcp"), "{untouched}");
    }

    #[test]
    fn an_inline_entry_stays_inline() {
        let (_dir, path) = file("[plugins]\ncapabilities = { tokps = [\"session_write\"] }\n");
        let text = edited(
            &path,
            &["plugins", "capabilities", "clock"],
            Some(&json!(["net"])),
        )
        .unwrap();
        assert!(
            text.contains("clock = [\"net\"]") && text.contains("tokps"),
            "{text}"
        );
    }

    #[test]
    fn current_reads_the_entry_as_json() {
        let (_dir, path) = file(
            "[mcp.servers.hub]\nurl = \"https://hub\"\nheaders = { Authorization = \"Bearer x\" }\n",
        );
        let hub = current(&path, &["mcp", "servers", "hub"]).unwrap().unwrap();
        assert_eq!(hub["headers"]["Authorization"], "Bearer x");
        assert_eq!(current(&path, &["mcp", "servers", "none"]).unwrap(), None);
    }

    #[test]
    fn a_value_toml_cannot_hold_is_refused() {
        let (_dir, path) = file("");
        assert!(edited(&path, &["plugins", "enabled"], Some(&json!([null]))).is_err());
        assert!(edited(&path, &[], Some(&json!(1))).is_err());
        assert!(parse("[mcp.servers.hub]\nargs = 3\n").is_err());
    }
}
