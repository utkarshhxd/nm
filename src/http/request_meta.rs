// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, header, request::Parts},
};

use crate::app::AppState;

/// Longest user agent we keep, in characters. A user agent is attacker-controlled input.
pub const MAX_USER_AGENT_CHARS: usize = 512;

/// Where a request came from, for audit rows and per-IP rate limiting.
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

/// The client address. `X-Forwarded-For` is a client-controlled header, so it is ignored unless
/// `trust_proxy_headers` says a proxy we run sets it. Then the rightmost entry is used: it is the
/// one our own proxy appended, while everything to its left is whatever the client sent.
/// Entries may be a bare address, `addr:port` or `[v6]:port` (Azure Application Gateway and Front
/// Door append the port). Absent, empty or unparseable header: the peer address.
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    trust_proxy_headers: bool,
) -> Option<IpAddr> {
    if !trust_proxy_headers {
        return peer;
    }
    // The last header line, as a proxy appends rather than rewrites.
    let Some(value) = headers.get_all("x-forwarded-for").iter().next_back() else {
        return peer;
    };
    let parsed = value
        .to_str()
        .ok()
        .and_then(|v| v.rsplit(',').next())
        .and_then(|entry| parse_forwarded_entry(entry.trim()));
    if parsed.is_none() {
        // Never log the header: it is client-influenced.
        tracing::warn!(
            "trust_proxy_headers is on but the rightmost X-Forwarded-For entry is unparseable; \
             using the peer address. Is the proxy topology as configured?"
        );
    }
    parsed.or(peer)
}

/// Order matters: a bare IPv6 address must be tried first or it would be misread as host:port.
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|s| s.ip()))
        .or_else(|| entry.strip_prefix('[')?.strip_suffix(']')?.parse().ok())
}

pub(crate) fn truncate_user_agent(ua: &str) -> String {
    ua.chars().take(MAX_USER_AGENT_CHARS).collect()
}

impl FromRequestParts<AppState> for RequestMeta {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip());
        let user_agent = parts
            .headers
            .get(header::USER_AGENT)
            .map(|v| truncate_user_agent(&String::from_utf8_lossy(v.as_bytes())));
        Ok(RequestMeta {
            ip: client_ip(&parts.headers, peer, state.config.trust_proxy_headers),
            user_agent,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};
    use std::net::IpAddr;

    fn xff(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn forwarded_for_ignored_unless_trusted() {
        let got = super::client_ip(&xff("1.1.1.1"), Some(ip("10.0.0.1")), false);
        assert_eq!(got, Some(ip("10.0.0.1")));
    }

    #[test]
    fn trusted_proxy_uses_rightmost_entry() {
        let got = super::client_ip(&xff("6.6.6.6, 2.2.2.2"), Some(ip("10.0.0.1")), true);
        assert_eq!(got, Some(ip("2.2.2.2")));
    }

    #[test]
    fn trusted_proxy_garbage_falls_back_to_peer() {
        let got = super::client_ip(&xff("nonsense"), Some(ip("10.0.0.1")), true);
        assert_eq!(got, Some(ip("10.0.0.1")));
    }

    /// The rightmost entry of `value` as seen by a trusted-proxy deployment with peer 10.0.0.1.
    fn trusted(value: &str) -> Option<IpAddr> {
        super::client_ip(&xff(value), Some(ip("10.0.0.1")), true)
    }

    #[test]
    fn trusted_proxy_accepts_ipv4_with_port() {
        assert_eq!(trusted("1.2.3.4:5678"), Some(ip("1.2.3.4")));
        assert_eq!(trusted("6.6.6.6, 1.2.3.4:5678"), Some(ip("1.2.3.4")));
    }

    #[test]
    fn trusted_proxy_accepts_bracketed_ipv6_with_port() {
        assert_eq!(trusted("[2001:db8::1]:443"), Some(ip("2001:db8::1")));
    }

    #[test]
    fn trusted_proxy_accepts_bare_ipv6_not_misread_as_host_port() {
        assert_eq!(trusted("2001:db8::1"), Some(ip("2001:db8::1")));
    }

    #[test]
    fn trusted_proxy_accepts_bare_bracketed_ipv6() {
        assert_eq!(trusted("[2001:db8::1]"), Some(ip("2001:db8::1")));
    }

    #[test]
    fn trusted_proxy_garbage_with_port_falls_back_to_peer() {
        assert_eq!(trusted("nonsense:80"), Some(ip("10.0.0.1")));
    }

    #[test]
    fn trusted_proxy_empty_value_falls_back_to_peer() {
        assert_eq!(trusted(""), Some(ip("10.0.0.1")));
        assert_eq!(trusted("2.2.2.2,"), Some(ip("10.0.0.1")));
    }

    #[test]
    fn trusted_proxy_trims_whitespace() {
        assert_eq!(trusted("  6.6.6.6 ,\t2.2.2.2  "), Some(ip("2.2.2.2")));
    }

    #[test]
    fn trusted_proxy_uses_last_of_multiple_header_lines() {
        let mut h = xff("6.6.6.6");
        h.append(
            "x-forwarded-for",
            HeaderValue::from_static("5.5.5.5, 2.2.2.2"),
        );
        assert_eq!(
            super::client_ip(&h, Some(ip("10.0.0.1")), true),
            Some(ip("2.2.2.2"))
        );
    }

    #[test]
    fn trusted_proxy_without_header_uses_peer() {
        let got = super::client_ip(&HeaderMap::new(), Some(ip("10.0.0.1")), true);
        assert_eq!(got, Some(ip("10.0.0.1")));
    }
}
