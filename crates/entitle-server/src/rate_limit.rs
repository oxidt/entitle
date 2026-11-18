//! Request limits, in memory:
//!
//! - [`RateLimit`]: per client IP, fixed one-minute windows, for unauthenticated endpoints.
//! - [`SdkBudget`]: token buckets per client IP and per project for public-key SDK calls
//!   that create customers or call a store ([`BudgetedKey`]).
//!
//! The client IP is the TCP peer, or with `ENTITLE_TRUST_PROXY` the last `X-Forwarded-For`
//! entry (the one the proxy added). IPv6 clients are keyed by their /64, which one host
//! usually controls whole.

use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use entitle_core::ProjectId;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{KeyKind, ProjectKey};
use crate::error::ApiError;

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
        let ip = source(ip);
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
}

/// The client's IP: see the module docs.
fn client_ip(trust_proxy: bool, headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
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
    trust_proxy
        .then(forwarded)
        .flatten()
        .or(peer.map(|p| p.ip()))
        // In-process callers (tests) have no peer: they share one window.
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// What limits key on: IPv4-mapped addresses (dual-stack listeners) as IPv4, IPv6 by /64.
fn source(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & (!0u128 << 64)).into()),
        v4 => v4,
    }
}

/// The request's client, as limits key on it ([`source`] of the client IP).
pub struct ClientSource(pub IpAddr);

impl FromRequestParts<AppState> for ClientSource {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        Ok(ClientSource(source(client_ip(
            state.trust_proxy,
            &parts.headers,
            peer,
        ))))
    }
}

/// `429` with `Retry-After` once the client IP is over the limit.
pub async fn limit(State(limit): State<RateLimit>, request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let ip = client_ip(limit.trust_proxy, request.headers(), peer);
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

/// At most this many buckets per [`Buckets`]; past it, new keys are refused until the
/// next sweep. Big enough for busy apps, small enough to bound memory (a few MB).
const MAX_BUCKETS: usize = 100_000;

/// Token buckets: `per_minute` tokens a minute, refilled continuously, bursts of as many.
struct Buckets<K> {
    per_minute: u32,
    state: Mutex<BucketState<K>>,
}

struct BucketState<K> {
    /// Tokens left and when they were counted.
    by_key: HashMap<K, (f64, Instant)>,
    /// Idle buckets are dropped at most once per minute.
    swept: Option<Instant>,
}

impl<K: Hash + Eq> Buckets<K> {
    fn per_minute(per_minute: u32) -> Self {
        Buckets {
            per_minute,
            state: Mutex::new(BucketState {
                by_key: HashMap::new(),
                swept: None,
            }),
        }
    }

    /// Takes a token, or says how long until the next one.
    fn take(&self, key: K, now: Instant) -> Result<(), Duration> {
        let capacity = f64::from(self.per_minute);
        let per_second = capacity / 60.0;
        let mut state = self.state.lock().expect("bucket lock");
        let BucketState { by_key, swept } = &mut *state;
        if swept.is_none_or(|t| now.duration_since(t) >= WINDOW) {
            // A bucket idle for a minute is full again: the same as not tracking it.
            by_key.retain(|_, (_, at)| now.duration_since(*at) < WINDOW);
            *swept = Some(now);
        }
        if by_key.len() >= MAX_BUCKETS && !by_key.contains_key(&key) {
            return Err(WINDOW);
        }
        let (tokens, at) = by_key.entry(key).or_insert((capacity, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * per_second).min(capacity);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / per_second))
        }
    }
}

/// Request budgets for public-key SDK calls that create customers or call a store:
/// per client IP (`ENTITLE_SDK_RATE_PER_IP`) and per project (`ENTITLE_SDK_RATE_PER_PROJECT`),
/// both in requests per minute.
#[derive(Clone)]
pub struct SdkBudget {
    per_ip: Arc<Buckets<IpAddr>>,
    per_project: Arc<Buckets<Uuid>>,
}

impl SdkBudget {
    pub fn per_minute(per_ip: u32, per_project: u32) -> Self {
        SdkBudget {
            per_ip: Arc::new(Buckets::per_minute(per_ip)),
            per_project: Arc::new(Buckets::per_minute(per_project)),
        }
    }

    /// The client's budget first, so one client over its own does not drain the project's.
    fn check(&self, project: ProjectId, client: IpAddr, now: Instant) -> Result<(), ApiError> {
        self.per_ip
            .take(client, now)
            .and_then(|()| self.per_project.take(project.0, now))
            .map_err(|wait| ApiError::TooManyRequests {
                retry_after: wait.as_secs_f64().ceil().max(1.0) as u64,
            })
    }
}

/// A [`ProjectKey`] whose request fits the [`SdkBudget`]. Secret keys (backends) are not
/// budgeted.
pub struct BudgetedKey(pub ProjectKey);

impl FromRequestParts<AppState> for BudgetedKey {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = ProjectKey::from_request_parts(parts, state).await?;
        if key.kind == KeyKind::Public {
            let Ok(ClientSource(client)) = ClientSource::from_request_parts(parts, state).await;
            state
                .sdk_budget
                .check(key.project_id, client, Instant::now())?;
        }
        Ok(BudgetedKey(key))
    }
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
        let ip = |trust| client_ip(trust, &headers, peer);
        assert_eq!(ip(true), "2.2.2.2".parse::<IpAddr>().unwrap());
        assert_eq!(ip(false), "9.9.9.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn buckets_allow_a_burst_then_refill_steadily() {
        let buckets = Buckets::per_minute(60);
        let now = Instant::now();
        for _ in 0..60 {
            assert!(buckets.take("a", now).is_ok());
        }
        assert_eq!(buckets.take("a", now), Err(Duration::from_secs(1)));
        assert!(buckets.take("b", now).is_ok());
        let later = now + Duration::from_secs(1);
        assert!(buckets.take("a", later).is_ok());
        assert!(buckets.take("a", later).is_err());
        // Never more than the burst, however long idle.
        let much_later = now + Duration::from_secs(3600);
        for _ in 0..60 {
            assert!(buckets.take("a", much_later).is_ok());
        }
        assert!(buckets.take("a", much_later).is_err());
    }

    #[test]
    fn sdk_budget_is_per_client_and_per_project() {
        let budget = SdkBudget::per_minute(2, 3);
        let (p, q) = (ProjectId(Uuid::from_u128(1)), ProjectId(Uuid::from_u128(2)));
        let (a, b) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let now = Instant::now();
        assert!(budget.check(p, a, now).is_ok());
        assert!(budget.check(p, a, now).is_ok());
        let Err(ApiError::TooManyRequests { retry_after }) = budget.check(p, a, now) else {
            panic!("client a is over its budget");
        };
        assert_eq!(retry_after, 30);
        // Another client still gets the project's last token, then the project is out.
        assert!(budget.check(p, b, now).is_ok());
        assert!(budget.check(p, b, now).is_err());
        assert!(budget.check(q, "10.0.0.3".parse().unwrap(), now).is_ok());
    }
}
