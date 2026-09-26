//! SSRF guards shared by built-in tools and plugin HTTP helpers.
//!
//! The agent's HTTP-touching surfaces resolve a hostname to its IP set
//! before issuing any request and reject non-routable addresses
//! (loopback, private, link-local, multicast, documentation, etc.). This
//! refuses common SSRF attacks where a malicious URL points at internal
//! services like `http://169.254.169.254/`.
//!
//! [`check`] vets the caller-supplied URL up front for a clear error.
//! [`guarded_agent`] builds an HTTP agent that re-applies the same policy
//! to every DNS resolution, so redirect hops and rebinding DNS answers
//! cannot reach a non-routable address either.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::ToolError;

/// Build a ureq agent from `config` whose every DNS lookup (the first
/// dial and each redirect hop) goes through [`is_unsafe`], so only
/// routable addresses can ever be dialed.
///
/// A refused hop fails the request with a
/// [`std::io::ErrorKind::PermissionDenied`] I/O error.
#[must_use]
pub fn guarded_agent(config: ureq::config::Config) -> ureq::Agent {
    ureq::Agent::with_parts(config, DefaultConnector::new(), SsrfResolver::default())
}

/// Like [`guarded_agent`], but also lets `allowed` through so tests can
/// serve from a loopback listener while every other address stays vetted.
#[cfg(test)]
pub(crate) fn guarded_agent_allowing(
    config: ureq::config::Config,
    allowed: SocketAddr,
) -> ureq::Agent {
    let resolver = SsrfResolver {
        inner: DefaultResolver::default(),
        allowed: Some(allowed),
    };
    ureq::Agent::with_parts(config, DefaultConnector::new(), resolver)
}

/// Wraps ureq's [`DefaultResolver`] to enforce the SSRF policy at
/// resolution time, so the transport only ever sees vetted addresses.
#[derive(Debug, Default)]
struct SsrfResolver {
    inner: DefaultResolver,
    allowed: Option<SocketAddr>,
}

impl Resolver for SsrfResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.inner.resolve(uri, config, timeout)?;
        let refused = addrs
            .iter()
            .any(|addr| Some(*addr) != self.allowed && is_unsafe(&addr.ip()));
        if refused {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("ssrf guard: refusing {uri}, it resolves to a non-routable address"),
            )));
        }
        Ok(addrs)
    }
}

/// Resolve `url` and reject if any returned address is non-routable.
///
/// `url` must already have a parsed scheme; this function only looks at
/// the host and port. It performs DNS resolution synchronously.
pub fn check(url: &url::Url) -> Result<(), ToolError> {
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::InvalidInput("url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(0);
    // An IPv6 literal arrives bracketed (`[::1]`); the brackets are URI
    // syntax, not part of the address, and the resolver rejects them.
    // Strip one pair before resolving. Policy is unchanged: the
    // resolved address still goes through `is_unsafe` below.
    let dial_host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let addrs: Vec<_> = (dial_host, port)
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

/// True if `ip` is non-routable (loopback, private, multicast, etc.).
#[must_use]
pub fn is_unsafe(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                // This network 0.0.0.0/8: dialing 0.x commonly lands
                // on loopback.
                || o[0] == 0
                // CGNAT 100.64.0.0/10 (Tailscale and friends).
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                // IETF protocol assignments 192.0.0.0/24.
                || o[0..3] == [192, 0, 0]
                // Benchmarking 198.18.0.0/15.
                || (o[0] == 198 && (18..=19).contains(&o[1]))
                // Reserved 240.0.0.0/4.
                || o[0] >= 240
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
            // IPv4-mapped ::ffff:0:0/96, NAT64 64:ff9b::/96, and the
            // deprecated IPv4-compatible ::/96 all carry an IPv4
            // address in their last 32 bits; vet it by the IPv4 rules.
            let mapped = segs[0..6] == [0, 0, 0, 0, 0, 0xffff];
            let nat64 = segs[0..6] == [0x64, 0xff9b, 0, 0, 0, 0];
            let compatible = segs[0..6] == [0, 0, 0, 0, 0, 0];
            if mapped || nat64 || compatible {
                return is_unsafe(&IpAddr::V4(embedded_v4(segs)));
            }
            false
        }
    }
}

/// The IPv4 address embedded in the last 32 bits of `segs`.
fn embedded_v4(segs: [u16; 8]) -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::new(
        (segs[6] >> 8) as u8,
        (segs[6] & 0xff) as u8,
        (segs[7] >> 8) as u8,
        (segs[7] & 0xff) as u8,
    )
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
    fn bracketed_global_ipv6_literal_passes_check() {
        let url =
            url::Url::parse("http://[2400:1a00:5b2f:6cb2:869e:56ff:fe03:2b71]:18080/x").unwrap();
        assert!(check(&url).is_ok());
    }

    #[test]
    fn bracketed_loopback_ipv6_literal_stays_refused() {
        let url = url::Url::parse("http://[::1]:18080/x").unwrap();
        assert!(check(&url).is_err());
    }

    #[test]
    fn ssrf_resolver_refuses_disallowed_addrs() {
        let resolver = SsrfResolver::default();
        let config = ureq::config::Config::default();
        let timeout = NextTimeout {
            after: ureq::unversioned::transport::time::Duration::from_secs(2),
            reason: ureq::Timeout::Resolve,
        };
        let resolve = |uri: &'static str| {
            Resolver::resolve(&resolver, &uri.parse().unwrap(), &config, timeout)
        };

        let addrs = resolve("http://1.1.1.1/x").unwrap();
        assert!(addrs.iter().any(|a| a.ip() == IpAddr::from([1, 1, 1, 1])));

        for uri in [
            "http://127.0.0.1/x",
            "http://10.0.0.1/x",
            "http://169.254.169.254/x",
            "http://[::1]/x",
        ] {
            let err = resolve(uri).unwrap_err();
            let ureq::Error::Io(io) = &err else {
                panic!("{uri}: {err:?}");
            };
            assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied, "{uri}");
            assert!(io.to_string().contains("ssrf guard"), "{uri}: {io}");
        }
    }

    #[test]
    fn unique_local_v6_is_unsafe() {
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn shared_and_reserved_v4_ranges_are_unsafe() {
        // CGNAT 100.64.0.0/10.
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(100, 127, 255, 254))));
        assert!(!is_unsafe(&IpAddr::V4(Ipv4Addr::new(100, 63, 255, 254))));
        assert!(!is_unsafe(&IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        // Benchmarking 198.18.0.0/15.
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1))));
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(198, 19, 255, 254))));
        assert!(!is_unsafe(&IpAddr::V4(Ipv4Addr::new(198, 20, 0, 1))));
        // IETF protocol assignments 192.0.0.0/24.
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(192, 0, 0, 9))));
        assert!(!is_unsafe(&IpAddr::V4(Ipv4Addr::new(192, 0, 1, 1))));
        // Reserved 240.0.0.0/4.
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1))));
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(254, 1, 2, 3))));
        // This network 0.0.0.0/8.
        assert!(is_unsafe(&IpAddr::V4(Ipv4Addr::new(0, 1, 2, 3))));
    }

    #[test]
    fn ipv4_embedded_v6_forms_are_vetted_by_the_ipv4_rules() {
        // NAT64 64:ff9b::/96.
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0x64, 0xff9b, 0, 0, 0, 0, 0x7f00, 1
        ))));
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0x64, 0xff9b, 0, 0, 0, 0, 0x0a00, 1
        ))));
        assert!(!is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0x64, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808
        ))));
        // Deprecated IPv4-compatible ::/96.
        assert!(is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0, 0x0a00, 1
        ))));
        // A global address outside the embedded forms stays safe.
        assert!(!is_unsafe(&IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0, 0, 0, 0, 0x6810, 0x85e3
        ))));
    }
}
