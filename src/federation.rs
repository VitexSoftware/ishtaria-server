//! Player-initiated portal pacts between worlds.
//!
//! A player invites a player of another world with a signed, single-use code.
//! The invited player's world verifies it against the inviter's pinned key and
//! sends a signed acceptance back. Each world keeps its own copy of the pact;
//! the operator policy (`closed`, `approve`, `open`) decides whether it is
//! accepted automatically or waits for approval. Messages are described in
//! `ishtaria-protocol` (`portal-invitation`, `portal-pact-accept`, `server-info`).

use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Extension, Json, Router,
};
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

const INVITE_PREFIX: &str = "ishtaria-invite:v1.";
const INVITE_DOMAIN: &[u8] = b"ishtaria/portal-invitation/v1\0";
const ACCEPT_DOMAIN: &[u8] = b"ishtaria/portal-pact-accept/v1\0";
const INVITE_TTL_SECONDS: i64 = 7 * 86_400;
const MAX_INVITE_SECONDS: i64 = 30 * 86_400;
pub(super) const ACCEPT_MAX_SECONDS: i64 = 600;
pub(super) const CLOCK_SKEW_SECONDS: i64 = 300;
const MAX_OPEN_INVITATIONS: i64 = 3;
const MAX_ACTIVE_PACTS: i64 = 5;
const MAX_CODE_LENGTH: usize = 2048;
pub(super) const MAX_PEER_REPLY: u64 = 16 * 1024;
pub(super) const NON_TERMINAL: &str = "('proposed', 'accepted', 'building', 'open')";

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
    fn post_accept(&self, api_url: String, message: String, allow_private: bool) -> Fut<()>;
    /// Tells the peer that an end of the portal is built or the pact is closed.
    fn post_status(&self, api_url: String, message: String, allow_private: bool) -> Fut<()>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvitationPayload {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    id: String,
    world: String,
    api_url: String,
    player: String,
    portal: String,
    issued: i64,
    expires: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptPayload {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    invitation: String,
    from_world: String,
    from_api_url: String,
    from_player: String,
    to_world: String,
    portal: String,
    issued: i64,
    expires: i64,
}

#[derive(FromRow)]
pub(super) struct Settings {
    pub public_url: Option<String>,
    pub policy: String,
    pub allow_private_peers: bool,
    pub signing_key: Vec<u8>,
    pub public_key: Vec<u8>,
}

#[derive(Serialize, FromRow)]
struct Pact {
    id: String,
    role: String,
    peer_host: String,
    peer_player: String,
    portal_name: String,
    peer_portal_name: Option<String>,
    state: String,
    created_at: i64,
}

#[derive(FromRow)]
struct InvitationRow {
    player_id: i64,
    portal_name: String,
    revoked: bool,
    expired: bool,
    host: Option<String>,
    player: Option<String>,
}

const PACT_COLUMNS: &str = "id::text AS id, role, peer_host, peer_player, portal_name, peer_portal_name, state, extract(epoch FROM created_at)::bigint AS created_at";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/ishtaria/server.json", get(server_info))
        .route("/portals/invitations", post(create_invitation))
        .route("/portals/invitations/{id}", delete(revoke_invitation))
        .route("/portals/pacts", get(list_pacts).post(accept_invitation))
        .route(
            "/federation/pacts",
            post(receive_acceptance).layer(DefaultBodyLimit::max(4096)),
        )
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

fn valid_portal(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_player(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 32 && !name.chars().any(char::is_control)
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

fn initial_state(policy: &str) -> &'static str {
    if policy == "open" {
        "accepted"
    } else {
        "proposed"
    }
}

/// Active pacts and portal names are shared limits for one player and one world.
async fn check_pact_capacity(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    state: &AppState,
    player_id: i64,
    portal_name: &str,
) -> Result<(), Error> {
    sqlx::query("SELECT id FROM players WHERE id = $1 FOR UPDATE")
        .bind(player_id)
        .execute(&mut **transaction)
        .await?;
    let active: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM portal_pacts WHERE player_id = $1 AND state IN {NON_TERMINAL}"
    ))
    .bind(player_id)
    .fetch_one(&mut **transaction)
    .await?;
    if active >= MAX_ACTIVE_PACTS {
        return Err(status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many active portal pacts",
        ));
    }
    let taken: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM portals WHERE world_id = $1 AND name = $2) OR EXISTS (SELECT 1 FROM portal_pacts WHERE world_id = $1 AND portal_name = $2 AND state IN {NON_TERMINAL})"
    ))
    .bind(state.world_id)
    .bind(portal_name)
    .fetch_one(&mut **transaction)
    .await?;
    if taken {
        return Err(status(StatusCode::CONFLICT, "portal name already in use"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewInvitation {
    portal_name: String,
}

#[derive(Serialize)]
struct InvitationCreated {
    id: String,
    code: String,
    expires_at: i64,
}

async fn create_invitation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NewInvitation>,
) -> Result<(StatusCode, Json<InvitationCreated>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !valid_portal(&body.portal_name) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid portal name"));
    }
    let settings = settings(&state).await?;
    if settings.policy == "closed" {
        return Err(status(StatusCode::FORBIDDEN, "federation is closed"));
    }
    let api_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::SERVICE_UNAVAILABLE, "federation not configured"))?;
    let key = signing_key(&settings)?;
    let mut transaction = state.pool.begin().await?;
    check_pact_capacity(&mut transaction, &state, player_id, &body.portal_name).await?;
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_invitations WHERE player_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now()")
        .bind(player_id)
        .fetch_one(&mut *transaction)
        .await?;
    if open >= MAX_OPEN_INVITATIONS {
        return Err(status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many open invitations",
        ));
    }
    let name_open: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM portal_invitations WHERE world_id = $1 AND portal_name = $2 AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now())")
        .bind(state.world_id)
        .bind(&body.portal_name)
        .fetch_one(&mut *transaction)
        .await?;
    if name_open {
        return Err(status(StatusCode::CONFLICT, "portal name already in use"));
    }
    let (id, issued, expires, username): (String, i64, i64, String) = sqlx::query_as(
        "WITH inserted AS (INSERT INTO portal_invitations (world_id, player_id, portal_name, expires_at) VALUES ($1, $2, $3, now() + make_interval(secs => $4)) RETURNING id, created_at, expires_at) SELECT inserted.id::text, extract(epoch FROM inserted.created_at)::bigint, extract(epoch FROM inserted.expires_at)::bigint, (SELECT username FROM players WHERE id = $2) FROM inserted",
    )
    .bind(state.world_id)
    .bind(player_id)
    .bind(&body.portal_name)
    .bind(INVITE_TTL_SECONDS as f64)
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let payload = InvitationPayload {
        v: 1,
        kind: "portal-invitation".into(),
        id: id.clone(),
        world: server_name(&state).await?,
        api_url,
        player: username,
        portal: body.portal_name,
        issued,
        expires,
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"))?;
    Ok((
        StatusCode::CREATED,
        Json(InvitationCreated {
            id,
            code: format!("{INVITE_PREFIX}{}", sign(&key, INVITE_DOMAIN, &bytes)),
            expires_at: expires,
        }),
    ))
}

async fn revoke_invitation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !valid_uuid(&id) {
        return Err(status(StatusCode::NOT_FOUND, "invitation not found"));
    }
    let revoked = sqlx::query("UPDATE portal_invitations SET revoked_at = now() WHERE id = $1::uuid AND player_id = $2 AND world_id = $3 AND accepted_at IS NULL AND revoked_at IS NULL")
        .bind(&id)
        .bind(player_id)
        .bind(state.world_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if revoked == 0 {
        return Err(status(StatusCode::NOT_FOUND, "invitation not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_pacts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Pact>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    Ok(Json(
        sqlx::query_as(&format!("SELECT {PACT_COLUMNS} FROM portal_pacts WHERE player_id = $1 AND world_id = $2 ORDER BY created_at DESC LIMIT 50"))
            .bind(player_id)
            .bind(state.world_id)
            .fetch_all(&state.pool)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    code: String,
    portal_name: String,
}

async fn find_pact(state: &AppState, invitation_id: &str) -> Result<Option<(i64, Pact)>, Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT player_id FROM portal_pacts WHERE world_id = $1 AND invitation_id = $2::uuid",
    )
    .bind(state.world_id)
    .bind(invitation_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((player_id,)) = row else {
        return Ok(None);
    };
    let pact = sqlx::query_as(&format!(
        "SELECT {PACT_COLUMNS} FROM portal_pacts WHERE world_id = $1 AND invitation_id = $2::uuid"
    ))
    .bind(state.world_id)
    .bind(invitation_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(Some((player_id, pact)))
}

async fn accept_invitation(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    headers: HeaderMap,
    Json(body): Json<Acceptance>,
) -> Result<(StatusCode, Json<Pact>), Error> {
    let directory = directory(peers);
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !valid_portal(&body.portal_name) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid portal name"));
    }
    let settings = settings(&state).await?;
    if settings.policy == "closed" {
        return Err(status(StatusCode::FORBIDDEN, "federation is closed"));
    }
    let own_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::SERVICE_UNAVAILABLE, "federation not configured"))?;
    let token = body
        .code
        .trim()
        .strip_prefix(INVITE_PREFIX)
        .filter(|token| token.len() <= MAX_CODE_LENGTH)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid invitation code"))?;
    let (bytes, signature) = open_token(token)?;
    let invitation: InvitationPayload = serde_json::from_slice(&bytes)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid invitation"))?;
    let current = now();
    if invitation.v != 1
        || invitation.kind != "portal-invitation"
        || !valid_uuid(&invitation.id)
        || !valid_host(&invitation.world)
        || !valid_url(&invitation.api_url)
        || !valid_player(&invitation.player)
        || !valid_portal(&invitation.portal)
        || invitation.expires <= invitation.issued
        || invitation.expires - invitation.issued > MAX_INVITE_SECONDS
        || invitation.issued > current + CLOCK_SKEW_SECONDS
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid invitation"));
    }
    let own_name = server_name(&state).await?;
    if invitation.world == own_name {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "cannot accept an invitation from the same world",
        ));
    }
    if let Some((owner, pact)) = find_pact(&state, &invitation.id).await? {
        return if owner == player_id {
            Ok((StatusCode::OK, Json(pact)))
        } else {
            Err(status(StatusCode::CONFLICT, "invitation already used"))
        };
    }
    if invitation.expires <= current {
        return Err(status(StatusCode::GONE, "invitation expired"));
    }
    let peer = peer_key(
        &state,
        &settings,
        directory.as_ref(),
        &invitation.world,
        &invitation.api_url,
    )
    .await?;
    verify(&peer.key, INVITE_DOMAIN, &bytes, &signature)?;
    pin_peer(&state, &invitation.world, &peer).await?;
    let username: String = sqlx::query_scalar("SELECT username FROM players WHERE id = $1")
        .bind(player_id)
        .fetch_one(&state.pool)
        .await?;
    {
        let mut transaction = state.pool.begin().await?;
        check_pact_capacity(&mut transaction, &state, player_id, &body.portal_name).await?;
        transaction.rollback().await?;
    }
    let accept = AcceptPayload {
        v: 1,
        kind: "portal-pact-accept".into(),
        invitation: invitation.id.clone(),
        from_world: own_name,
        from_api_url: own_url,
        from_player: username,
        to_world: invitation.world.clone(),
        portal: body.portal_name.clone(),
        issued: current,
        expires: current + 300,
    };
    let encoded = serde_json::to_vec(&accept)
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"))?;
    let message = sign(&signing_key(&settings)?, ACCEPT_DOMAIN, &encoded);
    directory
        .post_accept(
            invitation.api_url.clone(),
            message,
            settings.allow_private_peers,
        )
        .await?;
    // The peer's acceptance is idempotent, so a lost race or failed insert can be retried safely.
    let inserted: Option<Pact> = sqlx::query_as(&format!(
        "INSERT INTO portal_pacts (world_id, invitation_id, role, player_id, peer_host, peer_player, portal_name, peer_portal_name, state) VALUES ($1, $2::uuid, 'invitee', $3, $4, $5, $6, $7, $8) ON CONFLICT (world_id, invitation_id) DO NOTHING RETURNING {PACT_COLUMNS}"
    ))
    .bind(state.world_id)
    .bind(&invitation.id)
    .bind(player_id)
    .bind(&invitation.world)
    .bind(&invitation.player)
    .bind(&body.portal_name)
    .bind(&invitation.portal)
    .bind(initial_state(&settings.policy))
    .fetch_optional(&state.pool)
    .await?;
    match inserted {
        Some(pact) => Ok((StatusCode::CREATED, Json(pact))),
        None => match find_pact(&state, &invitation.id).await? {
            Some((owner, pact)) if owner == player_id => Ok((StatusCode::OK, Json(pact))),
            _ => Err(status(StatusCode::CONFLICT, "invitation already used")),
        },
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    message: String,
}

#[derive(Serialize)]
struct Received {
    pact: String,
    state: String,
}

/// Server-to-server: a peer world reports that its player accepted our player's invitation.
async fn receive_acceptance(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    Json(body): Json<Message>,
) -> Result<(StatusCode, Json<Received>), Error> {
    let directory = directory(peers);
    let settings = settings(&state).await?;
    if settings.policy == "closed" {
        return Err(status(StatusCode::FORBIDDEN, "federation is closed"));
    }
    let (bytes, signature) = open_token(&body.message)?;
    let accept: AcceptPayload = serde_json::from_slice(&bytes)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid acceptance"))?;
    let current = now();
    let own_name = server_name(&state).await?;
    if accept.v != 1
        || accept.kind != "portal-pact-accept"
        || !valid_uuid(&accept.invitation)
        || !valid_host(&accept.from_world)
        || !valid_url(&accept.from_api_url)
        || !valid_player(&accept.from_player)
        || !valid_portal(&accept.portal)
        || accept.to_world != own_name
        || accept.from_world == own_name
        || accept.expires <= accept.issued
        || accept.expires - accept.issued > ACCEPT_MAX_SECONDS
        || accept.issued > current + CLOCK_SKEW_SECONDS
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid acceptance"));
    }
    if accept.expires <= current {
        return Err(status(StatusCode::GONE, "acceptance expired"));
    }
    let peer = peer_key(
        &state,
        &settings,
        directory.as_ref(),
        &accept.from_world,
        &accept.from_api_url,
    )
    .await?;
    verify(&peer.key, ACCEPT_DOMAIN, &bytes, &signature)?;
    pin_peer(&state, &accept.from_world, &peer).await?;

    let mut transaction = state.pool.begin().await?;
    let invitation: Option<InvitationRow> = sqlx::query_as(
        "SELECT player_id, portal_name, revoked_at IS NOT NULL AS revoked, expires_at <= now() AS expired, accepted_by_host AS host, accepted_by_player AS player FROM portal_invitations WHERE id = $1::uuid AND world_id = $2 FOR UPDATE",
    )
    .bind(&accept.invitation)
    .bind(state.world_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(InvitationRow {
        player_id,
        portal_name,
        revoked,
        expired,
        host,
        player,
    }) = invitation
    else {
        return Err(status(StatusCode::NOT_FOUND, "invitation not found"));
    };
    if let (Some(host), Some(player)) = (host, player) {
        drop(transaction);
        return match find_pact(&state, &accept.invitation).await? {
            Some((_, pact)) if host == accept.from_world && player == accept.from_player => Ok((
                StatusCode::OK,
                Json(Received {
                    pact: pact.id,
                    state: pact.state,
                }),
            )),
            _ => Err(status(StatusCode::CONFLICT, "invitation already used")),
        };
    }
    if revoked || expired {
        return Err(status(StatusCode::GONE, "invitation no longer valid"));
    }
    let available: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM players WHERE id = $1 AND died_at IS NULL AND banned_at IS NULL)")
        .bind(player_id)
        .fetch_one(&mut *transaction)
        .await?;
    if !available {
        return Err(status(StatusCode::GONE, "inviting player unavailable"));
    }
    // The open invitation reserved the portal name, so only the player's quota is checked here.
    sqlx::query("SELECT id FROM players WHERE id = $1 FOR UPDATE")
        .bind(player_id)
        .execute(&mut *transaction)
        .await?;
    let active: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM portal_pacts WHERE player_id = $1 AND state IN {NON_TERMINAL}"
    ))
    .bind(player_id)
    .fetch_one(&mut *transaction)
    .await?;
    if active >= MAX_ACTIVE_PACTS {
        return Err(status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many active portal pacts",
        ));
    }
    sqlx::query("UPDATE portal_invitations SET accepted_at = now(), accepted_by_host = $2, accepted_by_player = $3 WHERE id = $1::uuid")
        .bind(&accept.invitation)
        .bind(&accept.from_world)
        .bind(&accept.from_player)
        .execute(&mut *transaction)
        .await?;
    let pact: Pact = sqlx::query_as(&format!(
        "INSERT INTO portal_pacts (world_id, invitation_id, role, player_id, peer_host, peer_player, portal_name, peer_portal_name, state) VALUES ($1, $2::uuid, 'inviter', $3, $4, $5, $6, $7, $8) RETURNING {PACT_COLUMNS}"
    ))
    .bind(state.world_id)
    .bind(&accept.invitation)
    .bind(player_id)
    .bind(&accept.from_world)
    .bind(&accept.from_player)
    .bind(portal_name)
    .bind(&accept.portal)
    .bind(initial_state(&settings.policy))
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Received {
            pact: pact.id,
            state: pact.state,
        }),
    ))
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

    fn post_accept(&self, api_url: String, message: String, allow_private: bool) -> Fut<()> {
        Box::pin(async move {
            if !valid_url(&api_url) {
                return Err(status(StatusCode::BAD_REQUEST, "invalid peer address"));
            }
            let url = format!("{}/federation/pacts", api_url.trim_end_matches('/'));
            let body = serde_json::json!({ "message": message }).to_string();
            let _permit = OUTBOUND.acquire().await.map_err(|_| unreachable_peer())?;
            tokio::task::spawn_blocking(move || {
                match agent(allow_private)
                    .post(&url)
                    .set("Content-Type", "application/json")
                    .send_string(&body)
                {
                    Ok(_) => Ok(()),
                    Err(ureq::Error::Status(409, _)) => {
                        Err(status(StatusCode::CONFLICT, "invitation already used"))
                    }
                    Err(ureq::Error::Status(404 | 410, _)) => {
                        Err(status(StatusCode::GONE, "invitation no longer valid"))
                    }
                    Err(ureq::Error::Status(403, _)) => {
                        Err(status(StatusCode::FORBIDDEN, "peer refused the pact"))
                    }
                    Err(ureq::Error::Status(429, _)) => {
                        Err(status(StatusCode::TOO_MANY_REQUESTS, "peer limit reached"))
                    }
                    Err(ureq::Error::Status(_, _)) => {
                        Err(status(StatusCode::BAD_GATEWAY, "peer rejected the pact"))
                    }
                    Err(_) => Err(unreachable_peer()),
                }
            })
            .await
            .map_err(|_| unreachable_peer())?
        })
    }

    fn post_status(&self, api_url: String, message: String, allow_private: bool) -> Fut<()> {
        Box::pin(async move {
            if !valid_url(&api_url) {
                return Err(status(StatusCode::BAD_REQUEST, "invalid peer address"));
            }
            let url = format!("{}/federation/pacts/status", api_url.trim_end_matches('/'));
            let body = serde_json::json!({ "message": message }).to_string();
            let _permit = OUTBOUND.acquire().await.map_err(|_| unreachable_peer())?;
            tokio::task::spawn_blocking(move || {
                match agent(allow_private)
                    .post(&url)
                    .set("Content-Type", "application/json")
                    .send_string(&body)
                {
                    Ok(_) => Ok(()),
                    Err(ureq::Error::Status(404 | 410, _)) => {
                        Err(status(StatusCode::GONE, "pact no longer known to the peer"))
                    }
                    Err(ureq::Error::Status(_, _)) => {
                        Err(status(StatusCode::BAD_GATEWAY, "peer rejected the status"))
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
        let token = sign(&key, INVITE_DOMAIN, b"{\"a\":1}");
        let (payload, signature) = open_token(&token).unwrap();
        assert!(verify(&key.verifying_key(), INVITE_DOMAIN, &payload, &signature).is_ok());
        assert!(verify(&key.verifying_key(), ACCEPT_DOMAIN, &payload, &signature).is_err());
        assert!(verify(
            &key.verifying_key(),
            INVITE_DOMAIN,
            b"{\"a\":2}",
            &signature
        )
        .is_err());
        assert!(verify(
            &SigningKey::from_bytes(&[8u8; 32]).verifying_key(),
            INVITE_DOMAIN,
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
