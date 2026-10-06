//! Shared parsing for Lua `add` specs that name a command launch:
//! `kage.mcp.add_server` and `kage.acp.add_agent` accept the same
//! `{ name, command, args, env }` shape and must reject bad input with
//! the same wording.

use std::collections::BTreeMap;

use mlua::{Table, Value};

/// A parsed launch spec.
#[derive(Debug)]
pub struct Launch {
    /// Registry key for the server or agent.
    pub name: String,
    /// Executable the host spawns.
    pub command: String,
    /// Positional arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
}

/// Parses the common fields of an add spec, with errors prefixed by
/// `prefix` (for example `kage.mcp.add_server`).
///
/// # Errors
///
/// When `name` or `command` is missing or empty, `args` is present but
/// not a string array, or `env` is present but not a string map.
pub fn parse_launch(prefix: &str, spec: &Table) -> mlua::Result<Launch> {
    let name: String = spec.get("name")?;
    let command: String = spec.get("command")?;
    if name.is_empty() || command.is_empty() {
        return Err(mlua::Error::external(format!(
            "{prefix}: `name` and `command` are required"
        )));
    }
    let args: Vec<String> = match spec.get::<Value>("args")? {
        Value::Nil => Vec::new(),
        Value::Table(t) => t
            .sequence_values::<String>()
            .collect::<Result<_, _>>()
            .map_err(|_| {
                mlua::Error::external(format!("{prefix}: `args` must be a string array"))
            })?,
        _ => {
            return Err(mlua::Error::external(format!(
                "{prefix}: `args` must be a string array"
            )));
        }
    };
    let mut env = BTreeMap::new();
    if let Value::Table(t) = spec.get::<Value>("env")? {
        for pair in t.pairs::<String, String>() {
            let (k, v) = pair.map_err(|_| {
                mlua::Error::external(format!("{prefix}: `env` must be a string map"))
            })?;
            env.insert(k, v);
        }
    }
    Ok(Launch {
        name,
        command,
        args,
        env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    #[test]
    fn parse_rejects_bad_args_and_env_with_the_prefix() {
        let lua = lua();
        let table: Table = lua
            .load("{ name = 'x', command = 'y', args = 'nope' }")
            .eval()
            .unwrap();
        let err = parse_launch("kage.mcp.add_server", &table)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("kage.mcp.add_server: `args` must be a string array"),
            "{err}"
        );

        let table: Table = lua
            .load("{ name = 'x', command = 'y', env = { K = {} } }")
            .eval()
            .unwrap();
        let err = parse_launch("kage.acp.add_agent", &table)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("kage.acp.add_agent: `env` must be a string map"),
            "{err}"
        );
    }

    #[test]
    fn parse_rejects_an_empty_name_or_command() {
        let lua = lua();
        let table: Table = lua.load("{ name = '', command = 'y' }").eval().unwrap();
        let err = parse_launch("kage.mcp.add_server", &table)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("kage.mcp.add_server: `name` and `command` are required"),
            "{err}"
        );
    }

    #[test]
    fn parse_defaults_missing_args_and_env() {
        let lua = lua();
        let table: Table = lua.load("{ name = 'x', command = 'y' }").eval().unwrap();
        let launch = parse_launch("kage.acp.add_agent", &table).unwrap();
        assert!(launch.args.is_empty());
        assert!(launch.env.is_empty());
    }
}
