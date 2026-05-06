//! `web_fetch` tool: GET an HTTP(S) URL and return readable text.
//!
//! Defends against SSRF by resolving the host to its IP addresses *before*
//! issuing the request and rejecting any address that lands in a private,
//! loopback, link-local, or otherwise non-routable range.

use std::fmt::Write as _;
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

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
        check_ssrf(&parsed)?;

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
        })
    }
}

fn check_ssrf(url: &url::Url) -> Result<(), ToolError> {
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::InvalidInput("url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(0);
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| ToolError::Other(format!("dns resolve failed for {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(ToolError::Other(format!("no DNS records for {host}")));
    }
    for addr in addrs {
        if is_unsafe(&addr.ip()) {
            return Err(ToolError::InvalidInput(format!(
                "refusing to fetch {host}: resolved to non-routable address {}",
                addr.ip()
            )));
        }
    }
    Ok(())
}

fn is_unsafe(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            let segs = v6.segments();
            // Unique local fc00::/7
            if segs[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            // Link-local fe80::/10
            if segs[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // IPv4-mapped ::ffff:0:0/96
            if segs[0..6] == [0, 0, 0, 0, 0, 0xffff] {
                let v4 = std::net::Ipv4Addr::new(
                    (segs[6] >> 8) as u8,
                    (segs[6] & 0xff) as u8,
                    (segs[7] >> 8) as u8,
                    (segs[7] & 0xff) as u8,
                );
                return is_unsafe(&IpAddr::V4(v4));
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn loopback_v4_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn private_v4_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))));
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
    }

    #[test]
    fn link_local_v4_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1))));
    }

    #[test]
    fn public_v4_is_safe() {
        assert!(!is_unsafe(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_unsafe(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn loopback_v6_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn unique_local_v6_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn link_local_v6_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn ipv4_mapped_v6_inherits_v4_classification() {
        // ::ffff:127.0.0.1
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001
        ))));
    }

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
