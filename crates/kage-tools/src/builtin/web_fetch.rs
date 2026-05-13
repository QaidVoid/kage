//! `web_fetch` tool: GET an HTTP(S) URL and return readable text.
//!
//! Defends against SSRF by resolving the host to its IP addresses *before*
//! issuing the request and rejecting any address that lands in a private,
//! loopback, link-local, or otherwise non-routable range.

use std::fmt::Write as _;
use std::io::Read;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::ssrf;
use crate::{Tool, ToolContext, ToolError, schema_for};

const DEFAULT_MAX_BYTES: u64 = 2_000_000;

/// Input shape for the `web_fetch` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WebFetchInput {
    /// HTTP or HTTPS URL to fetch.
    url: String,
    /// Cap on response body bytes read. Defaults to 2,000,000.
    #[serde(default)]
    max_bytes: Option<u64>,
}

/// Fetch an HTTP(S) URL and return readable text.
#[derive(Debug, Default)]
pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch an HTTP(S) URL and return its body as readable text. HTML is \
         stripped to plain text. Refuses to fetch private, loopback, or \
         link-local addresses. Body is capped at 2MB by default."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<WebFetchInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Network
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: WebFetchInput = serde_json::from_value(input)?;
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let parsed = url::Url::parse(&input.url)
            .map_err(|e| ToolError::InvalidInput(format!("invalid url: {e}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unsupported scheme: {other}"
                )));
            }
        }
        ssrf::check(&parsed)?;

        let max_bytes = input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        let agent = ureq::Agent::new_with_defaults();
        let response = agent
            .get(parsed.as_str())
            .header("user-agent", "kage/0.1 (+https://github.com/QaidVoid/kage)")
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(code) => {
                    ToolError::Other(format!("{} returned http {code}", parsed.as_str()))
                }
                other => ToolError::Other(format!("transport error: {other}")),
            })?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut reader = response.into_body().into_reader();
        let mut buf = Vec::new();
        let mut taken = (&mut reader).take(max_bytes + 1);
        let _ = taken.read_to_end(&mut buf);
        let truncated = u64::try_from(buf.len()).unwrap_or(u64::MAX) > max_bytes;
        if truncated {
            buf.truncate(usize::try_from(max_bytes).unwrap_or(buf.len()));
        }

        let text = if content_type.contains("html") {
            html2text::from_read(&buf[..], 100)
                .unwrap_or_else(|_| String::from_utf8_lossy(&buf).into_owned())
        } else {
            String::from_utf8_lossy(&buf).into_owned()
        };

        let mut output = text;
        if truncated {
            let _ = write!(output, "\n\n[... truncated at {max_bytes} bytes ...]");
        }

        Ok(ToolOutput {
            is_error: false,
            text: output,
            structured: Some(serde_json::json!({
                "url": parsed.as_str(),
                "status": status,
                "content_type": content_type,
                "truncated": truncated,
            })),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_scheme() {
        let err = WebFetchTool
            .execute(
                serde_json::json!({"url": "file:///etc/passwd"}),
                &ToolContext::new(std::path::Path::new("/tmp"), &kage_core::CancelFlag::new()),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn rejects_private_url() {
        let err = WebFetchTool
            .execute(
                serde_json::json!({"url": "http://127.0.0.1:8080/x"}),
                &ToolContext::new(std::path::Path::new("/tmp"), &kage_core::CancelFlag::new()),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
    }
}
