//! Building a portal and linking it with a portal of another world.
//!
//! A player builds a portal on their own: a construction site where they stand, and the
//! materials listed in `etc/portal.json` delivered to it. A finished portal offers two things:
//! a **share link** to copy, and a place to **paste the share link of another world's portal**.
//! When a link is pasted, the server first checks that the other world is reachable and that
//! the offered portal stands there and is complete; only then are the portals linked. The
//! operator policy (`closed`, `approve`, `open`) decides whether a link opens at once. Closing
//! a portal leaves a ruin that keeps the record of the materials delivered to it.

use super::federation::{
    self, directory, now, open_token, peer_key, pin_peer, server_name, settings, sign, signing_key,
    status, valid_uuid, verify, PeerDirectory, PortalInfo, CLOCK_SKEW_SECONDS,
    LINK_REQUEST_MAX_SECONDS,
};
use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::{Arc, OnceLock};

const LINK_PREFIX: &str = "ishtaria-portal:v1.";
const LINK_DOMAIN: &[u8] = b"ishtaria/portal-link/v1\0";
const LINK_REQUEST_DOMAIN: &[u8] = b"ishtaria/portal-link-request/v1\0";
const UNLINK_DOMAIN: &[u8] = b"ishtaria/portal-unlink/v1\0";
const MAX_LINK_LENGTH: usize = 2048;
const MAX_PORTALS_PER_PLAYER: i64 = 5;
const PORTAL_REGION_M: f64 = 600.0;

#[derive(Deserialize)]
struct Requirement {
    id: String,
    items: Vec<String>,
    quantity: i64,
}

#[derive(Deserialize)]
struct Requirements {
    version: u32,
    reach_m: f64,
    min_distance_m: f64,
    requirements: Vec<Requirement>,
}

fn requirements() -> &'static Requirements {
    static CONFIG: OnceLock<Requirements> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Requirements = serde_json::from_str(include_str!("../etc/portal.json"))
            .expect("bundled portal requirements must be valid");
        assert_eq!(config.version, 1);
        config
    })
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/portals/build", post(build))
        .route("/portals/mine", get(list))
        .route("/portals/mine/{id}", get(details).delete(close))
        .route("/portals/mine/{id}/contribute", post(contribute))
        .route("/portals/mine/{id}/connect", post(connect))
        .route(
            "/portals/mine/{id}/link",
            get(share_link).delete(disconnect),
        )
        .route("/federation/portals/{id}", get(public_info))
        .route(
            "/federation/portals/unlink",
            post(receive_unlink).layer(DefaultBodyLimit::max(4096)),
        )
        .route(
            "/federation/portals/link",
            post(receive_link).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/world/portals", get(world_portals))
}

#[derive(Serialize, FromRow)]
struct PortalRow {
    id: String,
    state: String,
    portal_name: String,
    peer_host: Option<String>,
    peer_portal_name: Option<String>,
    site_x: Option<f64>,
    site_y: Option<f64>,
    site_z: Option<f64>,
}

const PORTAL_ROW: &str =
    "id::text AS id, state, portal_name, peer_host, peer_portal_name, site_x, site_y, site_z";

#[derive(Serialize)]
struct Progress {
    id: String,
    items: Vec<String>,
    required: String,
    contributed: String,
}

#[derive(Serialize)]
struct Details {
    id: String,
    /// `building`, `built` (complete; share or paste a link), `pending` (linked, waiting for
    /// the operator), `open` or `closed`.
    state: String,
    portal_name: String,
    /// The world and portal this one is linked with, once linked.
    peer_host: Option<String>,
    peer_portal_name: Option<String>,
    site: Option<[f64; 3]>,
    requirements: Vec<Progress>,
}

/// How much of each requirement has been delivered, in configuration order.
async fn contributed(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    portal_id: &str,
) -> Result<Vec<i64>, Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT item_id, quantity FROM portal_contributions WHERE pact_id = $1::uuid",
    )
    .bind(portal_id)
    .fetch_all(&mut **transaction)
    .await?;
    Ok(requirements()
        .requirements
        .iter()
        .map(|requirement| {
            rows.iter()
                .filter(|(item, _)| requirement.items.contains(item))
                .map(|(_, quantity)| *quantity)
                .sum::<i64>()
                .min(requirement.quantity)
        })
        .collect())
}

async fn own_portal(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    state: &AppState,
    player_id: i64,
    id: &str,
) -> Result<PortalRow, Error> {
    if !valid_uuid(id) {
        return Err(status(StatusCode::NOT_FOUND, "portal not found"));
    }
    sqlx::query_as(&format!(
        "SELECT {PORTAL_ROW} FROM portal_pacts WHERE id = $1::uuid AND world_id = $2 AND player_id = $3 AND role = 'builder' FOR UPDATE"
    ))
    .bind(id)
    .bind(state.world_id)
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| status(StatusCode::NOT_FOUND, "portal not found"))
}

async fn describe(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    portal: PortalRow,
) -> Result<Details, Error> {
    let delivered = contributed(transaction, &portal.id).await?;
    let site = match (portal.site_x, portal.site_y, portal.site_z) {
        (Some(x), Some(y), Some(z)) => Some([x, y, z]),
        _ => None,
    };
    Ok(Details {
        requirements: requirements()
            .requirements
            .iter()
            .zip(delivered)
            .map(|(requirement, delivered)| Progress {
                id: requirement.id.clone(),
                items: requirement.items.clone(),
                required: requirement.quantity.to_string(),
                contributed: delivered.to_string(),
            })
            .collect(),
        id: portal.id,
        state: portal.state,
        portal_name: portal.portal_name,
        peer_host: portal.peer_host,
        peer_portal_name: portal.peer_portal_name,
        site,
    })
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<PortalRow>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    Ok(Json(
        sqlx::query_as(&format!(
            "SELECT {PORTAL_ROW} FROM portal_pacts WHERE world_id = $1 AND player_id = $2 AND role = 'builder' ORDER BY created_at DESC LIMIT 50"
        ))
        .bind(state.world_id)
        .bind(player_id)
        .fetch_all(&state.pool)
        .await?,
    ))
}

async fn details(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Details>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, portal).await?;
    transaction.commit().await?;
    Ok(Json(described))
}

fn distance(first: [f64; 3], second: [f64; 3]) -> f64 {
    (0..3)
        .map(|axis| (first[axis] - second[axis]).powi(2))
        .sum::<f64>()
        .sqrt()
}

async fn own_position(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<[f64; 3], Error> {
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await?;
    position
        .map(|(x, y, z)| [x, y, z])
        .ok_or_else(|| status(StatusCode::CONFLICT, "player position unavailable"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Build {
    portal_name: String,
}

/// Starts a portal: a construction site where the player stands.
async fn build(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Build>,
) -> Result<(StatusCode, Json<Details>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !federation::valid_portal(&input.portal_name) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid portal name"));
    }
    let terrain = super::movement::terrain(&state).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_pacts WHERE world_id = $1 AND player_id = $2 AND role = 'builder' AND state <> 'closed'")
        .bind(state.world_id).bind(player_id).fetch_one(&mut *transaction).await?;
    if active >= MAX_PORTALS_PER_PLAYER {
        return Err(status(StatusCode::TOO_MANY_REQUESTS, "too many portals"));
    }
    let position = own_position(&mut transaction, player_id).await?;
    super::land::require_tenant(&mut transaction, state.world_id, player_id, position).await?;
    let (face, x, y) = terrain
        .build_site(position)
        .ok_or_else(|| status(StatusCode::CONFLICT, "a portal cannot be built here"))?;
    let near: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM portal_pacts WHERE world_id = $1 AND site_x IS NOT NULL AND state NOT IN ('closed', 'declined', 'expired', 'banned') AND sqrt((site_x - $2)^2 + (site_y - $3)^2 + (site_z - $4)^2) < $5)")
        .bind(state.world_id).bind(position[0]).bind(position[1]).bind(position[2]).bind(requirements().min_distance_m)
        .fetch_one(&mut *transaction).await?;
    if near {
        return Err(status(StatusCode::CONFLICT, "another portal is too close"));
    }
    let portal_id: i64 = sqlx::query_scalar("INSERT INTO portals (world_id, name, state, face, x, y) VALUES ($1, $2, 'building', $3, $4, $5) ON CONFLICT DO NOTHING RETURNING id")
        .bind(state.world_id).bind(&input.portal_name).bind(face).bind(x).bind(y)
        .fetch_optional(&mut *transaction).await?
        .ok_or_else(|| status(StatusCode::CONFLICT, "portal name already in use"))?;
    let id: String = sqlx::query_scalar("INSERT INTO portal_pacts (world_id, role, player_id, portal_name, state, site_x, site_y, site_z, portal_id) VALUES ($1, 'builder', $2, $3, 'building', $4, $5, $6, $7) RETURNING id::text")
        .bind(state.world_id).bind(player_id).bind(&input.portal_name)
        .bind(position[0]).bind(position[1]).bind(position[2]).bind(portal_id)
        .fetch_one(&mut *transaction).await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, portal).await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(described)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contribution {
    item_id: String,
    quantity: String,
}

/// Delivers materials from the inventory to the construction site.
async fn contribute(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Contribution>,
) -> Result<Json<Details>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let amount = input
        .quantity
        .parse::<i64>()
        .ok()
        .filter(|quantity| *quantity > 0)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid quantity"))?;
    let config = requirements();
    let index = config
        .requirements
        .iter()
        .position(|requirement| requirement.items.contains(&input.item_id))
        .ok_or_else(|| {
            status(
                StatusCode::BAD_REQUEST,
                "the portal does not need this item",
            )
        })?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    let (Some(x), Some(y), Some(z)) = (portal.site_x, portal.site_y, portal.site_z) else {
        return Err(status(
            StatusCode::CONFLICT,
            "there is no construction site",
        ));
    };
    if portal.state != "building" {
        return Err(status(
            StatusCode::CONFLICT,
            "the portal is not under construction",
        ));
    }
    if distance(own_position(&mut transaction, player_id).await?, [x, y, z]) > config.reach_m {
        return Err(status(
            StatusCode::FORBIDDEN,
            "the construction site is out of reach",
        ));
    }
    let delivered = contributed(&mut transaction, &portal.id).await?;
    if amount > config.requirements[index].quantity - delivered[index] {
        return Err(status(
            StatusCode::CONFLICT,
            "more than the portal still needs",
        ));
    }
    let owned: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2 FOR UPDATE",
    )
    .bind(player_id)
    .bind(&input.item_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let owned = owned.unwrap_or(0);
    if owned < amount {
        return Err(status(StatusCode::CONFLICT, "not enough items"));
    }
    if owned == amount {
        sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
            .bind(player_id)
            .bind(&input.item_id)
            .execute(&mut *transaction)
            .await?;
    } else {
        sqlx::query("UPDATE player_inventory SET quantity = quantity - $3 WHERE player_id = $1 AND item_id = $2")
            .bind(player_id).bind(&input.item_id).bind(amount).execute(&mut *transaction).await?;
    }
    sqlx::query("INSERT INTO portal_contributions (pact_id, item_id, quantity) VALUES ($1::uuid, $2, $3) ON CONFLICT (pact_id, item_id) DO UPDATE SET quantity = portal_contributions.quantity + EXCLUDED.quantity")
        .bind(&portal.id).bind(&input.item_id).bind(amount).execute(&mut *transaction).await?;
    super::experience::grant(
        &mut transaction,
        player_id,
        amount * super::experience::BUILD_UNIT_XP,
    )
    .await?;
    let delivered = contributed(&mut transaction, &portal.id).await?;
    let complete = config
        .requirements
        .iter()
        .zip(&delivered)
        .all(|(requirement, delivered)| *delivered >= requirement.quantity);
    if complete {
        super::experience::grant(&mut transaction, player_id, super::experience::BUILD_END_XP)
            .await?;
        sqlx::query("UPDATE portal_pacts SET state = 'built', local_built_at = now(), updated_at = now() WHERE id = $1::uuid")
            .bind(&portal.id)
            .execute(&mut *transaction)
            .await?;
    }
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, portal).await?;
    transaction.commit().await?;
    Ok(Json(described))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkPayload {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    portal: String,
    name: String,
    world: String,
    api_url: String,
    issued: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkRequest {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    from_world: String,
    from_api_url: String,
    from_portal: String,
    from_name: String,
    to_world: String,
    to_portal: String,
    issued: i64,
    expires: i64,
}

#[derive(Serialize)]
struct ShareLink {
    link: String,
}

/// The share link of a finished portal, to be pasted at a portal of another world.
async fn share_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ShareLink>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    transaction.commit().await?;
    if portal.state != "built" {
        return Err(status(
            StatusCode::CONFLICT,
            "only a finished, unlinked portal can be shared",
        ));
    }
    let settings = settings(&state).await?;
    let api_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::CONFLICT, "federation not configured"))?;
    let payload = LinkPayload {
        v: 1,
        kind: "portal-link".into(),
        portal: portal.id,
        name: portal.portal_name,
        world: server_name(&state).await?,
        api_url,
        issued: now(),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"))?;
    Ok(Json(ShareLink {
        link: format!(
            "{LINK_PREFIX}{}",
            sign(&signing_key(&settings)?, LINK_DOMAIN, &bytes)
        ),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    link: String,
}

/// Public: what this world says about one of its portals (it stands and how far it is).
async fn public_info(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PortalInfo>, Error> {
    if !valid_uuid(&id) {
        return Err(status(StatusCode::NOT_FOUND, "portal not found"));
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT portal_name, state FROM portal_pacts WHERE id = $1::uuid AND world_id = $2 AND role = 'builder'",
    )
    .bind(&id)
    .bind(state.world_id)
    .fetch_optional(&state.pool)
    .await?;
    let (name, portal_state) =
        row.ok_or_else(|| status(StatusCode::NOT_FOUND, "portal not found"))?;
    Ok(Json(PortalInfo {
        world: server_name(&state).await?,
        portal: id,
        name,
        state: portal_state,
    }))
}

/// Pastes a share link at a finished portal of this world. The other world must be reachable
/// and the offered portal must stand there and be complete; then both ends are linked.
async fn connect(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Connect>,
) -> Result<Json<Details>, Error> {
    let directory = directory(peers);
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    transaction.commit().await?;
    if portal.state != "built" {
        return Err(status(
            StatusCode::CONFLICT,
            "only a finished, unlinked portal can be connected",
        ));
    }
    let settings = settings(&state).await?;
    if settings.policy == "closed" {
        return Err(status(
            StatusCode::FORBIDDEN,
            "this world does not link portals",
        ));
    }
    let own_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::CONFLICT, "federation not configured"))?;
    // 1. The link must be well formed and signed by the world it names.
    let token = input
        .link
        .trim()
        .strip_prefix(LINK_PREFIX)
        .filter(|token| token.len() <= MAX_LINK_LENGTH)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "this is not a portal share link"))?;
    let (bytes, signature) = open_token(token)?;
    let link: LinkPayload = serde_json::from_slice(&bytes)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "this is not a portal share link"))?;
    let own_name = server_name(&state).await?;
    if link.v != 1
        || link.kind != "portal-link"
        || !valid_uuid(&link.portal)
        || !federation::valid_host(&link.world)
        || !federation::valid_url(&link.api_url)
        || !federation::valid_portal(&link.name)
        || link.world == own_name
    {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "this is not a portal share link",
        ));
    }
    let peer = peer_key(
        &state,
        &settings,
        directory.as_ref(),
        &link.world,
        &link.api_url,
    )
    .await?;
    verify(&peer.key, LINK_DOMAIN, &bytes, &signature)?;
    // 2. The other world answers, and the portal stands there complete.
    let info = directory
        .portal_info(
            link.api_url.clone(),
            link.portal.clone(),
            settings.allow_private_peers,
        )
        .await?;
    if info.world != link.world || info.portal != link.portal {
        return Err(status(
            StatusCode::CONFLICT,
            "the portal does not stand there",
        ));
    }
    if info.state != "built" {
        return Err(status(
            StatusCode::CONFLICT,
            match info.state.as_str() {
                "building" => "the other portal is not finished",
                "open" | "pending" => "the other portal is already linked",
                _ => "the other portal is closed",
            },
        ));
    }
    pin_peer(&state, &link.world, &peer).await?;
    // 3. The other world links its end; it checks this portal in turn.
    let current = now();
    let request = LinkRequest {
        v: 1,
        kind: "portal-link-request".into(),
        from_world: own_name,
        from_api_url: own_url,
        from_portal: portal.id.clone(),
        from_name: portal.portal_name.clone(),
        to_world: link.world.clone(),
        to_portal: link.portal.clone(),
        issued: current,
        expires: current + 300,
    };
    let request_bytes = serde_json::to_vec(&request)
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"))?;
    let message = sign(
        &signing_key(&settings)?,
        LINK_REQUEST_DOMAIN,
        &request_bytes,
    );
    let peer_state = directory
        .post_link(link.api_url.clone(), message, settings.allow_private_peers)
        .await?;
    // 4. Our end follows the operator policy and the answer of the other end.
    let opens = settings.policy == "open" && peer_state == "open";
    let mut transaction = state.pool.begin().await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    if portal.state != "built" {
        return Err(status(
            StatusCode::CONFLICT,
            "the portal was linked meanwhile",
        ));
    }
    link_end(
        &mut transaction,
        &portal.id,
        &link.world,
        &link.portal,
        &link.name,
        opens,
    )
    .await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, portal).await?;
    transaction.commit().await?;
    Ok(Json(described))
}

/// Records the other end of a link and opens the portal when the policy allows it.
async fn link_end(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    portal_id: &str,
    peer_host: &str,
    peer_portal: &str,
    peer_name: &str,
    opens: bool,
) -> Result<(), Error> {
    let new_state = if opens { "open" } else { "pending" };
    let row: Option<Option<i64>> = sqlx::query_scalar("UPDATE portal_pacts SET state = $2, peer_host = $3, peer_portal_id = $4::uuid, peer_portal_name = $5, updated_at = now() WHERE id = $1::uuid AND state = 'built' RETURNING portal_id")
        .bind(portal_id).bind(new_state).bind(peer_host).bind(peer_portal).bind(peer_name)
        .fetch_optional(&mut **transaction).await?;
    if let Some(Some(id)) = row {
        sqlx::query("UPDATE portals SET state = $2, peer = $3, updated_at = now() WHERE id = $1 AND state IN ('building', 'open')")
            .bind(id)
            .bind(if opens { "open" } else { "building" })
            .bind(peer_host)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Message {
    message: String,
}

#[derive(Serialize)]
struct LinkReceived {
    state: String,
}

/// Server to server: another world asks this world to link one of its finished portals.
async fn receive_link(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    Json(body): Json<Message>,
) -> Result<Json<LinkReceived>, Error> {
    let directory = directory(peers);
    let settings = settings(&state).await?;
    if settings.policy == "closed" {
        return Err(status(
            StatusCode::FORBIDDEN,
            "this world does not link portals",
        ));
    }
    let (bytes, signature) = open_token(&body.message)?;
    let request: LinkRequest = serde_json::from_slice(&bytes)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid link request"))?;
    let current = now();
    let own_name = server_name(&state).await?;
    if request.v != 1
        || request.kind != "portal-link-request"
        || !valid_uuid(&request.from_portal)
        || !valid_uuid(&request.to_portal)
        || !federation::valid_host(&request.from_world)
        || !federation::valid_url(&request.from_api_url)
        || !federation::valid_portal(&request.from_name)
        || request.to_world != own_name
        || request.from_world == own_name
        || request.expires <= request.issued
        || request.expires - request.issued > LINK_REQUEST_MAX_SECONDS
        || request.issued > current + CLOCK_SKEW_SECONDS
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid link request"));
    }
    if request.expires <= current {
        return Err(status(StatusCode::GONE, "link request expired"));
    }
    let peer = peer_key(
        &state,
        &settings,
        directory.as_ref(),
        &request.from_world,
        &request.from_api_url,
    )
    .await?;
    verify(&peer.key, LINK_REQUEST_DOMAIN, &bytes, &signature)?;
    pin_peer(&state, &request.from_world, &peer).await?;
    // The requester's portal must really stand there and be complete.
    let info = directory
        .portal_info(
            request.from_api_url.clone(),
            request.from_portal.clone(),
            settings.allow_private_peers,
        )
        .await?;
    if info.world != request.from_world || info.portal != request.from_portal {
        return Err(status(
            StatusCode::CONFLICT,
            "the portal does not stand there",
        ));
    }
    let mut transaction = state.pool.begin().await?;
    let own: Option<(String, Option<String>, Option<String>)> = sqlx::query_as("SELECT state, peer_host, peer_portal_id::text FROM portal_pacts WHERE id = $1::uuid AND world_id = $2 AND role = 'builder' FOR UPDATE")
        .bind(&request.to_portal).bind(state.world_id)
        .fetch_optional(&mut *transaction).await?;
    let Some((own_state, linked_host, linked_portal)) = own else {
        return Err(status(StatusCode::NOT_FOUND, "portal not found"));
    };
    // Asking again after a link was made is answered with the state we are in.
    if matches!(own_state.as_str(), "open" | "pending")
        && linked_host.as_deref() == Some(request.from_world.as_str())
        && linked_portal.as_deref() == Some(request.from_portal.as_str())
    {
        return Ok(Json(LinkReceived { state: own_state }));
    }
    if own_state != "built" {
        return Err(status(
            StatusCode::CONFLICT,
            "the portal is not ready to be linked",
        ));
    }
    if !matches!(info.state.as_str(), "built" | "open" | "pending") {
        return Err(status(
            StatusCode::CONFLICT,
            "the requesting portal is not finished",
        ));
    }
    let opens = settings.policy == "open";
    link_end(
        &mut transaction,
        &request.to_portal,
        &request.from_world,
        &request.from_portal,
        &request.from_name,
        opens,
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(LinkReceived {
        state: if opens { "open" } else { "pending" }.to_owned(),
    }))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnlinkRequest {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    from_world: String,
    from_api_url: String,
    from_portal: String,
    to_world: String,
    to_portal: String,
    issued: i64,
    expires: i64,
}

/// Returns a linked portal to the finished, unlinked state and remembers that the other world
/// has to be told. Returns the other end, if there was one.
async fn break_link(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    portal_id: &str,
    new_state: &str,
) -> Result<Option<(String, String)>, Error> {
    let row: Option<(Option<i64>, Option<String>, Option<String>)> = sqlx::query_as("SELECT portal_id, peer_host, peer_portal_id::text FROM portal_pacts WHERE id = $1::uuid AND state IN ('open', 'pending') FOR UPDATE")
        .bind(portal_id).fetch_optional(&mut **transaction).await?;
    let Some((record, Some(host), Some(peer_portal))) = row else {
        return Ok(None);
    };
    sqlx::query("UPDATE portal_pacts SET state = $2, peer_host = NULL, peer_portal_id = NULL, peer_portal_name = NULL, updated_at = now() WHERE id = $1::uuid")
        .bind(portal_id).bind(new_state).execute(&mut **transaction).await?;
    if let Some(record) = record {
        sqlx::query("UPDATE portals SET state = $2, peer = NULL, updated_at = now() WHERE id = $1 AND state IN ('building', 'open')")
            .bind(record)
            .bind(if new_state == "closed" { "closed" } else { "building" })
            .execute(&mut **transaction)
            .await?;
    }
    sqlx::query("INSERT INTO portal_unlinks (world_id, own_portal_id, peer_host, peer_portal_id) VALUES ($1, $2::uuid, $3, $4::uuid)")
        .bind(world_id).bind(portal_id).bind(&host).bind(&peer_portal)
        .execute(&mut **transaction).await?;
    Ok(Some((host, peer_portal)))
}

/// Tells the other world that links were broken; what cannot be delivered now is repeated.
async fn notify_unlinks(state: &AppState, directory: &dyn PeerDirectory) -> Result<(), Error> {
    let pending: Vec<(i64, String, String, String)> = sqlx::query_as("SELECT id, own_portal_id::text, peer_host, peer_portal_id::text FROM portal_unlinks WHERE world_id = $1 ORDER BY id LIMIT 20")
        .bind(state.world_id).fetch_all(&state.pool).await?;
    if pending.is_empty() {
        return Ok(());
    }
    let settings = settings(state).await?;
    let own_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::SERVICE_UNAVAILABLE, "federation not configured"))?;
    let own_name = server_name(state).await?;
    let key = signing_key(&settings)?;
    for (id, own_portal, peer_host, peer_portal) in pending {
        let peer_url: Option<String> = sqlx::query_scalar(
            "SELECT api_url FROM federation_peers WHERE world_id = $1 AND host = $2",
        )
        .bind(state.world_id)
        .bind(&peer_host)
        .fetch_optional(&state.pool)
        .await?;
        let Some(peer_url) = peer_url else {
            // A world we never pinned cannot have been linked.
            sqlx::query("DELETE FROM portal_unlinks WHERE id = $1")
                .bind(id)
                .execute(&state.pool)
                .await?;
            continue;
        };
        let current = now();
        let request = UnlinkRequest {
            v: 1,
            kind: "portal-unlink".into(),
            from_world: own_name.clone(),
            from_api_url: own_url.clone(),
            from_portal: own_portal,
            to_world: peer_host,
            to_portal: peer_portal,
            issued: current,
            expires: current + 300,
        };
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"))?;
        let delivered = directory
            .post_unlink(
                peer_url,
                sign(&key, UNLINK_DOMAIN, &bytes),
                settings.allow_private_peers,
            )
            .await;
        if delivered.is_ok() || matches!(&delivered, Err(Error::Status(StatusCode::GONE, _))) {
            sqlx::query("DELETE FROM portal_unlinks WHERE id = $1")
                .bind(id)
                .execute(&state.pool)
                .await?;
        }
    }
    Ok(())
}

/// Repeats the messages about broken links that could not be delivered; run periodically.
pub(super) async fn retry_pending(state: &AppState, directory: &dyn PeerDirectory) {
    let _ = notify_unlinks(state, directory).await;
}

/// The default directory used by the periodic retry.
pub(super) fn http_directory() -> Arc<dyn PeerDirectory> {
    Arc::new(federation::HttpDirectory)
}

/// The owner breaks the link of a portal: it is a finished portal again, ready for another
/// link, and the other world is told so that its end is released as well.
async fn disconnect(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Details>, Error> {
    let directory = directory(peers);
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    if !matches!(portal.state.as_str(), "open" | "pending") {
        return Err(status(StatusCode::CONFLICT, "the portal is not linked"));
    }
    break_link(&mut transaction, state.world_id, &portal.id, "built").await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, portal).await?;
    transaction.commit().await?;
    // Best effort now; what cannot be delivered is repeated.
    let _ = notify_unlinks(&state, directory.as_ref()).await;
    Ok(Json(described))
}

/// Server to server: the owner of the other end broke the link.
async fn receive_unlink(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    Json(body): Json<Message>,
) -> Result<Json<LinkReceived>, Error> {
    let directory = directory(peers);
    let settings = settings(&state).await?;
    let (bytes, signature) = open_token(&body.message)?;
    let request: UnlinkRequest = serde_json::from_slice(&bytes)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid unlink request"))?;
    let current = now();
    let own_name = server_name(&state).await?;
    if request.v != 1
        || request.kind != "portal-unlink"
        || !valid_uuid(&request.from_portal)
        || !valid_uuid(&request.to_portal)
        || !federation::valid_host(&request.from_world)
        || !federation::valid_url(&request.from_api_url)
        || request.to_world != own_name
        || request.from_world == own_name
        || request.expires <= request.issued
        || request.expires - request.issued > LINK_REQUEST_MAX_SECONDS
        || request.issued > current + CLOCK_SKEW_SECONDS
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid unlink request"));
    }
    if request.expires <= current {
        return Err(status(StatusCode::GONE, "unlink request expired"));
    }
    // Only the world this portal is linked with, signing with its pinned key, may release it.
    let peer = peer_key(
        &state,
        &settings,
        directory.as_ref(),
        &request.from_world,
        &request.from_api_url,
    )
    .await?;
    verify(&peer.key, UNLINK_DOMAIN, &bytes, &signature)?;
    pin_peer(&state, &request.from_world, &peer).await?;
    let mut transaction = state.pool.begin().await?;
    let own: Option<(String, Option<String>, Option<String>)> = sqlx::query_as("SELECT state, peer_host, peer_portal_id::text FROM portal_pacts WHERE id = $1::uuid AND world_id = $2 AND role = 'builder' FOR UPDATE")
        .bind(&request.to_portal).bind(state.world_id)
        .fetch_optional(&mut *transaction).await?;
    let Some((own_state, host, portal)) = own else {
        return Err(status(StatusCode::NOT_FOUND, "portal not found"));
    };
    if matches!(own_state.as_str(), "open" | "pending")
        && host.as_deref() == Some(request.from_world.as_str())
        && portal.as_deref() == Some(request.from_portal.as_str())
    {
        // The other world already knows: no message goes back.
        sqlx::query("UPDATE portal_pacts SET state = 'built', peer_host = NULL, peer_portal_id = NULL, peer_portal_name = NULL, updated_at = now() WHERE id = $1::uuid")
            .bind(&request.to_portal).execute(&mut *transaction).await?;
        sqlx::query("UPDATE portals SET state = 'building', peer = NULL, updated_at = now() WHERE id = (SELECT portal_id FROM portal_pacts WHERE id = $1::uuid) AND state IN ('building', 'open')")
            .bind(&request.to_portal).execute(&mut *transaction).await?;
    }
    // Not linked with that portal (any more): nothing to release, and asking again is harmless.
    let state_now: String =
        sqlx::query_scalar("SELECT state FROM portal_pacts WHERE id = $1::uuid")
            .bind(&request.to_portal)
            .fetch_one(&mut *transaction)
            .await?;
    transaction.commit().await?;
    Ok(Json(LinkReceived { state: state_now }))
}

/// Closes a portal, whatever it is doing; it stays as an inactive ruin. A link it had is broken.
async fn close(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let directory = directory(peers);
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let portal = own_portal(&mut transaction, &state, player_id, &id).await?;
    if portal.state == "closed" {
        return Err(status(StatusCode::CONFLICT, "the portal is already closed"));
    }
    break_link(&mut transaction, state.world_id, &portal.id, "closed").await?;
    let row: Option<i64> = sqlx::query_scalar("UPDATE portal_pacts SET state = 'closed', updated_at = now() WHERE id = $1::uuid RETURNING portal_id")
        .bind(&portal.id).fetch_one(&mut *transaction).await?;
    if let Some(portal_id) = row {
        sqlx::query("UPDATE portals SET state = 'closed', updated_at = now() WHERE id = $1 AND state IN ('building', 'open')")
            .bind(portal_id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    let _ = notify_unlinks(&state, directory.as_ref()).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Region {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Serialize, FromRow)]
struct WorldPortal {
    name: String,
    state: String,
    peer: String,
    position: Vec<f64>,
}

/// Portals near a point, for rendering: under construction, open, or closed ruins.
async fn world_portals(
    State(state): State<AppState>,
    Query(region): Query<Region>,
) -> Result<Json<Vec<WorldPortal>>, Error> {
    if [region.x, region.y, region.z]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid region"));
    }
    Ok(Json(
        sqlx::query_as("SELECT portal_name AS name, CASE state WHEN 'closed' THEN 'closed' WHEN 'open' THEN 'open' ELSE 'building' END AS state, coalesce(peer_host, '') AS peer, ARRAY[site_x, site_y, site_z] AS position FROM portal_pacts WHERE world_id = $1 AND role = 'builder' AND site_x IS NOT NULL AND state IN ('building', 'built', 'pending', 'open', 'closed') AND sqrt((site_x - $2)^2 + (site_y - $3)^2 + (site_z - $4)^2) <= $5 ORDER BY portal_name LIMIT 64")
            .bind(state.world_id).bind(region.x).bind(region.y).bind(region.z).bind(PORTAL_REGION_M)
            .fetch_all(&state.pool).await?,
    ))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn requirements_are_consistent() {
        let config = requirements();
        assert!(config.reach_m > 0.0 && config.min_distance_m > 0.0);
        assert!(!config.requirements.is_empty());
        let mut seen = std::collections::HashSet::new();
        for requirement in &config.requirements {
            assert!(requirement.quantity > 0 && !requirement.items.is_empty());
            for item in &requirement.items {
                assert!(
                    seen.insert(item.clone()),
                    "{item} is needed by one requirement only"
                );
            }
        }
    }
}
