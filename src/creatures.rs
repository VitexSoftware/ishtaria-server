//! Butchering animals for meat and milking cows.
//!
//! Animals are generated, walk between waypoints that depend only on their id and the time
//! (see `movement::WalkingTerrain::animal_route`), and are described in `etc/creatures.json`.
//! The server regenerates the animal a player names where it stands now, checks reach,
//! stamina and rate, and decides the outcome. A butchered animal is stored as a change of
//! the generated world (`world_object_state`) and returns after a while. Dangerous animals
//! strike back at a hit they survive (`combat.rs`).

use super::gathering::{add_items, Gained};
use super::players::{self, Error};
use super::AppState;
use axum::{extract::State, http::HeaderMap, http::StatusCode, routing::post, Json, Router};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

/// An animal may have walked this far since the client last saw it.
const WALK_TOLERANCE_M: f64 = 3.0;

#[derive(Deserialize)]
struct Drop {
    item: String,
    min: i64,
    max: i64,
}

#[derive(Deserialize)]
struct Animal {
    model: String,
    /// Work points needed to butcher it; a swing is worth the points of the weapon in hand.
    hits: i32,
    respawn_minutes: i64,
    drops: Vec<Drop>,
}

#[derive(Deserialize)]
struct Milk {
    model: String,
    reach_m: f64,
    water: f64,
    cooldown_minutes: i64,
}

#[derive(Deserialize)]
struct Creatures {
    version: u32,
    reach_m: f64,
    cooldown_ms: i64,
    stamina_cost: i32,
    forget_hits_after_minutes: i64,
    weapons: HashMap<String, i32>,
    animals: Vec<Animal>,
    milk: Milk,
}

fn creatures() -> &'static Creatures {
    static CONFIG: OnceLock<Creatures> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Creatures = serde_json::from_str(include_str!("../etc/creatures.json"))
            .expect("bundled creatures must be valid");
        assert_eq!(config.version, 1);
        config
    })
}

/// Items the configuration refers to, for checking them against the item catalog.
#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    let config = creatures();
    config
        .weapons
        .keys()
        .cloned()
        .chain(
            config
                .animals
                .iter()
                .flat_map(|animal| animal.drops.iter().map(|drop| drop.item.clone())),
        )
        .collect()
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/butcher", post(butcher))
        .route("/players/me/milk", post(milk))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    object_id: String,
}

#[derive(Serialize)]
struct ButcherReply {
    object_id: String,
    model: String,
    /// The weapon in hand that was used.
    weapon: String,
    /// `hit` while the animal lives, `depleted` once it is butchered.
    state: &'static str,
    hits: i32,
    hits_required: i32,
    xp: i64,
    wear: Option<super::durability::Wear>,
    items: Vec<Gained>,
    /// How the animal bit back, when it did.
    counter: Option<super::combat::Counter>,
    player: players::Player,
}

#[derive(Serialize)]
struct MilkReply {
    object_id: String,
    /// Water the character drank, in points of the 0-100 reserve.
    water: f64,
    player: players::Player,
}

fn valid_id(id: &str) -> bool {
    id.len() <= 100 && id.is_ascii()
}

fn distance(from: [f64; 3], to: [f64; 3]) -> f64 {
    from.into_iter()
        .zip(to)
        .map(|(first, second)| (first - second).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Adds `points` of work to an animal and returns its total; an animal left alone for a while
/// recovers from its wounds, and a butchered one cannot be hit again until it returns.
async fn register_points(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    object_id: &str,
    points: i32,
) -> Result<i32, Error> {
    sqlx::query("INSERT INTO world_object_state (world_id, object_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(world_id).bind(object_id).execute(&mut **transaction).await?;
    let (hits, depleted, stale): (i32, bool, bool) = sqlx::query_as("SELECT hits, coalesce(depleted_until > now(), false), last_hit_at < now() - make_interval(mins => $3) FROM world_object_state WHERE world_id = $1 AND object_id = $2 FOR UPDATE")
        .bind(world_id).bind(object_id).bind(creatures().forget_hits_after_minutes as i32)
        .fetch_one(&mut **transaction).await?;
    if depleted {
        return Err(status(StatusCode::CONFLICT, "already butchered"));
    }
    Ok(if stale { points } else { hits + points })
}

/// Records the wounds of an animal, or, when it has had enough, takes its life: the drops go to
/// the character and the animal returns after a while (the second value is when, unix seconds).
async fn settle_animal(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    player_id: i64,
    object_id: &str,
    kind: &Animal,
    progress: i32,
) -> Result<(Vec<Gained>, Option<i64>), Error> {
    if progress < kind.hits {
        sqlx::query("UPDATE world_object_state SET hits = $3, last_hit_at = now() WHERE world_id = $1 AND object_id = $2")
            .bind(world_id).bind(object_id).bind(progress)
            .execute(&mut **transaction).await?;
        return Ok((Vec::new(), None));
    }
    let mut rng = OsRng;
    let drops: Vec<(String, i64)> = kind
        .drops
        .iter()
        .map(|drop| (drop.item.clone(), rng.gen_range(drop.min..=drop.max)))
        .collect();
    let items = add_items(transaction, player_id, &drops).await?;
    let until: i64 = sqlx::query_scalar("UPDATE world_object_state SET hits = 0, last_hit_at = now(), depleted_until = now() + make_interval(mins => $3) WHERE world_id = $1 AND object_id = $2 RETURNING extract(epoch FROM depleted_until)::bigint")
        .bind(world_id).bind(object_id).bind(kind.respawn_minutes as i32)
        .fetch_one(&mut **transaction).await?;
    Ok((items, Some(until)))
}

/// What a spell did to an animal.
#[derive(Serialize)]
pub(super) struct SpellStrike {
    /// `hit` while the animal lives, `depleted` once it is dead.
    pub state: &'static str,
    pub hits: i32,
    pub hits_required: i32,
    pub items: Vec<Gained>,
    pub counter: Option<super::combat::Counter>,
}

/// An animal of the generated world that a spell can hit.
pub(super) struct SpellTarget {
    pub id: String,
    pub model: String,
    pub position: [f64; 3],
}

/// The animal a spell is aimed at (it must be one the hunting rules know).
pub(super) async fn spell_target(
    state: &AppState,
    object_id: &str,
) -> Result<(SpellTarget, std::sync::Arc<super::movement::WalkingTerrain>), Error> {
    if !valid_id(object_id) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid object"));
    }
    let terrain = super::movement::terrain(state).await?;
    let animal = terrain
        .animal_by_id(object_id, super::movement::unix_now_ms())
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "animal not found"))?;
    if !creatures()
        .animals
        .iter()
        .any(|kind| kind.model == animal.model)
    {
        return Err(status(StatusCode::BAD_REQUEST, "animal cannot be hit"));
    }
    Ok((
        SpellTarget {
            id: object_id.to_owned(),
            model: animal.model,
            position: animal.position,
        },
        terrain,
    ))
}

/// Hits the animal with `points` of magic in a transaction where the caster is locked and alive
/// and already stands within the spell's range. The animal bites back only when the caster is
/// within its melee reach. Killing it removes it from the world until it returns.
pub(super) async fn spell_strike(
    world_id: i64,
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    terrain: &super::movement::WalkingTerrain,
    player_id: i64,
    target: &SpellTarget,
    caster: [f64; 3],
    points: i32,
) -> Result<SpellStrike, Error> {
    let kind = creatures()
        .animals
        .iter()
        .find(|kind| kind.model == target.model)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "animal cannot be hit"))?;
    let object_id = target.id.as_str();
    let progress = register_points(transaction, world_id, object_id, points).await?;
    super::experience::grant(transaction, player_id, 1).await?;
    let (items, returns_at) =
        settle_animal(transaction, world_id, player_id, object_id, kind, progress).await?;
    let melee = distance(caster, target.position) <= creatures().reach_m + WALK_TOLERANCE_M;
    let counter = if returns_at.is_none() && melee {
        super::combat::counter_attack(transaction, player_id, &target.model).await?
    } else {
        None
    };
    if let Some(until) = returns_at {
        terrain.mark_removed(object_id, until, None);
    }
    Ok(SpellStrike {
        state: if returns_at.is_some() {
            "depleted"
        } else {
            "hit"
        },
        hits: if returns_at.is_some() {
            0
        } else {
            (progress + points - 1) / points
        },
        hits_required: (kind.hits + points - 1) / points,
        items,
        counter,
    })
}

async fn butcher(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Target>,
) -> Result<Json<ButcherReply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !valid_id(&input.object_id) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid object"));
    }
    let config = creatures();
    let terrain = super::movement::terrain(&state).await?;
    let animal = terrain
        .animal_by_id(&input.object_id, super::movement::unix_now_ms())
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "animal not found"))?;
    let kind = config
        .animals
        .iter()
        .find(|kind| kind.model == animal.model)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "animal cannot be butchered"))?;

    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    #[allow(clippy::type_complexity)]
    let row: Option<(f64, f64, f64, i32, Option<f64>)> = sqlx::query_as("SELECT position_x, position_y, position_z, stamina, extract(epoch FROM (clock_timestamp() - last_gathered_at))::float8 FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let (x, y, z, stamina, since) =
        row.ok_or_else(|| status(StatusCode::CONFLICT, "player position unavailable"))?;
    if distance([x, y, z], animal.position) > config.reach_m + WALK_TOLERANCE_M {
        return Err(status(StatusCode::FORBIDDEN, "animal is out of reach"));
    }
    if since.is_some_and(|seconds| seconds * 1000.0 < config.cooldown_ms as f64) {
        return Err(status(StatusCode::TOO_MANY_REQUESTS, "too fast"));
    }
    if stamina < config.stamina_cost {
        return Err(status(StatusCode::CONFLICT, "too exhausted"));
    }
    // Only the weapon in hand counts, and only while the character still owns it.
    let equipped: Option<String> = sqlx::query_scalar("SELECT hand FROM player_equipment WHERE player_id = $1 AND EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = hand)")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let weapon = equipped.ok_or_else(|| status(StatusCode::CONFLICT, "weapon missing"))?;
    let points = *config
        .weapons
        .get(&weapon)
        .ok_or_else(|| status(StatusCode::CONFLICT, "weapon missing"))?;
    let progress =
        register_points(&mut transaction, state.world_id, &input.object_id, points).await?;
    sqlx::query("UPDATE players SET stamina = stamina - $2, last_gathered_at = clock_timestamp() WHERE id = $1")
        .bind(player_id).bind(config.stamina_cost).execute(&mut *transaction).await?;
    let wear = super::durability::wear(&mut transaction, player_id, &weapon, 1).await?;
    super::experience::grant(&mut transaction, player_id, 1).await?;
    let (items, returns_at) = settle_animal(
        &mut transaction,
        state.world_id,
        player_id,
        &input.object_id,
        kind,
        progress,
    )
    .await?;
    // A dangerous animal that survived the blow may strike back; killing it ends the danger.
    let counter = if returns_at.is_none() {
        super::combat::counter_attack(&mut transaction, player_id, &animal.model).await?
    } else {
        None
    };
    if counter.as_ref().is_some_and(|counter| counter.died) {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    transaction.commit().await?;
    if let Some(until) = returns_at {
        terrain.mark_removed(&input.object_id, until, None);
    }
    Ok(Json(ButcherReply {
        object_id: input.object_id,
        model: animal.model,
        weapon,
        state: if returns_at.is_some() {
            "depleted"
        } else {
            "hit"
        },
        hits: if returns_at.is_some() {
            0
        } else {
            (progress + points - 1) / points
        },
        hits_required: (kind.hits + points - 1) / points,
        xp: 1,
        wear,
        items,
        counter,
        player: players::profile(&state, player_id).await?,
    }))
}

async fn milk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Target>,
) -> Result<Json<MilkReply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !valid_id(&input.object_id) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid object"));
    }
    let config = &creatures().milk;
    super::survival::settle(&state, player_id).await?;
    let terrain = super::movement::terrain(&state).await?;
    let cow = terrain
        .animal_by_id(&input.object_id, super::movement::unix_now_ms())
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "animal not found"))?;
    if cow.model != config.model {
        return Err(status(StatusCode::BAD_REQUEST, "animal gives no milk"));
    }
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
    if distance([x, y, z], cow.position) > config.reach_m + WALK_TOLERANCE_M {
        return Err(status(StatusCode::FORBIDDEN, "animal is out of reach"));
    }
    // One cow gives milk once in a while, however many characters ask at the same time.
    let milked: Option<i32> = sqlx::query_scalar("INSERT INTO creature_milked (world_id, object_id, available_at) VALUES ($1, $2, now() + make_interval(mins => $3)) ON CONFLICT (world_id, object_id) DO UPDATE SET available_at = EXCLUDED.available_at WHERE creature_milked.available_at <= now() RETURNING 1")
        .bind(state.world_id).bind(&input.object_id).bind(config.cooldown_minutes as i32)
        .fetch_optional(&mut *transaction).await?;
    if milked.is_none() {
        return Err(status(StatusCode::CONFLICT, "cow was milked recently"));
    }
    super::survival::add_water(&mut transaction, player_id, config.water).await?;
    transaction.commit().await?;
    Ok(Json(MilkReply {
        object_id: input.object_id,
        water: config.water,
        player: players::profile(&state, player_id).await?,
    }))
}
