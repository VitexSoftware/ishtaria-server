//! Federation identity of a world: its signing key, the pinned keys of peers, and the
//! network access to other worlds. Portals of two worlds are linked in `portals.rs`; the
//! operator policy (`closed`, `approve`, `open`) decides whether a link opens at once.

use super::players::Error;
use super::AppState;
use axum::{extract::State, http::StatusCode, routing::get, Extension, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::{
    future::Future,
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) const LINK_REQUEST_MAX_SECONDS: i64 = 600;
pub(super) const CLOCK_SKEW_SECONDS: i64 = 300;
pub(super) const MAX_PEER_REPLY: u64 = 16 * 1024;

static OUTBOUND: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) type Fut<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>;

/// What a world publishes at `/.well-known/ishtaria/server.json`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct ServerInfo {
    pub protocol: u8,
    pub server_name: String,
    pub api_url: String,
    pub public_key: String,
    pub ruleset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub federation_policy: Option<String>,
}

/// Network access to other worlds, replaceable in tests.
pub(super) trait PeerDirectory: Send + Sync {
    fn server_info(&self, api_url: String, allow_private: bool) -> Fut<ServerInfo>;
    /// What the peer says about one of its portals (`GET /federation/portals/{id}`).
    fn portal_info(&self, api_url: String, portal: String, allow_private: bool) -> Fut<PortalInfo>;
    /// Asks the peer to link its portal with ours; returns the state of the peer's end.
    fn post_link(&self, api_url: String, message: String, allow_private: bool) -> Fut<String>;
    /// Tells the peer that a link was broken by the owner of a portal.
    fn post_unlink(&self, api_url: String, message: String, allow_private: bool) -> Fut<()>;
}

/// What a world publishes about one of its portals: that it stands and whether it is complete.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct PortalInfo {
    pub world: String,
    pub portal: String,
    pub name: String,
    /// `building`, `built` (complete, not linked), `pending`, `open` or `closed`.
    pub state: String,
}

#[derive(FromRow)]
pub(super) struct Settings {
    pub public_url: Option<String>,
    pub policy: String,
    pub allow_private_peers: bool,
    pub signing_key: Vec<u8>,
    pub public_key: Vec<u8>,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/.well-known/ishtaria/server.json", get(server_info))
}

pub(super) fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

pub(super) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

pub(super) fn valid_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 253
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
}

pub(super) fn valid_portal(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(super) fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// `http(s)://host[:port][/path]` without credentials, query or fragment.
pub(super) fn valid_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    url.len() <= 300
        && !rest.is_empty()
        && !rest.starts_with('/')
        && !url.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '@' | '?' | '#' | '\\')
        })
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || (octets[0] == 100 && (64..128).contains(&octets[1]))
                || octets[0] == 0)
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(mapped));
            }
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80)
        }
    }
}

fn format_key(key: &[u8]) -> String {
    format!("ed25519:{}", B64.encode(key))
}

fn parse_key(text: &str) -> Result<VerifyingKey, Error> {
    let invalid = || status(StatusCode::UNPROCESSABLE_ENTITY, "invalid peer key");
    let bytes = B64
        .decode(text.strip_prefix("ed25519:").ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    VerifyingKey::from_bytes(&bytes.try_into().map_err(|_| invalid())?).map_err(|_| invalid())
}

pub(super) fn sign(key: &SigningKey, domain: &[u8], payload: &[u8]) -> String {
    let mut message = domain.to_vec();
    message.extend_from_slice(payload);
    format!(
        "{}.{}",
        B64.encode(payload),
        B64.encode(key.sign(&message).to_bytes())
    )
}

/// Splits `payload.signature` into decoded payload and signature bytes.
pub(super) fn open_token(token: &str) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let invalid = || status(StatusCode::BAD_REQUEST, "invalid signed message");
    let (payload, signature) = token.split_once('.').ok_or_else(invalid)?;
    if signature.contains('.') {
        return Err(invalid());
    }
    Ok((
        B64.decode(payload).map_err(|_| invalid())?,
        B64.decode(signature).map_err(|_| invalid())?,
    ))
}

pub(super) fn verify(
    key: &VerifyingKey,
    domain: &[u8],
    payload: &[u8],
    signature: &[u8],
) -> Result<(), Error> {
    let signature = Signature::from_slice(signature)
        .map_err(|_| status(StatusCode::UNAUTHORIZED, "invalid signature"))?;
    let mut message = domain.to_vec();
    message.extend_from_slice(payload);
    key.verify_strict(&message, &signature)
        .map_err(|_| status(StatusCode::UNAUTHORIZED, "invalid signature"))
}

pub(super) async fn settings(state: &AppState) -> Result<Settings, Error> {
    sqlx::query_as("SELECT public_url, policy, allow_private_peers, signing_key, public_key FROM federation_settings WHERE world_id = $1")
        .bind(state.world_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| status(StatusCode::SERVICE_UNAVAILABLE, "federation unavailable"))
}

pub(super) fn signing_key(settings: &Settings) -> Result<SigningKey, Error> {
    let bytes: [u8; 32] = settings
        .signing_key
        .as_slice()
        .try_into()
        .map_err(|_| status(StatusCode::SERVICE_UNAVAILABLE, "federation unavailable"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub(super) async fn server_name(state: &AppState) -> Result<String, Error> {
    Ok(
        sqlx::query_scalar("SELECT server_name FROM worlds WHERE id = $1")
            .bind(state.world_id)
            .fetch_one(&state.pool)
            .await?,
    )
}

pub(super) fn directory(
    extension: Option<Extension<Arc<dyn PeerDirectory>>>,
) -> Arc<dyn PeerDirectory> {
    extension.map_or_else(
        || Arc::new(HttpDirectory) as Arc<dyn PeerDirectory>,
        |Extension(directory)| directory,
    )
}

/// Creates the world's signing identity on first start and applies operator configuration.
pub(super) async fn ensure_settings(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    public_url: Option<&str>,
    policy: Option<&str>,
    allow_private_peers: Option<bool>,
) -> anyhow::Result<()> {
    if let Some(url) = public_url {
        anyhow::ensure!(
            valid_url(url),
            "public_url must be http(s)://host[:port][/path]"
        );
    }
    if let Some(policy) = policy {
        anyhow::ensure!(
            ["closed", "approve", "open"].contains(&policy),
            "federation policy must be closed, approve or open"
        );
    }
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let key = SigningKey::from_bytes(&seed);
    sqlx::query("INSERT INTO federation_settings (world_id, signing_key, public_key) VALUES ($1, $2, $3) ON CONFLICT (world_id) DO NOTHING")
        .bind(world_id)
        .bind(key.to_bytes().to_vec())
        .bind(key.verifying_key().to_bytes().to_vec())
        .execute(&mut **transaction)
        .await?;
    if public_url.is_some() || policy.is_some() || allow_private_peers.is_some() {
        sqlx::query("UPDATE federation_settings SET public_url = coalesce($2, public_url), policy = coalesce($3, policy), allow_private_peers = coalesce($4, allow_private_peers), updated_at = now() WHERE world_id = $1")
            .bind(world_id)
            .bind(public_url)
            .bind(policy)
            .bind(allow_private_peers)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

async fn server_info(State(state): State<AppState>) -> Result<Json<ServerInfo>, Error> {
    let settings = settings(&state).await?;
    let api_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "federation not configured"))?;
    let ruleset: String = sqlx::query_scalar("SELECT ruleset FROM worlds WHERE id = $1")
        .bind(state.world_id)
        .fetch_one(&state.pool)
        .await?;
    Ok(Json(ServerInfo {
        protocol: 1,
        server_name: server_name(&state).await?,
        api_url,
        public_key: format_key(&settings.public_key),
        ruleset,
        federation_policy: Some(settings.policy),
    }))
}

/// Host name of an `http(s)` URL, lowercased; IP literals are not world names.
fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split('/').next()?;
    if authority.starts_with('[') {
        return None;
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

/// A peer key found for a world name; it is pinned only after a signature made with it verifies.
pub(super) struct PeerKey {
    pub key: VerifyingKey,
    api_url: String,
    pinned: bool,
}

/// Looks up the pinned key of a peer or, on first contact, fetches the key it publishes.
/// Outside development networks the world name must be the host of the URL it is fetched
/// from, otherwise any server could claim to be another world and have its key pinned.
pub(super) async fn peer_key(
    state: &AppState,
    settings: &Settings,
    directory: &dyn PeerDirectory,
    host: &str,
    api_url: &str,
) -> Result<PeerKey, Error> {
    let pinned: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT state, public_key FROM federation_peers WHERE world_id = $1 AND host = $2",
    )
    .bind(state.world_id)
    .bind(host)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((peer_state, key)) = pinned {
        if peer_state == "banned" {
            return Err(status(StatusCode::FORBIDDEN, "peer banned"));
        }
        let bytes: [u8; 32] = key
            .try_into()
            .map_err(|_| status(StatusCode::SERVICE_UNAVAILABLE, "federation unavailable"))?;
        return Ok(PeerKey {
            key: VerifyingKey::from_bytes(&bytes)
                .map_err(|_| status(StatusCode::SERVICE_UNAVAILABLE, "federation unavailable"))?,
            api_url: api_url.to_string(),
            pinned: true,
        });
    }
    if !settings.allow_private_peers && url_host(api_url).as_deref() != Some(host) {
        return Err(status(
            StatusCode::UNPROCESSABLE_ENTITY,
            "peer address does not match its world name",
        ));
    }
    let info = directory
        .server_info(api_url.to_string(), settings.allow_private_peers)
        .await?;
    if info.protocol != 1 || info.server_name != host {
        return Err(status(
            StatusCode::UNPROCESSABLE_ENTITY,
            "peer identity mismatch",
        ));
    }
    Ok(PeerKey {
        key: parse_key(&info.public_key)?,
        api_url: api_url.to_string(),
        pinned: false,
    })
}

/// Pins a first-contact key once a message signed with it has verified (trust on first use).
pub(super) async fn pin_peer(state: &AppState, host: &str, peer: &PeerKey) -> Result<(), Error> {
    if peer.pinned {
        return Ok(());
    }
    sqlx::query("INSERT INTO federation_peers (world_id, host, api_url, public_key) VALUES ($1, $2, $3, $4) ON CONFLICT (world_id, host) DO NOTHING")
        .bind(state.world_id)
        .bind(host)
        .bind(&peer.api_url)
        .bind(peer.key.to_bytes().to_vec())
        .execute(&state.pool)
        .await?;
    let stored: (String, Vec<u8>) = sqlx::query_as(
        "SELECT state, public_key FROM federation_peers WHERE world_id = $1 AND host = $2",
    )
    .bind(state.world_id)
    .bind(host)
    .fetch_one(&state.pool)
    .await?;
    if stored.0 == "banned" {
        return Err(status(StatusCode::FORBIDDEN, "peer banned"));
    }
    if stored.1 != peer.key.to_bytes() {
        return Err(status(StatusCode::CONFLICT, "peer key changed"));
    }
    Ok(())
}

/// Resolver that refuses non-public addresses, so the checked address is the connected one.
struct PublicOnly {
    allow_private: bool,
}

impl ureq::Resolver for PublicOnly {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let addresses: Vec<SocketAddr> = netloc
            .to_socket_addrs()?
            .filter(|address| self.allow_private || is_public_ip(address.ip()))
            .collect();
        if addresses.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "peer address not allowed",
            ));
        }
        Ok(addresses)
    }
}

pub(super) fn agent(allow_private: bool) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .resolver(PublicOnly { allow_private })
        .redirects(0)
        .timeout(Duration::from_secs(5))
        .build()
}

pub(super) fn unreachable_peer() -> Error {
    status(StatusCode::BAD_GATEWAY, "peer unreachable")
}

/// Production directory: bounded, redirect-free HTTP to public peer addresses.
pub(super) struct HttpDirectory;

impl PeerDirectory for HttpDirectory {
    fn server_info(&self, api_url: String, allow_private: bool) -> Fut<ServerInfo> {
        Box::pin(async move {
            if !valid_url(&api_url) {
                return Err(status(StatusCode::BAD_REQUEST, "invalid peer address"));
            }
            let url = format!(
                "{}/.well-known/ishtaria/server.json",
                api_url.trim_end_matches('/')
            );
            let _permit = OUTBOUND.acquire().await.map_err(|_| unreachable_peer())?;
            tokio::task::spawn_blocking(move || {
                let response = agent(allow_private)
                    .get(&url)
                    .call()
                    .map_err(|_| unreachable_peer())?;
                let mut body = Vec::new();
                response
                    .into_reader()
                    .take(MAX_PEER_REPLY)
                    .read_to_end(&mut body)
                    .map_err(|_| unreachable_peer())?;
                serde_json::from_slice(&body)
                    .map_err(|_| status(StatusCode::BAD_GATEWAY, "invalid peer info"))
            })
            .await
            .map_err(|_| unreachable_peer())?
        })
    }

    fn portal_info(&self, api_url: String, portal: String, allow_private: bool) -> Fut<PortalInfo> {
        Box::pin(async move {
            if !valid_url(&api_url) || !valid_uuid(&portal) {
                return Err(status(StatusCode::BAD_REQUEST, "invalid peer address"));
            }
            let url = format!(
                "{}/federation/portals/{portal}",
                api_url.trim_end_matches('/')
            );
            let _permit = OUTBOUND.acquire().await.map_err(|_| unreachable_peer())?;
            tokio::task::spawn_blocking(move || {
                let response = match agent(allow_private).get(&url).call() {
                    Ok(response) => response,
                    Err(ureq::Error::Status(404 | 410, _)) => {
                        return Err(status(
                            StatusCode::NOT_FOUND,
                            "the portal does not stand there",
                        ))
                    }
                    Err(_) => return Err(unreachable_peer()),
                };
                let mut body = Vec::new();
                response
                    .into_reader()
                    .take(MAX_PEER_REPLY)
                    .read_to_end(&mut body)
                    .map_err(|_| unreachable_peer())?;
                serde_json::from_slice(&body)
                    .map_err(|_| status(StatusCode::BAD_GATEWAY, "invalid peer reply"))
            })
            .await
            .map_err(|_| unreachable_peer())?
        })
    }

    fn post_link(&self, api_url: String, message: String, allow_private: bool) -> Fut<String> {
        Box::pin(async move {
            if !valid_url(&api_url) {
                return Err(status(StatusCode::BAD_REQUEST, "invalid peer address"));
            }
            let url = format!("{}/federation/portals/link", api_url.trim_end_matches('/'));
            let body = serde_json::json!({ "message": message }).to_string();
            let _permit = OUTBOUND.acquire().await.map_err(|_| unreachable_peer())?;
            tokio::task::spawn_blocking(move || {
                match agent(allow_private)
                    .post(&url)
                    .set("Content-Type", "application/json")
                    .send_string(&body)
                {
                    Ok(response) => {
                        let mut reply = Vec::new();
                        response
                            .into_reader()
                            .take(MAX_PEER_REPLY)
                            .read_to_end(&mut reply)
                            .map_err(|_| unreachable_peer())?;
                        let value: serde_json::Value = serde_json::from_slice(&reply)
                            .map_err(|_| status(StatusCode::BAD_GATEWAY, "invalid peer reply"))?;
                        match value.get("state").and_then(|state| state.as_str()) {
                            Some("open") => Ok("open".to_owned()),
                            Some("pending") => Ok("pending".to_owned()),
                            _ => Err(status(StatusCode::BAD_GATEWAY, "invalid peer reply")),
                        }
                    }
                    Err(ureq::Error::Status(404, _)) => Err(status(
                        StatusCode::NOT_FOUND,
                        "the portal does not stand there",
                    )),
                    Err(ureq::Error::Status(409, _)) => Err(status(
                        StatusCode::CONFLICT,
                        "the peer portal is not ready to be linked",
                    )),
                    Err(ureq::Error::Status(403, _)) => {
                        Err(status(StatusCode::FORBIDDEN, "peer refused the link"))
                    }
                    Err(ureq::Error::Status(429, _)) => {
                        Err(status(StatusCode::TOO_MANY_REQUESTS, "peer limit reached"))
                    }
                    Err(ureq::Error::Status(_, _)) => {
                        Err(status(StatusCode::BAD_GATEWAY, "peer rejected the link"))
                    }
                    Err(_) => Err(unreachable_peer()),
                }
            })
            .await
            .map_err(|_| unreachable_peer())?
        })
    }

    fn post_unlink(&self, api_url: String, message: String, allow_private: bool) -> Fut<()> {
        Box::pin(async move {
            if !valid_url(&api_url) {
                return Err(status(StatusCode::BAD_REQUEST, "invalid peer address"));
            }
            let url = format!(
                "{}/federation/portals/unlink",
                api_url.trim_end_matches('/')
            );
            let body = serde_json::json!({ "message": message }).to_string();
            let _permit = OUTBOUND.acquire().await.map_err(|_| unreachable_peer())?;
            tokio::task::spawn_blocking(move || {
                match agent(allow_private)
                    .post(&url)
                    .set("Content-Type", "application/json")
                    .send_string(&body)
                {
                    Ok(_) => Ok(()),
                    // A peer that no longer knows the portal will never need the message.
                    Err(ureq::Error::Status(404 | 410, _)) => Err(status(
                        StatusCode::GONE,
                        "the peer does not know the portal",
                    )),
                    Err(ureq::Error::Status(_, _)) => {
                        Err(status(StatusCode::BAD_GATEWAY, "peer rejected the message"))
                    }
                    Err(_) => Err(unreachable_peer()),
                }
            })
            .await
            .map_err(|_| unreachable_peer())?
        })
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    const LINK_DOMAIN_TEST: &[u8] = b"ishtaria/portal-link/v1\0";
    const LINK_REQUEST_DOMAIN_TEST: &[u8] = b"ishtaria/portal-link-request/v1\0";

    #[test]
    fn peer_addresses_and_urls_are_validated() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public_ip(private.parse().unwrap()), "{private}");
        }
        for public in ["8.8.8.8", "93.184.216.34", "2606:4700::1111"] {
            assert!(is_public_ip(public.parse().unwrap()), "{public}");
        }
        assert!(
            valid_url("https://svet-a.example.org:7400") && valid_url("http://127.0.0.1:7400/api")
        );
        for bad in [
            "ftp://x",
            "https://",
            "https:///x",
            "https://user@host",
            "https://host?x=1",
            "https://host/#a",
            "https://ho st",
            "javascript:alert(1)",
        ] {
            assert!(!valid_url(bad), "{bad}");
        }
        assert!(
            valid_host("svet-a.example.org")
                && !valid_host("-a.example.org")
                && !valid_host("Upper.example.org")
                && !valid_host("")
        );
        assert!(
            valid_uuid("0192f3a1-5b1e-7c3a-9d4e-1a2b3c4d5e6f")
                && !valid_uuid("0192F3A1-5b1e-7c3a-9d4e-1a2b3c4d5e6f")
        );
    }

    #[test]
    fn world_names_are_bound_to_the_url_host() {
        assert_eq!(
            url_host("https://Svet-A.example.org:7400/x").as_deref(),
            Some("svet-a.example.org")
        );
        assert_eq!(
            url_host("http://svet-a.example.org.").as_deref(),
            Some("svet-a.example.org")
        );
        assert_eq!(url_host("http://[::1]:7400"), None);
        assert_eq!(url_host("ftp://svet-a.example.org"), None);
    }

    #[test]
    fn signatures_are_domain_separated_and_tamper_evident() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let token = sign(&key, LINK_DOMAIN_TEST, b"{\"a\":1}");
        let (payload, signature) = open_token(&token).unwrap();
        assert!(verify(&key.verifying_key(), LINK_DOMAIN_TEST, &payload, &signature).is_ok());
        assert!(verify(
            &key.verifying_key(),
            LINK_REQUEST_DOMAIN_TEST,
            &payload,
            &signature
        )
        .is_err());
        assert!(verify(
            &key.verifying_key(),
            LINK_DOMAIN_TEST,
            b"{\"a\":2}",
            &signature
        )
        .is_err());
        assert!(verify(
            &SigningKey::from_bytes(&[8u8; 32]).verifying_key(),
            LINK_DOMAIN_TEST,
            &payload,
            &signature
        )
        .is_err());
        assert!(open_token("a.b.c").is_err() && open_token("nodot").is_err());
        assert_eq!(
            parse_key(&format_key(&key.verifying_key().to_bytes())).unwrap(),
            key.verifying_key()
        );
        assert!(parse_key("ed25519:short").is_err() && parse_key("rsa:abc").is_err());
    }
}
