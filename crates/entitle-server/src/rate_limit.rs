//! A per-client-IP request limit (fixed one-minute windows, in memory) for
//! unauthenticated endpoints. The client IP is the TCP peer, or with
//! `ENTITLE_TRUST_PROXY` the last `X-Forwarded-For` entry (the one the proxy added).
//! IPv6 clients are keyed by their /64, which one host usually controls whole.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const WINDOW: Duration = Duration::from_secs(60);
/// At most this many clients are tracked; past it, new clients are refused until the
/// next sweep. Bounds memory, and the per-request cost, under many-address floods.
const MAX_TRACKED: usize = 10_000;

#[derive(Clone)]
pub struct RateLimit {
    per_minute: u32,
    trust_proxy: bool,
    windows: Arc<Mutex<Windows>>,
}

#[derive(Default)]
struct Windows {
    by_client: HashMap<IpAddr, (Instant, u32)>,
    /// Expired windows are dropped at most once per window.
    swept: Option<Instant>,
}

impl RateLimit {
    pub fn per_minute(per_minute: u32, trust_proxy: bool) -> Self {
        RateLimit {
            per_minute,
            trust_proxy,
            windows: Arc::default(),
        }
    }

    fn allow(&self, ip: IpAddr, now: Instant) -> bool {
        // IPv4-mapped (dual-stack listeners) back to IPv4 first, then IPv6 by /64.
        let ip = match ip.to_canonical() {
            IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & (!0u128 << 64)).into()),
            v4 => v4,
        };
        let mut windows = self.windows.lock().expect("rate limit lock");
        let Windows { by_client, swept } = &mut *windows;
        if swept.is_none_or(|t| now.duration_since(t) >= WINDOW) {
            by_client.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
            *swept = Some(now);
        }
        if by_client.len() >= MAX_TRACKED && !by_client.contains_key(&ip) {
            return false;
        }
        let (start, count) = by_client.entry(ip).or_insert((now, 0));
        if now.duration_since(*start) >= WINDOW {
            (*start, *count) = (now, 0);
        }
        *count += 1;
        *count <= self.per_minute
    }

    fn client_ip(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
        let forwarded = || {
            // The last field: a proxy may append its own instead of extending the first.
            let list = headers
                .get_all("x-forwarded-for")
                .iter()
                .next_back()?
                .to_str()
                .ok()?;
            list.rsplit(',').next()?.trim().parse().ok()
        };
        self.trust_proxy
            .then(forwarded)
            .flatten()
            .or(peer.map(|p| p.ip()))
            // In-process callers (tests) have no peer: they share one window.
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }
}

/// `429` with `Retry-After` once the client IP is over the limit.
pub async fn limit(State(limit): State<RateLimit>, request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let ip = limit.client_ip(request.headers(), peer);
    if limit.allow(ip, Instant::now()) {
        return next.run(request).await;
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        "too many requests, try again in a minute\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_and_resets_after_the_window() {
        let limit = RateLimit::per_minute(2, false);
        let (a, b) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let now = Instant::now();
        assert!(limit.allow(a, now));
        assert!(limit.allow(a, now));
        assert!(!limit.allow(a, now));
        assert!(limit.allow(b, now));
        assert!(limit.allow(a, now + WINDOW));
    }

    #[test]
    fn ipv6_shares_a_window_per_64() {
        let limit = RateLimit::per_minute(1, false);
        let now = Instant::now();
        assert!(limit.allow("2001:db8::1".parse().unwrap(), now));
        assert!(!limit.allow("2001:db8::ffff:2".parse().unwrap(), now));
        assert!(limit.allow("2001:db8:0:1::1".parse().unwrap(), now));
        // IPv4-mapped peers keep their own windows.
        assert!(limit.allow("::ffff:10.0.0.1".parse().unwrap(), now));
        assert!(limit.allow("::ffff:10.0.0.2".parse().unwrap(), now));
    }

    #[test]
    fn refuses_new_clients_when_full_until_the_sweep() {
        let limit = RateLimit::per_minute(5, false);
        let now = Instant::now();
        for i in 0..MAX_TRACKED as u32 {
            assert!(limit.allow(IpAddr::V4(i.into()), now));
        }
        let new = IpAddr::V4(u32::MAX.into());
        assert!(!limit.allow(new, now));
        assert!(limit.allow(IpAddr::V4(0.into()), now));
        assert!(limit.allow(new, now + WINDOW));
    }

    #[test]
    fn forwarded_for_counts_only_behind_a_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", "6.6.6.6".parse().unwrap());
        headers.append("x-forwarded-for", "1.1.1.1, 2.2.2.2".parse().unwrap());
        let peer = Some("9.9.9.9:1234".parse().unwrap());
        let ip = |trust| RateLimit::per_minute(1, trust).client_ip(&headers, peer);
        assert_eq!(ip(true), "2.2.2.2".parse::<IpAddr>().unwrap());
        assert_eq!(ip(false), "9.9.9.9".parse::<IpAddr>().unwrap());
    }
}
