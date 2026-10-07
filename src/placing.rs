//! Building and farming: things that characters put into the world.
//!
//! A campfire, a workbench, a tent or a growing crop is a row of `placed_objects`; the generated
//! world stays as it is and only these changes are stored. The client names where it wants a
//! thing; the server checks reach, the ground, spacing and the character's limits, puts the
//! thing on the surface and takes the item. Crops ripen by the clock (offline time counts).
//! Only the owner picks a thing up or harvests a crop; anybody can use a station.
//! What can be placed is data in `etc/placeables.json`.

use super::gathering::add_items;
use super::players::{self, Error};
use super::shops::take;
use super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Deserialize)]
pub(super) struct Thing {
    pub model: String,
    pub scale: f64,
    pub spacing_m: f64,
    #[serde(default)]
    pub station: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Crop {
    pub kind: String,
    pub model: String,
    pub produce: String,
    pub min: i64,
    pub max: i64,
    pub grow_minutes: i64,
}

#[derive(Deserialize)]
pub(super) struct Fishing {
    pub water_reach_m: f64,
    pub cooldown_ms: i64,
    pub stamina_cost: i32,
    pub catch_chance: f64,
    pub catch: String,
}

#[derive(Deserialize)]
pub(super) struct Config {
    version: u32,
    reach_m: f64,
    station_reach_m: f64,
    list_radius_m: f64,
    max_placed_per_player: i64,
    max_crops_per_player: i64,
    things: HashMap<String, Thing>,
    crops: HashMap<String, Crop>,
    crop_spacing_m: f64,
    pub fishing: Fishing,
}

pub(super) fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Config = serde_json::from_str(include_str!("../etc/placeables.json"))
            .expect("bundled placeables must be valid");
        assert_eq!(config.version, 1);
        config
    })
}

/// Items the configuration refers to, for checking them against the item catalog.
#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    let config = config();
    config
        .things
        .keys()
        .cloned()
        .chain(config.crops.keys().cloned())
        .chain(config.crops.values().map(|crop| crop.produce.clone()))
        .chain([config.fishing.catch.clone(), "fishing_rod".to_owned()])
        .collect()
}

/// Stations the recipes may name.
#[cfg(test)]
pub(super) fn stations() -> Vec<String> {
    config()
        .things
        .values()
        .filter_map(|thing| thing.station.clone())
        .collect()
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/place", post(place))
        .route("/players/me/plant", post(plant))
        .route("/world/placed", get(placed))
        .route("/placed/{id}/pickup", post(pickup))
        .route("/placed/{id}/harvest", post(harvest))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

fn crop_of_kind(kind: &str) -> Option<(&'static str, &'static Crop)> {
    config()
        .crops
        .iter()
        .find(|(_, crop)| crop.kind == kind)
        .map(|(seed, crop)| (seed.as_str(), crop))
}

fn distance_squared(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(a, b)| (a - b).powi(2))
        .sum()
}

/// Whether a station of this name stands within reach of the character.
pub(super) async fn station_near(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    station: &str,
) -> Result<bool, Error> {
    let kinds: Vec<String> = config()
        .things
        .iter()
        .filter(|(_, thing)| thing.station.as_deref() == Some(station))
        .map(|(kind, _)| kind.clone())
        .collect();
    let reach = config().station_reach_m;
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM placed_objects JOIN players ON players.id = $1 WHERE placed_objects.world_id = players.world_id AND placed_objects.kind = ANY($2) AND players.position_x IS NOT NULL AND (placed_objects.position_x - players.position_x)^2 + (placed_objects.position_y - players.position_y)^2 + (placed_objects.position_z - players.position_z)^2 <= $3)")
        .bind(player_id).bind(&kinds).bind(reach * reach).fetch_one(&mut **transaction).await?)
}

#[derive(Serialize, FromRow)]
struct Row {
    id: i64,
    kind: String,
    position_x: f64,
    position_y: f64,
    position_z: f64,
    yaw: f64,
    owner: Option<String>,
    /// Growth from 0 to 1 of a crop; absent for other things.
    growth: Option<f64>,
}

#[derive(Serialize)]
struct View {
    id: i64,
    kind: String,
    model: String,
    position: [f64; 3],
    yaw: f64,
    scale: f64,
    owner: Option<String>,
    station: Option<String>,
    crop: Option<CropView>,
}

#[derive(Serialize)]
struct CropView {
    growth: f64,
    ripe: bool,
}

const ROW: &str = "SELECT placed_objects.id, placed_objects.kind, placed_objects.position_x, placed_objects.position_y, placed_objects.position_z, placed_objects.yaw, players.username AS owner, CASE WHEN ripe_at IS NULL THEN NULL ELSE least(1.0, greatest(0.0, extract(epoch FROM (clock_timestamp() - planted_at)) / greatest(1.0, extract(epoch FROM (ripe_at - planted_at)))))::float8 END AS growth FROM placed_objects LEFT JOIN players ON players.id = placed_objects.owner_id";

fn view(row: Row) -> Option<View> {
    let config = config();
    let (model, scale, station, crop) = if let Some((_, crop)) = crop_of_kind(&row.kind) {
        let growth = row.growth.unwrap_or(0.0);
        (
            crop.model.clone(),
            0.35 + 0.65 * growth,
            None,
            Some(CropView {
                growth,
                ripe: growth >= 1.0,
            }),
        )
    } else {
        let thing = config.things.get(&row.kind)?;
        (
            thing.model.clone(),
            thing.scale,
            thing.station.clone(),
            None,
        )
    };
    Some(View {
        id: row.id,
        kind: row.kind,
        model,
        position: [row.position_x, row.position_y, row.position_z],
        yaw: row.yaw,
        scale,
        owner: row.owner,
        station,
        crop,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    item_id: String,
    /// Where the character wants it, in metres from the planet's centre.
    x: f64,
    y: f64,
    z: f64,
    #[serde(default)]
    yaw: f64,
}

#[derive(Serialize)]
struct Placed {
    object: View,
    player: players::Player,
}

/// The surface point below a requested position, or an error when it is not dry land.
async fn surface(state: &AppState, target: &Target) -> Result<[f64; 3], Error> {
    let values = [target.x, target.y, target.z, target.yaw];
    if values.iter().any(|value| !value.is_finite())
        || [target.x, target.y, target.z]
            .iter()
            .any(|value| value.abs() > 7_000_000.0)
        || !(0.0..=std::f64::consts::TAU + 0.001).contains(&target.yaw)
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid position"));
    }
    let length = (target.x.powi(2) + target.y.powi(2) + target.z.powi(2)).sqrt();
    if length < 1.0 {
        return Err(status(StatusCode::BAD_REQUEST, "invalid position"));
    }
    let direction = [target.x / length, target.y / length, target.z / length];
    let terrain = super::movement::terrain(state).await?;
    let height = terrain.ground_height(direction);
    if height <= 0.3 {
        return Err(status(
            StatusCode::CONFLICT,
            "nothing can be placed on water",
        ));
    }
    Ok(direction.map(|axis| axis * (super::movement::RADIUS + height)))
}

/// Puts a thing of this kind where the character asked, taking `item` from them.
async fn put_down(
    state: &AppState,
    player_id: i64,
    target: &Target,
    item: &str,
    kind: &str,
    spacing: f64,
    crop: Option<&Crop>,
) -> Result<Json<Placed>, Error> {
    let point = surface(state, target).await?;
    let config = config();
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let position: Option<(f64, f64, f64)> = sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let (x, y, z) =
        position.ok_or_else(|| status(StatusCode::CONFLICT, "player position unavailable"))?;
    if distance_squared([x, y, z], point) > config.reach_m * config.reach_m {
        return Err(status(StatusCode::FORBIDDEN, "too far to place"));
    }
    // Placing is serialised per world so two characters cannot put things on one spot.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(state.world_id)
        .execute(&mut *transaction)
        .await?;
    let crops = crop.is_some();
    let owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM placed_objects WHERE owner_id = $1 AND (kind LIKE 'crop\\_%') = $2",
    )
    .bind(player_id)
    .bind(crops)
    .fetch_one(&mut *transaction)
    .await?;
    let limit = if crops {
        config.max_crops_per_player
    } else {
        config.max_placed_per_player
    };
    if owned >= limit {
        return Err(status(StatusCode::CONFLICT, "too many placed things"));
    }
    let crowded: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM placed_objects WHERE world_id = $1 AND position_x BETWEEN $2 - $5 AND $2 + $5 AND (position_x - $2)^2 + (position_y - $3)^2 + (position_z - $4)^2 < $5 * $5)")
        .bind(state.world_id).bind(point[0]).bind(point[1]).bind(point[2]).bind(spacing)
        .fetch_one(&mut *transaction).await?;
    if crowded {
        return Err(status(StatusCode::CONFLICT, "something is in the way"));
    }
    take(&mut transaction, player_id, item, 1).await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO placed_objects (world_id, owner_id, kind, position_x, position_y, position_z, yaw, ripe_at) VALUES ($1, $2, $3, $4, $5, $6, $7, CASE WHEN $8::bigint IS NULL THEN NULL ELSE now() + make_interval(mins => $8::int) END) RETURNING id")
        .bind(state.world_id).bind(player_id).bind(kind).bind(point[0]).bind(point[1]).bind(point[2]).bind(target.yaw)
        .bind(crop.map(|crop| crop.grow_minutes))
        .fetch_one(&mut *transaction).await?;
    let row: Row = sqlx::query_as(&format!("{ROW} WHERE placed_objects.id = $1"))
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    transaction.commit().await?;
    let object =
        view(row).ok_or_else(|| status(StatusCode::INTERNAL_SERVER_ERROR, "unknown thing"))?;
    Ok(Json(Placed {
        object,
        player: players::profile(state, player_id).await?,
    }))
}

async fn place(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(target): Json<Target>,
) -> Result<Json<Placed>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let thing = config()
        .things
        .get(&target.item_id)
        .ok_or_else(|| status(StatusCode::CONFLICT, "this cannot be placed"))?;
    put_down(
        &state,
        player_id,
        &target,
        &target.item_id,
        &target.item_id,
        thing.spacing_m,
        None,
    )
    .await
}

async fn plant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(target): Json<Target>,
) -> Result<Json<Placed>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let crop = config()
        .crops
        .get(&target.item_id)
        .ok_or_else(|| status(StatusCode::CONFLICT, "this cannot be planted"))?;
    put_down(
        &state,
        player_id,
        &target,
        &target.item_id,
        &crop.kind,
        config().crop_spacing_m,
        Some(crop),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Area {
    x: f64,
    y: f64,
    z: f64,
}

/// What stands around a point (no login needed, like the generated objects).
async fn placed(
    State(state): State<AppState>,
    Query(area): Query<Area>,
) -> Result<Json<Vec<View>>, Error> {
    let radius = config().list_radius_m;
    if [area.x, area.y, area.z]
        .iter()
        .any(|value| !value.is_finite() || value.abs() > 7_000_000.0)
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid position"));
    }
    let rows: Vec<Row> = sqlx::query_as(&format!("{ROW} WHERE placed_objects.world_id = $1 AND placed_objects.position_x BETWEEN $2 - $5 AND $2 + $5 AND (placed_objects.position_x - $2)^2 + (placed_objects.position_y - $3)^2 + (placed_objects.position_z - $4)^2 <= $5 * $5 ORDER BY placed_objects.id LIMIT 500"))
        .bind(state.world_id).bind(area.x).bind(area.y).bind(area.z).bind(radius)
        .fetch_all(&state.pool).await?;
    Ok(Json(rows.into_iter().filter_map(view).collect()))
}

/// Locks the thing and the character who is its owner and stands within reach of it.
async fn own_nearby(
    state: &AppState,
    player_id: i64,
    id: i64,
) -> Result<(sqlx::Transaction<'static, sqlx::Postgres>, Row), Error> {
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let position: Option<(f64, f64, f64)> = sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let (x, y, z) =
        position.ok_or_else(|| status(StatusCode::CONFLICT, "player position unavailable"))?;
    let row: Option<Row> = sqlx::query_as(&format!("{ROW} WHERE placed_objects.id = $1 AND placed_objects.world_id = $2 AND placed_objects.owner_id = $3 FOR UPDATE OF placed_objects"))
        .bind(id).bind(state.world_id).bind(player_id)
        .fetch_optional(&mut *transaction).await?;
    let row = match row {
        Some(row) => row,
        None => {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM placed_objects WHERE id = $1 AND world_id = $2)",
            )
            .bind(id)
            .bind(state.world_id)
            .fetch_one(&mut *transaction)
            .await?;
            return Err(if exists {
                status(StatusCode::FORBIDDEN, "this is not yours")
            } else {
                status(StatusCode::NOT_FOUND, "nothing there")
            });
        }
    };
    let reach = config().reach_m;
    if distance_squared([x, y, z], [row.position_x, row.position_y, row.position_z]) > reach * reach
    {
        return Err(status(StatusCode::FORBIDDEN, "too far away"));
    }
    Ok((transaction, row))
}

#[derive(Serialize)]
struct Taken {
    items: Vec<super::gathering::Gained>,
    player: players::Player,
}

/// Takes a thing back into the inventory (a crop that is not ripe gives its seed back).
async fn pickup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Taken>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let (mut transaction, row) = own_nearby(&state, player_id, id).await?;
    let item = match crop_of_kind(&row.kind) {
        Some((seed, _)) => seed.to_owned(),
        None => row.kind.clone(),
    };
    let items = add_items(&mut transaction, player_id, &[(item, 1)]).await?;
    sqlx::query("DELETE FROM placed_objects WHERE id = $1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(Json(Taken {
        items,
        player: players::profile(&state, player_id).await?,
    }))
}

/// Harvests a ripe crop: the produce and one seed to plant again.
async fn harvest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Taken>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let (mut transaction, row) = own_nearby(&state, player_id, id).await?;
    let (seed, crop) = crop_of_kind(&row.kind)
        .ok_or_else(|| status(StatusCode::CONFLICT, "this is not a crop"))?;
    if row.growth.unwrap_or(0.0) < 1.0 {
        return Err(status(StatusCode::CONFLICT, "not ripe yet"));
    }
    let amount = OsRng.gen_range(crop.min..=crop.max);
    let items = add_items(
        &mut transaction,
        player_id,
        &[(crop.produce.clone(), amount), (seed.to_owned(), 1)],
    )
    .await?;
    sqlx::query("DELETE FROM placed_objects WHERE id = $1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    super::experience::grant(&mut transaction, player_id, 2).await?;
    transaction.commit().await?;
    Ok(Json(Taken {
        items,
        player: players::profile(&state, player_id).await?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_placeables_are_consistent() {
        let config = config();
        assert!(config.reach_m > 0.0 && config.list_radius_m > config.reach_m);
        for thing in config.things.values() {
            assert!(
                thing.spacing_m > 0.0 && thing.scale > 0.0,
                "{}",
                thing.model
            );
        }
        for (seed, crop) in &config.crops {
            assert!(seed.starts_with("seed_"));
            assert!(crop.kind.starts_with("crop_"), "{}", crop.kind);
            assert!(0 < crop.min && crop.min <= crop.max && crop.grow_minutes > 0);
        }
        assert!((0.0..=1.0).contains(&config.fishing.catch_chance));
    }
}
