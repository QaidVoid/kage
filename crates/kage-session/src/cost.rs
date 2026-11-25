//! Persisted dollar estimates attached to individual charged attempts.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::{Custom, EntryId, SessionEntry};

pub(crate) const USAGE_COST_KIND: &str = "kage:usage_cost";

/// The dollar estimate recorded for one accepted or rejected attempt.
///
/// Store this annotation immediately before its target message or rejected
/// usage entry. It is not itself a charge. A missing annotation identifies
/// legacy usage; a present annotation with `amount: None` records an unknown
/// price that must not be recalculated when the session is reopened.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageCost {
    /// Entry carrying the accepted message or rejected usage.
    pub entry: EntryId,
    /// Model used for the attempt, independent of later model changes.
    pub model: String,
    /// Saved USD estimate, including zero for known free usage, or unknown.
    pub amount: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageCostData {
    entry: EntryId,
    model: String,
    amount: Value,
}

impl UsageCost {
    /// Wrap this estimate in an appendable custom annotation.
    ///
    /// Returns an error if a known amount is negative or nonfinite.
    pub fn into_entry(
        self,
        id: EntryId,
        ts: DateTime<Utc>,
    ) -> Result<SessionEntry, serde_json::Error> {
        if self
            .amount
            .is_some_and(|amount| !amount.is_finite() || amount < 0.0)
        {
            return Err(serde::de::Error::custom("invalid usage cost amount"));
        }
        Ok(SessionEntry::Custom(Custom {
            id,
            ts,
            kind: USAGE_COST_KIND.to_owned(),
            data: serde_json::json!({
                "entry": self.entry,
                "model": self.model,
                "amount": self.amount,
            }),
        }))
    }

    /// Decode a cost annotation, returning `None` for other custom kinds.
    ///
    /// Malformed payloads and negative or nonfinite known amounts are errors.
    pub fn from_custom(custom: &Custom) -> Result<Option<Self>, serde_json::Error> {
        if custom.kind != USAGE_COST_KIND {
            return Ok(None);
        }
        let data: UsageCostData = serde_json::from_value(custom.data.clone())?;
        let amount = match data.amount {
            Value::Null => None,
            Value::Number(number) => match number.as_f64() {
                Some(amount) if amount.is_finite() && amount >= 0.0 => Some(amount),
                _ => return Err(serde::de::Error::custom("invalid usage cost amount")),
            },
            _ => return Err(serde::de::Error::custom("invalid usage cost amount")),
        };
        Ok(Some(Self {
            entry: data.entry,
            model: data.model,
            amount,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[test]
    fn annotation_round_trips_and_legacy_custom_readers_ignore_it() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyCustom {
            id: EntryId,
            ts: DateTime<Utc>,
            kind: String,
            data: Value,
        }
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum LegacyEntry {
            Custom(LegacyCustom),
        }
        for amount in [None, Some(0.0), Some(0.125)] {
            let cost = UsageCost {
                entry: EntryId::new(),
                model: "anthropic/claude".into(),
                amount,
            };
            let entry = cost.clone().into_entry(EntryId::new(), Utc::now()).unwrap();
            let line = serde_json::to_string(&entry).unwrap();
            let SessionEntry::Custom(custom) = serde_json::from_str(&line).unwrap() else {
                panic!("not custom");
            };
            assert_eq!(UsageCost::from_custom(&custom).unwrap(), Some(cost));
            let LegacyEntry::Custom(legacy) = serde_json::from_str(&line).unwrap();
            assert_eq!(legacy.id, custom.id);
            assert_eq!(legacy.ts, custom.ts);
            assert_eq!(legacy.kind, USAGE_COST_KIND);
            assert_eq!(legacy.data, custom.data);
        }
    }

    #[test]
    fn annotation_rejects_invalid_payloads_on_read_and_write() {
        let entry = EntryId::new();
        for data in [
            serde_json::json!({"entry": entry, "model": "m", "amount": -0.1}),
            serde_json::json!({"entry": entry, "model": "m", "amount": "1"}),
            serde_json::json!({"entry": entry, "model": "m", "amount": true}),
            serde_json::json!({"entry": entry, "model": "m", "amount": {}}),
            serde_json::json!({"entry": entry, "model": "m"}),
            serde_json::json!({"entry": entry, "amount": 0}),
            serde_json::json!({"model": "m", "amount": 0}),
            serde_json::json!({"entry": entry, "model": 1, "amount": 0}),
            serde_json::json!({"entry": "bad", "model": "m", "amount": 0}),
            serde_json::json!({"entry": entry, "model": "m", "amount": 0, "other": 1}),
            serde_json::json!([]),
        ] {
            let custom = Custom {
                id: EntryId::new(),
                ts: Utc::now(),
                kind: USAGE_COST_KIND.into(),
                data,
            };
            assert!(UsageCost::from_custom(&custom).is_err(), "{custom:?}");
            assert!(serde_json::to_string(&custom).is_err(), "{custom:?}");
            let raw = serde_json::json!({"id": custom.id, "ts": custom.ts, "kind": custom.kind, "data": custom.data});
            assert!(serde_json::from_value::<Custom>(raw).is_err());
        }
    }

    #[test]
    fn annotation_rejects_duplicate_payload_keys() {
        for field in ["entry", "model", "amount"] {
            let entry = EntryId::new();
            let value = match field {
                "entry" => format!("\"{entry}\""),
                "model" => "\"m\"".into(),
                _ => "0".into(),
            };
            let raw = format!(
                r#"{{"id":"{}","ts":"{}","kind":"kage:usage_cost","data":{{"entry":"{entry}","model":"m","amount":null,"{field}":{value}}}}}"#,
                EntryId::new(),
                Utc::now().to_rfc3339(),
            );
            assert!(serde_json::from_str::<Custom>(&raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn unrelated_custom_payloads_are_not_annotations() {
        let custom = Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: "plugin:example".into(),
            data: Value::Null,
        };
        assert_eq!(UsageCost::from_custom(&custom).unwrap(), None);
    }

    #[test]
    fn nonfinite_amounts_cannot_be_encoded_as_unknown() {
        for amount in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let cost = UsageCost {
                entry: EntryId::new(),
                model: "m".into(),
                amount: Some(amount),
            };
            assert!(cost.into_entry(EntryId::new(), Utc::now()).is_err());
        }
    }
}
