//! Shared request-header assembly for the built-in providers.

use std::collections::BTreeMap;

/// Headers every provider request carries: the protocol's `fixed`
/// headers, the credential header when a key is configured (`None`
/// for local endpoints with an empty key), then the configured
/// extras in key order.
pub(crate) fn request_headers(
    fixed: &[(&'static str, String)],
    credential: Option<(&'static str, String)>,
    extras: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut headers: Vec<_> = fixed
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    if let Some((name, value)) = credential {
        headers.push((name.to_owned(), value));
    }
    headers.extend(
        extras
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extras() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("x-b".to_owned(), "2".to_owned()),
            ("x-a".to_owned(), "1".to_owned()),
        ])
    }

    #[test]
    fn fixed_then_credential_then_extras_in_key_order() {
        let headers = request_headers(
            &[
                ("content-type", "application/json".to_owned()),
                ("anthropic-version", "2023-06-01".to_owned()),
            ],
            Some(("x-api-key", "k".to_owned())),
            &extras(),
        );
        assert_eq!(
            headers,
            [
                ("content-type", "application/json"),
                ("anthropic-version", "2023-06-01"),
                ("x-api-key", "k"),
                ("x-a", "1"),
                ("x-b", "2"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
    }

    #[test]
    fn an_empty_key_omits_the_credential_header() {
        let headers = request_headers(
            &[("content-type", "application/json".to_owned())],
            None,
            &extras(),
        );
        assert_eq!(
            headers,
            [
                ("content-type", "application/json"),
                ("x-a", "1"),
                ("x-b", "2"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
    }
}
