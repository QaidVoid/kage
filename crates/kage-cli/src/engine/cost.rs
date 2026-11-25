//! Restore billable accounting without repricing saved cost estimates.

use kage_core::protocol::Usage;
use kage_core::{CostStatus, TokenCost};
use kage_provider::ProviderRegistry;
use kage_session::ReplayResult;

/// Restore saved costs and estimate only legacy usage from its recorded model.
/// Missing historical providers or prices leave the total explicitly incomplete.
pub(crate) fn usage_from_replay(replay: &ReplayResult, registry: &ProviderRegistry) -> Usage {
    let accounting = &replay.accounting;
    let mut usage = Usage {
        total: accounting.usage,
        cost: accounting.cost,
        cost_status: accounting.cost_status,
        context_used: replay.usage_total.last_context,
        ..Usage::default()
    };
    for (model, tokens) in &accounting.legacy_usage {
        let price = registry.resolve(model).ok().and_then(|resolved| {
            kage_provider::model_cost(resolved.provider.as_ref(), &resolved.model)
        });
        if let Some(price) = price {
            let cost = TokenCost::from_usage(
                tokens,
                price.input,
                price.output,
                price.cache_read,
                price.cache_write,
            );
            if cost.total.is_finite() && cost.total >= 0.0 {
                usage.cost += cost.total;
                usage.cost_status = usage.cost_status.merge(CostStatus::Estimated);
                continue;
            }
        }
        usage.cost_status = CostStatus::Incomplete;
    }
    usage
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_core::{CancelFlag, ModelCost, TokenUsage};
    use kage_provider::{
        EventStream, Provider, ProviderError, ProviderMetadata, ProviderModel, StreamRequest,
    };
    use kage_session::{
        EntryId, FORMAT_VERSION, Header, ReplayAccounting, SessionId, SessionWriter,
    };

    use super::*;

    #[derive(Debug)]
    struct Priced {
        metadata: ProviderMetadata,
        models: Vec<ProviderModel>,
    }

    impl Provider for Priced {
        fn metadata(&self) -> &ProviderMetadata {
            &self.metadata
        }

        fn models(&self) -> Vec<ProviderModel> {
            self.models.clone()
        }

        fn stream(&self, _: StreamRequest, _: &CancelFlag) -> Result<EventStream, ProviderError> {
            unreachable!("restoring costs must not call a provider")
        }
    }

    fn registry() -> ProviderRegistry {
        let model = |id: &str, price: Option<f64>| ProviderModel {
            id: id.into(),
            name: id.into(),
            cost: price.map(|price| ModelCost {
                input: price,
                output: price * 2.0,
                cache_read: Some(price / 10.0),
                cache_write: Some(price / 5.0),
            }),
            ..ProviderModel::default()
        };
        ProviderRegistry::new().with(Arc::new(Priced {
            metadata: ProviderMetadata {
                id: "priced".into(),
                display_name: "Priced".into(),
                supports_caching: true,
                supports_thinking: false,
                supports_tool_use: false,
            },
            models: vec![
                model("a", Some(1.0)),
                model("b", Some(10.0)),
                model("free", Some(0.0)),
                model("unknown", None),
            ],
        }))
    }

    fn replay(accounting: ReplayAccounting) -> ReplayResult {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = SessionWriter::create(
            path.clone(),
            Header {
                version: FORMAT_VERSION,
                session: SessionId::new(),
                id: EntryId::new(),
                ts: chrono::Utc::now(),
                cwd: dir.path().to_path_buf(),
                model: "missing/fallback".into(),
                system_prompt: String::new(),
                backend: None,
                parent_session: None,
                parent_entry: None,
            },
        )
        .unwrap();
        drop(writer);
        let mut replay = kage_session::replay(&path).unwrap();
        replay.accounting = accounting;
        replay.usage_total.last_context = 57;
        replay
    }

    #[test]
    fn recorded_costs_survive_missing_or_replaced_pricing() {
        let replay = replay(ReplayAccounting {
            usage: TokenUsage {
                input: 100,
                ..TokenUsage::default()
            },
            cost: 1.25,
            ..ReplayAccounting::default()
        });
        for registry in [registry(), ProviderRegistry::new()] {
            let usage = usage_from_replay(&replay, &registry);
            assert!((usage.cost - 1.25).abs() < 1e-12);
            assert_eq!(usage.cost_status, CostStatus::Recorded);
            assert_eq!(usage.total.input, 100);
            assert_eq!(usage.context_used, 57);
        }
    }

    #[test]
    fn legacy_usage_is_estimated_per_historical_model_and_token_category() {
        let tokens = TokenUsage {
            input: 100,
            output: 20,
            cache_read: 50,
            cache_write: 10,
        };
        let replay = replay(ReplayAccounting {
            cost: 1.0,
            legacy_usage: [("priced:a".into(), tokens), ("priced/b".into(), tokens)].into(),
            ..ReplayAccounting::default()
        });
        let usage = usage_from_replay(&replay, &registry());
        assert!((usage.cost - 1.001_617).abs() < 1e-12);
        assert_eq!(usage.cost_status, CostStatus::Estimated);
    }

    #[test]
    fn explicitly_unknown_snapshots_are_not_repriced() {
        let replay = replay(ReplayAccounting {
            cost: 0.5,
            cost_status: CostStatus::Incomplete,
            usage: TokenUsage {
                input: 100,
                ..TokenUsage::default()
            },
            ..ReplayAccounting::default()
        });
        let usage = usage_from_replay(&replay, &registry());
        assert!((usage.cost - 0.5).abs() < 1e-12);
        assert_eq!(usage.cost_status, CostStatus::Incomplete);
    }

    #[test]
    fn missing_or_unpriced_legacy_models_do_not_use_the_final_models_rate() {
        for model in [
            "missing/a",
            "priced/unlisted",
            "priced/unknown",
            "unqualified",
        ] {
            let replay = replay(ReplayAccounting {
                cost: 0.5,
                legacy_usage: [(
                    model.into(),
                    TokenUsage {
                        input: 100,
                        ..TokenUsage::default()
                    },
                )]
                .into(),
                ..ReplayAccounting::default()
            });
            let usage = usage_from_replay(&replay, &registry());
            assert!((usage.cost - 0.5).abs() < 1e-12, "{model}");
            assert_eq!(usage.cost_status, CostStatus::Incomplete, "{model}");
        }
    }

    #[test]
    fn explicitly_free_legacy_models_are_known_zero_estimates() {
        let replay = replay(ReplayAccounting {
            legacy_usage: [(
                "priced/free".into(),
                TokenUsage {
                    input: 100,
                    ..TokenUsage::default()
                },
            )]
            .into(),
            ..ReplayAccounting::default()
        });
        let usage = usage_from_replay(&replay, &registry());
        assert!(usage.cost.abs() < f64::EPSILON);
        assert_eq!(usage.cost_status, CostStatus::Estimated);
    }
}
