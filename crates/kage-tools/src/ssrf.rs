//! SSRF guards shared by built-in tools and plugin HTTP helpers.
//!
//! The agent's HTTP-touching surfaces resolve a hostname to its IP set
//! before issuing any request and reject non-routable addresses
//! (loopback, private, link-local, multicast, documentation, etc.). This
//! refuses common SSRF attacks where a malicious URL points at internal
//! services like `http://169.254.169.254/`.

use std::net::{IpAddr, ToSocketAddrs};

use crate::ToolError;

/// Resolve `url` and reject if any returned address is non-routable.
///
/// `url` must already have a parsed scheme; this function only looks at
/// the host and port. It performs DNS resolution synchronously.
pub fn check(url: &url::Url) -> Result<(), ToolError> {
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

/// True if `ip` is non-routable (loopback, private, multicast, etc.).
#[must_use]
pub fn is_unsafe(ip: &IpAddr) -> bool {
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
}
