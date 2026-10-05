//! Joint building of a portal pact.
//!
//! After the operator accepts a pact, each player places a construction site for
//! their own end of the portal and delivers the materials listed in
//! `etc/portal.json`. When both worlds report their end built, the pact and the
//! portals open. Either player can close the pact; the portals then stay as
//! inactive ruins and keep the record of the materials delivered to them.

use super::federation::{
    self, directory, now, open_token, peer_key, pin_peer, server_name, settings, sign, signing_key,
    status, valid_uuid, verify, PeerDirectory, ACCEPT_MAX_SECONDS, CLOCK_SKEW_SECONDS,
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

const STATUS_DOMAIN: &[u8] = b"ishtaria/portal-pact-status/v1\0";
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
        .route("/portals/pacts/{id}", get(details).delete(cancel))
        .route("/portals/pacts/{id}/site", post(place_site))
        .route("/portals/pacts/{id}/contribute", post(contribute))
        .route(
            "/federation/pacts/status",
            post(receive_status).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/world/portals", get(world_portals))
}

#[derive(Serialize, FromRow)]
struct PactRow {
    id: String,
    role: String,
    state: String,
    peer_host: String,
    peer_player: String,
    portal_name: String,
    peer_portal_name: Option<String>,
    site_x: Option<f64>,
    site_y: Option<f64>,
    site_z: Option<f64>,
    local_built: bool,
    peer_built: bool,
}

const PACT_ROW: &str = "id::text AS id, role, state, peer_host, peer_player, portal_name, peer_portal_name, site_x, site_y, site_z, local_built_at IS NOT NULL AS local_built, peer_built_at IS NOT NULL AS peer_built";

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
    role: String,
    state: String,
    peer_host: String,
    peer_player: String,
    portal_name: String,
    peer_portal_name: Option<String>,
    site: Option<[f64; 3]>,
    local_built: bool,
    peer_built: bool,
    requirements: Vec<Progress>,
}

/// How much of each requirement has been delivered, in configuration order.
async fn contributed(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pact_id: &str,
) -> Result<Vec<i64>, Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT item_id, quantity FROM portal_contributions WHERE pact_id = $1::uuid",
    )
    .bind(pact_id)
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

async fn own_pact(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    state: &AppState,
    player_id: i64,
    id: &str,
) -> Result<PactRow, Error> {
    if !valid_uuid(id) {
        return Err(status(StatusCode::NOT_FOUND, "pact not found"));
    }
    sqlx::query_as(&format!(
        "SELECT {PACT_ROW} FROM portal_pacts WHERE id = $1::uuid AND world_id = $2 AND player_id = $3 FOR UPDATE"
    ))
    .bind(id)
    .bind(state.world_id)
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| status(StatusCode::NOT_FOUND, "pact not found"))
}

async fn describe(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pact: PactRow,
) -> Result<Details, Error> {
    let delivered = contributed(transaction, &pact.id).await?;
    let site = match (pact.site_x, pact.site_y, pact.site_z) {
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
        id: pact.id,
        role: pact.role,
        state: pact.state,
        peer_host: pact.peer_host,
        peer_player: pact.peer_player,
        portal_name: pact.portal_name,
        peer_portal_name: pact.peer_portal_name,
        site,
        local_built: pact.local_built,
        peer_built: pact.peer_built,
    })
}

async fn details(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Details>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let pact = own_pact(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, pact).await?;
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

/// Places the construction site of this world's end of the portal where the player stands.
async fn place_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Details>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let terrain = super::movement::terrain(&state).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let pact = own_pact(&mut transaction, &state, player_id, &id).await?;
    if pact.state != "accepted" || pact.site_x.is_some() {
        return Err(status(
            StatusCode::CONFLICT,
            "the pact is not waiting for a construction site",
        ));
    }
    let position = own_position(&mut transaction, player_id).await?;
    super::land::require_tenant(&mut transaction, state.world_id, player_id, position).await?;
    let (face, x, y) = terrain
        .build_site(position)
        .ok_or_else(|| status(StatusCode::CONFLICT, "a portal cannot be built here"))?;
    let near: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM portal_pacts WHERE world_id = $1 AND site_x IS NOT NULL AND state IN ('building', 'open') AND sqrt((site_x - $2)^2 + (site_y - $3)^2 + (site_z - $4)^2) < $5)")
        .bind(state.world_id).bind(position[0]).bind(position[1]).bind(position[2]).bind(requirements().min_distance_m)
        .fetch_one(&mut *transaction).await?;
    if near {
        return Err(status(StatusCode::CONFLICT, "another portal is too close"));
    }
    let portal_id: i64 = sqlx::query_scalar("INSERT INTO portals (world_id, name, peer, state, face, x, y) VALUES ($1, $2, $3, 'building', $4, $5, $6) ON CONFLICT DO NOTHING RETURNING id")
        .bind(state.world_id).bind(&pact.portal_name).bind(&pact.peer_host).bind(face).bind(x).bind(y)
        .fetch_optional(&mut *transaction).await?
        .ok_or_else(|| status(StatusCode::CONFLICT, "portal name already in use"))?;
    sqlx::query("UPDATE portal_pacts SET site_x = $2, site_y = $3, site_z = $4, portal_id = $5, state = 'building', updated_at = now() WHERE id = $1::uuid")
        .bind(&pact.id).bind(position[0]).bind(position[1]).bind(position[2]).bind(portal_id)
        .execute(&mut *transaction).await?;
    let pact = own_pact(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, pact).await?;
    transaction.commit().await?;
    Ok(Json(described))
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
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Contribution>,
) -> Result<Json<Details>, Error> {
    let directory = directory(peers);
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
    let pact = own_pact(&mut transaction, &state, player_id, &id).await?;
    let (Some(x), Some(y), Some(z)) = (pact.site_x, pact.site_y, pact.site_z) else {
        return Err(status(
            StatusCode::CONFLICT,
            "there is no construction site yet",
        ));
    };
    if pact.state != "building" || pact.local_built {
        return Err(status(
            StatusCode::CONFLICT,
            "this end is not under construction",
        ));
    }
    if distance(own_position(&mut transaction, player_id).await?, [x, y, z]) > config.reach_m {
        return Err(status(
            StatusCode::FORBIDDEN,
            "the construction site is out of reach",
        ));
    }
    let delivered = contributed(&mut transaction, &pact.id).await?;
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
        .bind(&pact.id).bind(&input.item_id).bind(amount).execute(&mut *transaction).await?;
    let delivered = contributed(&mut transaction, &pact.id).await?;
    let complete = config
        .requirements
        .iter()
        .zip(&delivered)
        .all(|(requirement, delivered)| *delivered >= requirement.quantity);
    if complete {
        sqlx::query("UPDATE portal_pacts SET local_built_at = now(), notify_pending = 'built', updated_at = now() WHERE id = $1::uuid")
            .bind(&pact.id).execute(&mut *transaction).await?;
        open_if_built(&mut transaction, &pact.id).await?;
    }
    let pact = own_pact(&mut transaction, &state, player_id, &id).await?;
    let described = describe(&mut transaction, pact).await?;
    transaction.commit().await?;
    if complete {
        // Best effort: a message the peer does not receive now is repeated later.
        let _ = notify_peer(&state, directory.as_ref(), &id).await;
    }
    Ok(Json(described))
}

/// Opens the pact and its portal once both ends are built.
async fn open_if_built(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pact_id: &str,
) -> Result<bool, Error> {
    let opened: Option<i64> = sqlx::query_scalar("UPDATE portal_pacts SET state = 'open', updated_at = now() WHERE id = $1::uuid AND state = 'building' AND local_built_at IS NOT NULL AND peer_built_at IS NOT NULL RETURNING portal_id")
        .bind(pact_id).fetch_optional(&mut **transaction).await?;
    if let Some(portal_id) = opened {
        sqlx::query("UPDATE portals SET state = 'open', updated_at = now() WHERE id = $1 AND state = 'building'")
            .bind(portal_id)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(opened.is_some())
}

/// Closes the pact and leaves its portal as an inactive ruin.
async fn close_pact(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pact_id: &str,
) -> Result<(), Error> {
    let portal: Option<Option<i64>> = sqlx::query_scalar("UPDATE portal_pacts SET state = 'closed', updated_at = now() WHERE id = $1::uuid AND state IN ('proposed', 'accepted', 'building', 'open') RETURNING portal_id")
        .bind(pact_id).fetch_optional(&mut **transaction).await?;
    if let Some(Some(portal_id)) = portal {
        sqlx::query("UPDATE portals SET state = 'closed', updated_at = now() WHERE id = $1 AND state IN ('building', 'open')")
            .bind(portal_id)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

async fn cancel(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let directory = directory(peers);
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let pact = own_pact(&mut transaction, &state, player_id, &id).await?;
    if matches!(
        pact.state.as_str(),
        "closed" | "declined" | "expired" | "banned"
    ) {
        return Err(status(StatusCode::CONFLICT, "the pact is already closed"));
    }
    close_pact(&mut transaction, &pact.id).await?;
    sqlx::query("UPDATE portal_pacts SET notify_pending = 'closed' WHERE id = $1::uuid")
        .bind(&pact.id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    let _ = notify_peer(&state, directory.as_ref(), &id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusPayload {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    invitation: String,
    from_world: String,
    from_api_url: String,
    to_world: String,
    status: String,
    issued: i64,
    expires: i64,
}

/// Sends the pending status of a pact to the other world, and forgets it once delivered.
async fn notify_peer(
    state: &AppState,
    directory: &dyn PeerDirectory,
    pact_id: &str,
) -> Result<(), Error> {
    let row: Option<(String, String, String)> = sqlx::query_as("SELECT invitation_id::text, peer_host, notify_pending FROM portal_pacts WHERE id = $1::uuid AND world_id = $2 AND notify_pending IS NOT NULL")
        .bind(pact_id).bind(state.world_id).fetch_optional(&state.pool).await?;
    let Some((invitation, peer_host, pending)) = row else {
        return Ok(());
    };
    let settings = settings(state).await?;
    let own_url = settings
        .public_url
        .clone()
        .ok_or_else(|| status(StatusCode::SERVICE_UNAVAILABLE, "federation not configured"))?;
    let peer_url: String = sqlx::query_scalar(
        "SELECT api_url FROM federation_peers WHERE world_id = $1 AND host = $2",
    )
    .bind(state.world_id)
    .bind(&peer_host)
    .fetch_one(&state.pool)
    .await?;
    let current = now();
    let payload = StatusPayload {
        v: 1,
        kind: "portal-pact-status".into(),
        invitation,
        from_world: server_name(state).await?,
        from_api_url: own_url,
        to_world: peer_host,
        status: pending.clone(),
        issued: current,
        expires: current + 300,
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|_| status(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"))?;
    let message = sign(&signing_key(&settings)?, STATUS_DOMAIN, &bytes);
    let delivered = directory
        .post_status(peer_url, message, settings.allow_private_peers)
        .await;
    // A peer that no longer knows the pact will never accept the message.
    if delivered.is_ok() || matches!(&delivered, Err(Error::Status(StatusCode::GONE, _))) {
        sqlx::query("UPDATE portal_pacts SET notify_pending = NULL WHERE id = $1::uuid AND notify_pending = $2")
            .bind(pact_id).bind(pending).execute(&state.pool).await?;
    }
    delivered
}

/// Repeats status messages that could not be delivered; run periodically.
pub(super) async fn retry_pending(state: &AppState, directory: &dyn PeerDirectory) {
    let pending: Result<Vec<String>, sqlx::Error> = sqlx::query_scalar(
        "SELECT id::text FROM portal_pacts WHERE world_id = $1 AND notify_pending IS NOT NULL LIMIT 20",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await;
    for id in pending.unwrap_or_default() {
        let _ = notify_peer(state, directory, &id).await;
    }
}

/// The default directory used by the periodic retry.
pub(super) fn http_directory() -> Arc<dyn PeerDirectory> {
    Arc::new(federation::HttpDirectory)
}

#[derive(Deserialize)]
struct Message {
    message: String,
}

#[derive(Serialize)]
struct StatusReceived {
    state: String,
}

/// Server-to-server: the other world reports that its end is built or the pact is closed.
async fn receive_status(
    State(state): State<AppState>,
    peers: Option<Extension<Arc<dyn PeerDirectory>>>,
    Json(body): Json<Message>,
) -> Result<Json<StatusReceived>, Error> {
    let directory = directory(peers);
    let settings = settings(&state).await?;
    let (bytes, signature) = open_token(&body.message)?;
    let report: StatusPayload = serde_json::from_slice(&bytes)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid status"))?;
    let current = now();
    let own_name = server_name(&state).await?;
    if report.v != 1
        || report.kind != "portal-pact-status"
        || !valid_uuid(&report.invitation)
        || !federation::valid_host(&report.from_world)
        || !federation::valid_url(&report.from_api_url)
        || !["built", "closed"].contains(&report.status.as_str())
        || report.to_world != own_name
        || report.from_world == own_name
        || report.expires <= report.issued
        || report.expires - report.issued > ACCEPT_MAX_SECONDS
        || report.issued > current + CLOCK_SKEW_SECONDS
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid status"));
    }
    if report.expires <= current {
        return Err(status(StatusCode::GONE, "status expired"));
    }
    let peer = peer_key(
        &state,
        &settings,
        directory.as_ref(),
        &report.from_world,
        &report.from_api_url,
    )
    .await?;
    verify(&peer.key, STATUS_DOMAIN, &bytes, &signature)?;
    pin_peer(&state, &report.from_world, &peer).await?;

    let mut transaction = state.pool.begin().await?;
    let pact: Option<(String, String)> = sqlx::query_as("SELECT id::text, state FROM portal_pacts WHERE world_id = $1 AND invitation_id = $2::uuid AND peer_host = $3 FOR UPDATE")
        .bind(state.world_id).bind(&report.invitation).bind(&report.from_world)
        .fetch_optional(&mut *transaction).await?;
    let Some((pact_id, pact_state)) = pact else {
        return Err(status(StatusCode::NOT_FOUND, "pact not found"));
    };
    match report.status.as_str() {
        "closed" => close_pact(&mut transaction, &pact_id).await?,
        _ if matches!(pact_state.as_str(), "building" | "accepted" | "open") => {
            sqlx::query("UPDATE portal_pacts SET peer_built_at = coalesce(peer_built_at, now()), updated_at = now() WHERE id = $1::uuid")
                .bind(&pact_id).execute(&mut *transaction).await?;
            open_if_built(&mut transaction, &pact_id).await?;
        }
        _ => {}
    }
    let state_now: String =
        sqlx::query_scalar("SELECT state FROM portal_pacts WHERE id = $1::uuid")
            .bind(&pact_id)
            .fetch_one(&mut *transaction)
            .await?;
    transaction.commit().await?;
    Ok(Json(StatusReceived { state: state_now }))
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
        sqlx::query_as("SELECT portal_name AS name, CASE state WHEN 'closed' THEN 'closed' WHEN 'open' THEN 'open' ELSE 'building' END AS state, peer_host AS peer, ARRAY[site_x, site_y, site_z] AS position FROM portal_pacts WHERE world_id = $1 AND site_x IS NOT NULL AND state IN ('building', 'open', 'closed') AND sqrt((site_x - $2)^2 + (site_y - $3)^2 + (site_z - $4)^2) <= $5 ORDER BY portal_name LIMIT 64")
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
