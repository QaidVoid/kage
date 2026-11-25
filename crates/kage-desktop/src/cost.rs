//! Shared labels for monetary amounts and their pricing quality.

use kage_client::wire::CostStatus;

use crate::store::Store;

/// Formats a session tree's single-currency amount without inventing a total.
pub(crate) fn tree_label(store: &Store, id: &str, precision: usize) -> Option<String> {
    let cost = store.tree_cost(id);
    label(
        cost.as_ref().map(|cost| cost.amount),
        cost.as_ref().map_or("", |cost| cost.currency.as_str()),
        precision,
        store.tree_cost_status(id),
    )
}

/// Qualifies an amount while keeping a known zero distinct from missing pricing.
pub(crate) fn label(
    amount: Option<f64>,
    currency: &str,
    precision: usize,
    status: CostStatus,
) -> Option<String> {
    if status == CostStatus::Incomplete && amount.is_none_or(|amount| amount == 0.0) {
        return Some("cost unknown".to_owned());
    }
    let amount = amount?;
    let space = if currency == "$" { "" } else { " " };
    let amount = format!("{currency}{space}{amount:.precision$}");
    Some(match status {
        CostStatus::Recorded => amount,
        CostStatus::Estimated => format!("~{amount}"),
        CostStatus::Incomplete => format!("{amount} (partial)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_distinguish_recorded_estimated_and_partial_amounts() {
        assert_eq!(
            label(Some(1.2), "$", 2, CostStatus::Recorded).as_deref(),
            Some("$1.20")
        );
        assert_eq!(
            label(Some(1.2), "$", 2, CostStatus::Estimated).as_deref(),
            Some("~$1.20")
        );
        assert_eq!(
            label(Some(1.2), "$", 2, CostStatus::Incomplete).as_deref(),
            Some("$1.20 (partial)")
        );
        assert_eq!(
            label(Some(1.2), "EUR", 4, CostStatus::Estimated).as_deref(),
            Some("~EUR 1.2000")
        );
    }

    #[test]
    fn unknown_pricing_is_not_a_free_recorded_amount() {
        assert_eq!(
            label(None, "$", 2, CostStatus::Incomplete).as_deref(),
            Some("cost unknown")
        );
        assert_eq!(
            label(Some(0.0), "$", 2, CostStatus::Incomplete).as_deref(),
            Some("cost unknown")
        );
        assert_eq!(
            label(Some(0.0), "$", 2, CostStatus::Recorded).as_deref(),
            Some("$0.00")
        );
        assert_eq!(
            label(Some(0.0), "$", 2, CostStatus::Estimated).as_deref(),
            Some("~$0.00")
        );
        assert_eq!(label(None, "$", 2, CostStatus::Recorded), None);
    }
}
