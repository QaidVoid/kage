//! The engine option registry over rpc: `_kage/options/list` reads
//! every option with its value in effect, and `_kage/options/set`
//! validates one and writes it into the user `config.toml`, keeping
//! the file's comments and layout.

use kage_acp::acp::{OptionEntry, OptionSetRequest, OptionsResponse};
use kage_core::config::Config;
use kage_core::options::{
    OPTIONS, OptionKind, OptionSource, OptionStore, lookup, option_from_json, option_to_json,
    save_options,
};
use kage_jsonrpc::RpcError;

/// Every option with the value `config` puts in effect.
pub(super) fn entries(config: &Config) -> OptionsResponse {
    let (store, _) = OptionStore::from_config(config);
    let options = OPTIONS
        .iter()
        .map(|def| {
            let (kind, min, max, values) = match def.kind {
                OptionKind::Bool { .. } => ("bool", None, None, Vec::new()),
                OptionKind::Int { min, max, .. } => ("int", Some(min), Some(max), Vec::new()),
                OptionKind::Fraction { .. } => ("fraction", None, None, Vec::new()),
                OptionKind::Choice { values, .. } => (
                    "choice",
                    None,
                    None,
                    values.iter().map(|v| (*v).to_owned()).collect(),
                ),
                OptionKind::Str { .. } => ("str", None, None, Vec::new()),
                OptionKind::Key { .. } => ("key", None, None, Vec::new()),
            };
            OptionEntry {
                name: def.name.to_owned(),
                toml: def.toml.to_owned(),
                doc: def.doc.to_owned(),
                kind: kind.to_owned(),
                min,
                max,
                values,
                default: option_to_json(&def.default_value()),
                value: store
                    .get(def.name)
                    .map_or(serde_json::Value::Null, option_to_json),
                configured: store.source(def.name) == Some(OptionSource::Toml),
                live: def.live,
            }
        })
        .collect();
    OptionsResponse { options }
}

/// Validates `req` and writes it into the user config at `path`.
pub(super) fn set(path: &std::path::Path, req: &OptionSetRequest) -> Result<(), RpcError> {
    let invalid = |message: String| RpcError::new(-32602, message);
    let def = lookup(&req.name).map_err(|e| invalid(e.to_string()))?;
    let value = option_from_json(&req.value)
        .ok_or_else(|| invalid(format!("{} takes {}", def.name, def.expected())))?;
    let value = def.validate(value).map_err(|e| invalid(e.to_string()))?;
    save_options(path, &[(def, value)]).map_err(|e| RpcError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{entries, set};
    use kage_acp::acp::OptionSetRequest;
    use kage_core::config::Config;

    #[test]
    fn a_set_writes_the_user_config_and_keeps_its_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[agents]\nmax_depth = 1 # keep\n").unwrap();
        let req = OptionSetRequest {
            name: "agent_max_depth".into(),
            value: serde_json::json!(2),
        };
        set(&path, &req).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# mine") && text.contains("max_depth = 2 # keep"),
            "{text}"
        );

        let config = Config::load(&path).unwrap();
        let listed = entries(&config);
        let depth = listed
            .options
            .iter()
            .find(|o| o.name == "agent_max_depth")
            .unwrap();
        assert_eq!(depth.value, serde_json::json!(2));
        assert!(depth.configured);
        assert_eq!((depth.min, depth.max), (Some(0), Some(3)));
    }

    #[test]
    fn a_value_out_of_bounds_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let req = OptionSetRequest {
            name: "agent_max_depth".into(),
            value: serde_json::json!(9),
        };
        let err = set(&path, &req).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(!path.exists());
        let unknown = OptionSetRequest {
            name: "nope".into(),
            value: serde_json::json!(true),
        };
        assert_eq!(set(&path, &unknown).unwrap_err().code, -32602);
    }
}
