//! Protection and observation of a public server: rate limits, a request timeout, an access log
//! and metrics. It has no dependency of its own: limits are token buckets kept in memory, the log
//! goes to standard error (the journal of systemd adds the time) and the metrics are plain text in the
//! Prometheus format.
//!
//! The guard is a middleware that `main` installs together with an `Extension<Arc<Guard>>`;
//! a router without them (every test) is not limited. Nothing here logs a token, a password or a
//! query string.

use super::AppState;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// A request that takes longer than this is answered with 504 and its work is dropped.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Buckets that have not been touched for this long are forgotten.
const IDLE_BUCKET: Duration = Duration::from_secs(600);
/// A hard bound on the memory of the limiter.
const MAX_BUCKETS: usize = 200_000;
/// Anything slower than this is logged even when the access log only reports problems.
const SLOW_MS: u128 = 500;

/// `[limits]` of `server.toml`. Every field is optional; the defaults suit a public server.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct LimitsConfig {
    /// Registrations and logins allowed per minute from one address.
    #[serde(default = "default_auth_per_minute")]
    pub auth_per_minute: u32,
    /// Sustained requests per second from one address, and the burst allowed on top of it.
    #[serde(default = "default_requests_per_second")]
    pub requests_per_second: u32,
    #[serde(default = "default_burst")]
    pub burst: u32,
    /// Behind a reverse proxy that you run, take the client address from the last entry of
    /// `X-Forwarded-For` (the one the proxy added). Never enable it on a directly exposed server.
    #[serde(default)]
    pub trust_proxy: bool,
    /// `errors` (default: status 400 and above, and slow requests), `all` or `off`.
    #[serde(default = "default_access_log")]
    pub access_log: String,
    /// Bearer token that lets a remote scraper read `/metrics`; without it only loopback may.
    #[serde(default)]
    pub metrics_token: Option<String>,
}

fn default_auth_per_minute() -> u32 {
    10
}
fn default_requests_per_second() -> u32 {
    100
}
fn default_burst() -> u32 {
    300
}
fn default_access_log() -> String {
    "errors".to_owned()
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            auth_per_minute: default_auth_per_minute(),
            requests_per_second: default_requests_per_second(),
            burst: default_burst(),
            trust_proxy: false,
            access_log: default_access_log(),
            metrics_token: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Class {
    /// Registration and login: guessing passwords and making accounts.
    Auth,
    General,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccessLog {
    Off,
    Errors,
    All,
}

/// The state of the guard: buckets and counters.
pub(super) struct Guard {
    config: LimitsConfig,
    log: AccessLog,
    buckets: Mutex<HashMap<(IpAddr, Class), Bucket>>,
    started: Instant,
    classes: [AtomicU64; 4],
    limited: AtomicU64,
    duration_ms_sum: AtomicU64,
    duration_count: AtomicU64,
    timeouts: AtomicU64,
    groups: Vec<(&'static str, AtomicU64)>,
}

const GROUPS: [&str; 14] = [
    "players",
    "world",
    "terrain",
    "friends",
    "events",
    "chat",
    "shops",
    "trades",
    "placed",
    "portals",
    "story",
    "federation",
    "land",
    "other",
];

impl Guard {
    pub(super) fn new(config: LimitsConfig) -> Self {
        let log = match config.access_log.as_str() {
            "off" => AccessLog::Off,
            "all" => AccessLog::All,
            _ => AccessLog::Errors,
        };
        Self {
            config,
            log,
            buckets: Mutex::new(HashMap::new()),
            started: Instant::now(),
            classes: Default::default(),
            limited: AtomicU64::new(0),
            duration_ms_sum: AtomicU64::new(0),
            duration_count: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            groups: GROUPS
                .iter()
                .map(|group| (*group, AtomicU64::new(0)))
                .collect(),
        }
    }

    /// Takes one token from the bucket of this address and class, or says how long to wait.
    fn take(&self, ip: IpAddr, class: Class, now: Instant) -> Result<(), Duration> {
        let (capacity, per_second) = match class {
            Class::Auth => {
                let per_minute = f64::from(self.config.auth_per_minute.max(1));
                (per_minute, per_minute / 60.0)
            }
            Class::General => (
                f64::from(self.config.burst.max(1)),
                f64::from(self.config.requests_per_second.max(1)),
            ),
        };
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if buckets.len() >= MAX_BUCKETS {
            buckets.retain(|_, bucket| now.duration_since(bucket.last) < IDLE_BUCKET);
            if buckets.len() >= MAX_BUCKETS {
                // Under a flood from more addresses than the table holds, refuse the new ones.
                return Err(Duration::from_secs(60));
            }
        }
        let bucket = buckets.entry((ip, class)).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_second).min(capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(
                ((1.0 - bucket.tokens) / per_second).max(1.0),
            ))
        }
    }

    /// Forgets the buckets nobody has used for a while.
    fn sweep(&self, now: Instant) {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        buckets.retain(|_, bucket| now.duration_since(bucket.last) < IDLE_BUCKET);
    }

    fn count_status(&self, status: StatusCode) {
        let index = match status.as_u16() {
            200..=299 => 0,
            300..=399 => 1,
            400..=499 => 2,
            _ => 3,
        };
        self.classes[index].fetch_add(1, Ordering::Relaxed);
    }

    fn count_group(&self, path: &str) {
        let group = group_of(path);
        if let Some((_, counter)) = self.groups.iter().find(|(name, _)| *name == group) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The text of `/metrics`, with the gauges the caller measured.
    fn render(&self, online: i64, alive: i64, pool_open: u32, pool_idle: usize) -> String {
        let mut text = String::new();
        let mut line = |help: &str, kind: &str, name: &str, value: String| {
            text.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{value}\n"
            ));
        };
        let classes = ["2xx", "3xx", "4xx", "5xx"];
        line(
            "HTTP requests answered, by status class.",
            "counter",
            "ishtaria_http_requests_total",
            classes
                .iter()
                .zip(&self.classes)
                .map(|(class, counter)| {
                    format!(
                        "ishtaria_http_requests_total{{class=\"{class}\"}} {}",
                        counter.load(Ordering::Relaxed)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        line(
            "HTTP requests by area of the API.",
            "counter",
            "ishtaria_http_requests_by_group_total",
            self.groups
                .iter()
                .map(|(group, counter)| {
                    format!(
                        "ishtaria_http_requests_by_group_total{{group=\"{group}\"}} {}",
                        counter.load(Ordering::Relaxed)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        line(
            "Requests refused by a rate limit.",
            "counter",
            "ishtaria_http_rate_limited_total",
            format!(
                "ishtaria_http_rate_limited_total {}",
                self.limited.load(Ordering::Relaxed)
            ),
        );
        line(
            "Requests that ran into the request timeout.",
            "counter",
            "ishtaria_http_timeouts_total",
            format!(
                "ishtaria_http_timeouts_total {}",
                self.timeouts.load(Ordering::Relaxed)
            ),
        );
        line(
            "Time spent answering requests, in milliseconds.",
            "summary",
            "ishtaria_http_request_duration_ms",
            format!(
                "ishtaria_http_request_duration_ms_sum {}\nishtaria_http_request_duration_ms_count {}",
                self.duration_ms_sum.load(Ordering::Relaxed),
                self.duration_count.load(Ordering::Relaxed)
            ),
        );
        line(
            "Living characters whose client was active in the last 30 seconds.",
            "gauge",
            "ishtaria_players_online",
            format!("ishtaria_players_online {online}"),
        );
        line(
            "Living characters.",
            "gauge",
            "ishtaria_players_alive",
            format!("ishtaria_players_alive {alive}"),
        );
        line(
            "Connections of the database pool.",
            "gauge",
            "ishtaria_db_pool_connections",
            format!(
                "ishtaria_db_pool_connections{{state=\"open\"}} {pool_open}\nishtaria_db_pool_connections{{state=\"idle\"}} {pool_idle}"
            ),
        );
        line(
            "Seconds since the server started.",
            "gauge",
            "ishtaria_uptime_seconds",
            format!(
                "ishtaria_uptime_seconds {}",
                self.started.elapsed().as_secs()
            ),
        );
        line(
            "Version of the server.",
            "gauge",
            "ishtaria_build_info",
            format!(
                "ishtaria_build_info{{version=\"{}\"}} 1",
                env!("CARGO_PKG_VERSION")
            ),
        );
        text
    }
}

/// The area of the API a path belongs to (for the metrics; unknown paths are `other`).
fn group_of(path: &str) -> &'static str {
    let mut parts = path.trim_start_matches('/').split('/');
    match parts.next().unwrap_or("") {
        "players" => "players",
        "world" => "world",
        "terrain" => "terrain",
        "friends" => "friends",
        "events" => "events",
        "chat" => "chat",
        "shops" => "shops",
        "trades" => "trades",
        "placed" => "placed",
        "portals" => "portals",
        "story" => "story",
        "federation" | ".well-known" => "federation",
        "land" => "land",
        _ => "other",
    }
}

fn class_of(method: &Method, path: &str) -> Option<Class> {
    if path == "/health" {
        return None;
    }
    if method == Method::POST && (path == "/players" || path == "/players/login") {
        return Some(Class::Auth);
    }
    Some(Class::General)
}

/// The address of the client: the peer of the connection, or the last entry of
/// `X-Forwarded-For` when the operator says a proxy of theirs sits in front.
fn client_ip(request: &Request, trust_proxy: bool) -> IpAddr {
    if trust_proxy {
        if let Some(address) = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
        {
            return address;
        }
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |info| info.0.ip())
}

fn refusal(retry_after: Duration) -> Response {
    let seconds = retry_after.as_secs().max(1).to_string();
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    if let Ok(value) = HeaderValue::from_str(&seconds) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The middleware: limit, time, count and log. Installed by `main` with the guard as an extension.
pub(super) async fn middleware(request: Request, next: Next) -> Response {
    let Some(guard) = request.extensions().get::<Arc<Guard>>().cloned() else {
        return next.run(request).await;
    };
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let ip = client_ip(&request, guard.config.trust_proxy);
    guard.count_group(&path);
    if let Some(class) = class_of(&method, &path) {
        if let Err(wait) = guard.take(ip, class, started) {
            guard.limited.fetch_add(1, Ordering::Relaxed);
            guard.count_status(StatusCode::TOO_MANY_REQUESTS);
            if guard.log != AccessLog::Off {
                eprintln!("access ip={ip} method={method} path={path} status=429 limited=true");
            }
            return refusal(wait);
        }
    }
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            guard.timeouts.fetch_add(1, Ordering::Relaxed);
            (StatusCode::GATEWAY_TIMEOUT, "the request took too long").into_response()
        }
    };
    let elapsed = started.elapsed();
    guard.count_status(response.status());
    guard
        .duration_ms_sum
        .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
    let count = guard.duration_count.fetch_add(1, Ordering::Relaxed) + 1;
    if count % 5000 == 0 {
        guard.sweep(started);
    }
    let status = response.status();
    let report = match guard.log {
        AccessLog::Off => false,
        AccessLog::All => true,
        AccessLog::Errors => status.as_u16() >= 400 || elapsed.as_millis() >= SLOW_MS,
    };
    if report {
        eprintln!(
            "access ip={ip} method={method} path={path} status={} ms={}",
            status.as_u16(),
            elapsed.as_millis()
        );
    }
    response
}

/// `GET /metrics`: Prometheus text. Only the loopback address may read it, or anyone who presents the
/// configured `metrics_token`. Without the guard (tests, tools) the route does not exist.
pub(super) async fn metrics(State(state): State<AppState>, request: Request) -> Response {
    let Some(guard) = request.extensions().get::<Arc<Guard>>().cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let loopback = peer.is_some_and(|ip| ip.is_loopback()) && !guard.config.trust_proxy;
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let token_ok = match (&guard.config.metrics_token, presented) {
        (Some(expected), Some(given)) => constant_time_eq(expected.as_bytes(), given.as_bytes()),
        _ => false,
    };
    if !loopback && !token_ok {
        return StatusCode::NOT_FOUND.into_response();
    }
    let online: Result<i64, _> = sqlx::query_scalar("SELECT count(*) FROM players WHERE world_id = $1 AND died_at IS NULL AND last_seen_at > now() - interval '30 seconds'")
        .bind(state.world_id).fetch_one(&state.pool).await;
    let alive: Result<i64, _> =
        sqlx::query_scalar("SELECT count(*) FROM players WHERE world_id = $1 AND died_at IS NULL")
            .bind(state.world_id)
            .fetch_one(&state.pool)
            .await;
    let body = guard.render(
        online.unwrap_or(-1),
        alive.unwrap_or(-1),
        state.pool.size(),
        state.pool.num_idle(),
    );
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

/// Compares two secrets without stopping at the first difference.
fn constant_time_eq(first: &[u8], second: &[u8]) -> bool {
    if first.len() != second.len() {
        return false;
    }
    first
        .iter()
        .zip(second)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn a_bucket_empties_refills_and_is_kept_per_address() {
        let guard = Guard::new(LimitsConfig {
            auth_per_minute: 3,
            ..LimitsConfig::default()
        });
        let start = Instant::now();
        for _ in 0..3 {
            assert!(guard.take(address(1), Class::Auth, start).is_ok());
        }
        let wait = guard.take(address(1), Class::Auth, start).unwrap_err();
        assert!(wait >= Duration::from_secs(1) && wait <= Duration::from_secs(60));
        assert!(
            guard.take(address(2), Class::Auth, start).is_ok(),
            "another address has its own bucket"
        );
        assert!(
            guard.take(address(1), Class::General, start).is_ok(),
            "classes are separate"
        );
        // 3 per minute: one token again after twenty seconds, never more than the capacity.
        let later = start + Duration::from_secs(21);
        assert!(guard.take(address(1), Class::Auth, later).is_ok());
        assert!(guard.take(address(1), Class::Auth, later).is_err());
        let much_later = start + Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(guard.take(address(1), Class::Auth, much_later).is_ok());
        }
        assert!(guard.take(address(1), Class::Auth, much_later).is_err());
    }

    #[test]
    fn idle_buckets_are_forgotten() {
        let guard = Guard::new(LimitsConfig::default());
        let start = Instant::now();
        guard.take(address(1), Class::General, start).unwrap();
        guard.sweep(start + IDLE_BUCKET + Duration::from_secs(1));
        assert!(guard.buckets.lock().unwrap().is_empty());
    }

    #[test]
    fn paths_are_classified_and_grouped() {
        assert_eq!(class_of(&Method::GET, "/health"), None);
        assert_eq!(class_of(&Method::POST, "/players"), Some(Class::Auth));
        assert_eq!(class_of(&Method::POST, "/players/login"), Some(Class::Auth));
        assert_eq!(class_of(&Method::GET, "/players/me"), Some(Class::General));
        assert_eq!(class_of(&Method::POST, "/chat/say"), Some(Class::General));
        assert_eq!(group_of("/players/me/cast"), "players");
        assert_eq!(group_of("/shops/markets:trader_00/buy"), "shops");
        assert_eq!(group_of("/nonsense"), "other");
        assert!(GROUPS
            .iter()
            .all(|group| group_of(&format!("/{group}")) == *group
                || *group == "other"
                || *group == "federation"));
    }

    #[test]
    fn secrets_compare_in_constant_time_and_by_value() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }
}
