//! Completeness and provenance of accumulated cost totals.

use serde::{Deserialize, Serialize};

/// How reliably a cost total accounts for its usage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostStatus {
    /// All usage has a recorded cost.
    #[default]
    Recorded,
    /// All usage is priced, but some cost was estimated.
    Estimated,
    /// Some usage has no known cost, or the pricing status is unrecognized.
    #[serde(other)]
    Incomplete,
}

impl CostStatus {
    /// Combine totals by keeping the least reliable status.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        self.max(other)
    }

    /// Whether all costs were recorded rather than estimated or missing.
    #[must_use]
    pub fn is_recorded(&self) -> bool {
        *self == Self::Recorded
    }
}

#[cfg(test)]
mod tests {
    use super::CostStatus;

    #[test]
    fn unrecognized_status_is_incomplete() {
        assert_eq!(
            serde_json::from_str::<CostStatus>("\"future_status\"").unwrap(),
            CostStatus::Incomplete
        );
    }

    #[test]
    fn recorded_is_the_default() {
        assert_eq!(CostStatus::default(), CostStatus::Recorded);
        assert!(CostStatus::Recorded.is_recorded());
        assert!(!CostStatus::Estimated.is_recorded());
        assert!(!CostStatus::Incomplete.is_recorded());
    }

    #[test]
    fn merging_keeps_the_worst_status() {
        let statuses = [
            CostStatus::Recorded,
            CostStatus::Estimated,
            CostStatus::Incomplete,
        ];
        for (left_index, left) in statuses.into_iter().enumerate() {
            for (right_index, right) in statuses.into_iter().enumerate() {
                let expected = statuses[left_index.max(right_index)];
                assert_eq!(left.merge(right), expected);
                assert_eq!(right.merge(left), expected);
            }
        }
    }

    #[test]
    fn statuses_serialize_as_snake_case() {
        for (status, name) in [
            (CostStatus::Recorded, "recorded"),
            (CostStatus::Estimated, "estimated"),
            (CostStatus::Incomplete, "incomplete"),
        ] {
            let value = serde_json::to_value(status).unwrap();
            assert_eq!(value, name);
            assert_eq!(serde_json::from_value::<CostStatus>(value).unwrap(), status);
        }
    }
}
