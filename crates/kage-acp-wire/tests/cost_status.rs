//! Pricing quality metadata stays optional and independent of the currency.

use kage_acp_wire::{CostStatus, SubagentUsage, UsageTokens, UsageUpdate};
use serde_json::{Value, json};

#[test]
fn cost_status_merges_toward_the_least_complete_quality() {
    let statuses = [
        CostStatus::Recorded,
        CostStatus::Estimated,
        CostStatus::Incomplete,
    ];
    assert_eq!(CostStatus::default(), CostStatus::Recorded);
    for (index, status) in statuses.into_iter().enumerate() {
        assert_eq!(status.is_recorded(), index == 0);
        for (other_index, other) in statuses.into_iter().enumerate() {
            assert_eq!(status.merge(other), statuses[index.max(other_index)]);
        }
    }
}

#[test]
fn cost_status_uses_snake_case_names() {
    for (status, name) in [
        (CostStatus::Recorded, "recorded"),
        (CostStatus::Estimated, "estimated"),
        (CostStatus::Incomplete, "incomplete"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(name));
        assert_eq!(
            serde_json::from_value::<CostStatus>(json!(name)).unwrap(),
            status
        );
    }
}

#[test]
fn legacy_token_totals_default_to_recorded_and_serialize_unchanged() {
    fn assert_copy_eq<T: Copy + Eq>() {}
    assert_copy_eq::<UsageTokens>();
    let legacy = json!({"input": 3, "output": 2, "cacheRead": 7, "cacheWrite": 1});
    let tokens: UsageTokens = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(tokens.cost_status, CostStatus::Recorded);
    assert_eq!(serde_json::to_value(tokens).unwrap(), legacy);
}

#[test]
fn session_metadata_distinguishes_known_free_and_unpriced_usage() {
    for (status, encoded_status) in [
        (CostStatus::Recorded, None),
        (CostStatus::Estimated, Some("estimated")),
        (CostStatus::Incomplete, Some("incomplete")),
    ] {
        let mut value = json!({
            "used": 3,
            "size": 100,
            "cost": {"amount": 0.0, "currency": "USD"},
            "_meta": {"kage": {"input": 3, "output": 0, "cacheRead": 0, "cacheWrite": 0}}
        });
        if let Some(encoded_status) = encoded_status {
            value["_meta"]["kage"]["costStatus"] = json!(encoded_status);
        }
        let usage: UsageUpdate = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            usage.cost.as_ref().unwrap().amount.to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(
            usage.meta.as_ref().unwrap().kage.unwrap().cost_status,
            status
        );
        assert_eq!(serde_json::to_value(usage).unwrap(), value);
    }
}

#[test]
fn external_currency_usage_without_metadata_is_unchanged() {
    for currency in ["USD", "EUR", "JPY"] {
        let value = json!({"used": 4, "size": 100, "cost": {"amount": 1.5, "currency": currency}});
        let usage: UsageUpdate = serde_json::from_value(value.clone()).unwrap();
        assert!(usage.meta.is_none());
        assert_eq!(serde_json::to_value(usage).unwrap(), value);
    }
}

#[test]
fn child_cost_status_defaults_and_roundtrips() {
    let legacy = json!({"input": 3, "output": 2, "cacheRead": 0, "cacheWrite": 0, "cost": 0.0});
    let usage: SubagentUsage = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(usage.cost_status, CostStatus::Recorded);
    assert_eq!(serde_json::to_value(&usage).unwrap(), legacy);
    let minimal: SubagentUsage = serde_json::from_value(json!({"input": 3, "output": 2})).unwrap();
    assert_eq!(minimal.cost_status, CostStatus::Recorded);
    for status in [CostStatus::Estimated, CostStatus::Incomplete] {
        let usage = SubagentUsage {
            cost_status: status,
            ..usage.clone()
        };
        let value = serde_json::to_value(&usage).unwrap();
        assert_ne!(value["costStatus"], Value::Null);
        assert_eq!(
            serde_json::from_value::<SubagentUsage>(value).unwrap(),
            usage
        );
    }
}

#[test]
fn unknown_future_cost_status_remains_incomplete() {
    assert_eq!(
        serde_json::from_str::<CostStatus>("\"future_status\"").unwrap(),
        CostStatus::Incomplete
    );
}
