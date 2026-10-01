//! The model catalog over rpc: `_kage/models/list` reports every model
//! the engine can run now, with what the catalog knows of each, so a
//! client's model picker can show context, prices and thinking levels.

use kage_acp::acp::{ModelEntry, ModelProvider, ModelsResponse};
use kage_core::{Input, Inputs, ModelCost, Reasoning};
use kage_provider::ProviderRegistry;

/// Every provider in `registry` with its models: the ones it declares,
/// else the catalog's for its id, as the TUI's model picker lists them.
pub(super) fn catalog(registry: &ProviderRegistry) -> ModelsResponse {
    let mut ids: Vec<&str> = registry.ids().collect();
    ids.sort_unstable();
    let providers = ids
        .into_iter()
        .filter_map(|id| {
            let provider = registry.get(id)?;
            let declared = provider.models();
            let (name, models) = if declared.is_empty() {
                let info = kage_provider::catalog::provider(id)?;
                let models = info
                    .models
                    .iter()
                    .map(|m| {
                        entry(
                            id,
                            m.id,
                            m.name,
                            Facts {
                                context: m.context,
                                cost: m.cost,
                                reasoning: m.reasoning,
                                input: m.input,
                                released: m.release_date,
                            },
                        )
                    })
                    .collect();
                (info.name.to_owned(), models)
            } else {
                let models = declared
                    .iter()
                    .map(|m| {
                        entry(
                            id,
                            &m.id,
                            &m.name,
                            Facts {
                                context: m.context,
                                cost: m.cost,
                                reasoning: m.reasoning,
                                input: m.input,
                                released: None,
                            },
                        )
                    })
                    .collect();
                (provider.metadata().display_name.clone(), models)
            };
            Some(ModelProvider {
                id: id.to_owned(),
                name,
                models,
            })
        })
        .collect();
    ModelsResponse { providers }
}

/// What the catalog or the provider knows of one model.
#[derive(Clone, Copy)]
struct Facts<'a> {
    context: Option<u64>,
    cost: Option<ModelCost>,
    reasoning: Reasoning,
    input: Inputs,
    released: Option<&'a str>,
}

fn entry(provider: &str, id: &str, name: &str, facts: Facts<'_>) -> ModelEntry {
    ModelEntry {
        id: kage_core::qualify_model(provider, id),
        name: name.to_owned(),
        context: facts.context,
        input_cost: facts.cost.map(|cost| cost.input),
        output_cost: facts.cost.map(|cost| cost.output),
        thinking: facts
            .reasoning
            .levels()
            .into_iter()
            .map(|level| level.as_str().to_owned())
            .collect(),
        images: facts.input.contains(Input::Image),
        released: facts.released.map(str::to_owned),
    }
}
