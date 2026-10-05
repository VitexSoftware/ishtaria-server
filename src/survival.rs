use super::{players, AppState};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use players::Error;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};

const REST_DELAY: f64 = 5.0;
const STAMINA_RECOVERY: f64 = 0.5;
const WATER_IDLE_RATE: f64 = 0.01;
const DEHYDRATION_DAMAGE: f64 = 0.2;

#[derive(FromRow)]
struct Reserves {
    stamina: f64,
    water: f64,
    health: f64,
    activity_seconds: f64,
    elapsed: f64,
    idle: f64,
}

async fn reserves(transaction: &mut Transaction<'_, Postgres>, id: i64) -> Result<Reserves, Error> {
    Ok(sqlx::query_as("SELECT stamina + stamina_fraction AS stamina, water + water_fraction AS water, health + health_fraction AS health, activity_seconds, greatest(0, extract(epoch FROM (clock_timestamp() - survival_updated_at)))::float8 AS elapsed, greatest(0, extract(epoch FROM (clock_timestamp() - last_active_at)))::float8 AS idle FROM players WHERE id = $1")
        .bind(id).fetch_one(&mut **transaction).await?)
}

async fn save_reserves(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
    value: &Reserves,
) -> Result<(), Error> {
    sqlx::query("UPDATE players SET stamina = floor($2)::integer, stamina_fraction = $2 - floor($2), water = floor($3)::integer, water_fraction = $3 - floor($3), health = floor($4)::integer, health_fraction = $4 - floor($4), activity_seconds = $5 WHERE id = $1")
        .bind(id).bind(value.stamina).bind(value.water).bind(value.health).bind(value.activity_seconds).execute(&mut **transaction).await?;
    Ok(())
}

async fn rest(transaction: &mut Transaction<'_, Postgres>, id: i64) -> Result<bool, Error> {
    let mut value = reserves(transaction, id).await?;
    let resting = value.elapsed.min((value.idle - REST_DELAY).max(0.0));
    value.stamina = (value.stamina + resting * STAMINA_RECOVERY).min(100.0);
    if resting > 0.0 {
        value.activity_seconds = 0.0;
    }
    let dehydrated = (value.elapsed - (value.water - 1.0).max(0.0) / WATER_IDLE_RATE).max(0.0);
    value.water = (value.water - value.elapsed * WATER_IDLE_RATE).max(0.0);
    value.health = (value.health - dehydrated * DEHYDRATION_DAMAGE).clamp(0.0, 100.0);
    save_reserves(transaction, id, &value).await?;
    sqlx::query("UPDATE players SET survival_updated_at = clock_timestamp() WHERE id = $1")
        .bind(id)
        .execute(&mut **transaction)
        .await?;
    if value.health < 1.0 {
        die(
            transaction,
            id,
            if value.water < 1.0 {
                "dehydration"
            } else {
                "exhaustion"
            },
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
}

pub(super) async fn activity(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
    seconds: f64,
    running: bool,
) -> Result<bool, Error> {
    let mut value = reserves(transaction, id).await?;
    let seconds = seconds.clamp(0.0, 0.25);
    let exertion = (value.activity_seconds + seconds - 10.0).clamp(0.0, seconds);
    value.activity_seconds = (value.activity_seconds + seconds).min(10.0);
    let stamina_rate = if running { 0.5 } else { 0.1 };
    let exhausted = if value.stamina < 1.0 {
        seconds
    } else {
        (exertion - (value.stamina - 1.0) / stamina_rate).max(0.0)
    };
    value.stamina = (value.stamina - exertion * stamina_rate).max(0.0);
    value.water = (value.water - seconds * if running { 0.09 } else { 0.015 }).max(0.0);
    value.health = (value.health - exhausted * 0.05).max(0.0);
    save_reserves(transaction, id, &value).await?;
    sqlx::query("UPDATE players SET last_active_at = clock_timestamp() WHERE id = $1")
        .bind(id)
        .execute(&mut **transaction)
        .await?;
    if value.health < 1.0 {
        die(transaction, id, "exhaustion").await?;
        return Ok(false);
    }
    Ok(true)
}

#[derive(Serialize, FromRow)]
pub(super) struct Item {
    pub item_id: String,
    name: String,
    quantity: String,
    calories: i32,
    capacity_bonus: i32,
    asset: Option<String>,
    /// For example `tool`, `weapon` or `food`; clients decide from it what can be equipped.
    category: String,
}

#[derive(Serialize, FromRow)]
pub(super) struct Inventory {
    pub capacity: i64,
    pub used: i64,
    pub items: Vec<Item>,
}

#[derive(Serialize, FromRow)]
pub(super) struct Life {
    uuid: String,
    alive: bool,
    born_at: String,
    age_days: String,
    last_ate_at: String,
    calories_consumed: String,
    lifetime_gold: String,
    death_cause: Option<String>,
}

#[derive(FromRow)]
struct LockedPlayer {
    id: i64,
    alive: bool,
    starving: bool,
}

#[derive(Serialize, FromRow)]
struct Grave {
    id: String,
    player_uuid: String,
    player_name: String,
    kind: String,
    cause: String,
    died_at: String,
    gold: String,
    position_x: Option<f64>,
    position_y: Option<f64>,
    position_z: Option<f64>,
    #[sqlx(skip)]
    items: Vec<Item>,
    #[sqlx(skip)]
    obituary: Option<Obituary>,
}

#[derive(Serialize, FromRow)]
pub(super) struct Memorial {
    id: String,
    player_name: String,
    kind: String,
    position: Vec<f64>,
}

pub(super) async fn memorials(
    state: &AppState,
    direction: [f64; 3],
) -> Result<Vec<Memorial>, Error> {
    Ok(sqlx::query_as("SELECT id::text, player_name, kind, ARRAY[position_x, position_y, position_z] AS position FROM graves WHERE world_id = $1 AND sqrt(position_x * position_x + position_y * position_y + position_z * position_z) BETWEEN 6363000 AND 6379000 AND (position_x * $2 + position_y * $3 + position_z * $4) / nullif(sqrt(position_x * position_x + position_y * position_y + position_z * position_z), 0) >= $5 ORDER BY id LIMIT 128")
        .bind(state.world_id).bind(direction[0]).bind(direction[1]).bind(direction[2]).bind((700.0_f64 / 6_371_000.0).cos()).fetch_all(&state.pool).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Eat {
    item_id: String,
}

#[derive(FromRow)]
struct Stackable {
    quantity: i64,
    stack_limit: i64,
    capacity_bonus: i32,
    multi_slot: bool,
    slot_cost: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Loot {
    item_id: String,
    quantity: String,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/eat", post(eat))
        .route("/graves/{id}", get(grave))
        .route("/graves/{id}/loot", post(loot))
}

pub(super) async fn life(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<Life, Error> {
    Ok(sqlx::query_as("SELECT uuid::text, died_at IS NULL AS alive, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS born_at, greatest(0, floor(extract(epoch FROM (now() - created_at)) / 86400))::bigint::text AS age_days, last_ate_at::text, calories_consumed::text, lifetime_gold::text, death_cause FROM players WHERE id = $1")
        .bind(id).fetch_one(&mut **transaction).await?)
}

pub(super) async fn slots(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<(i64, i64), Error> {
    Ok(sqlx::query_as("SELECT 100::bigint + 10::bigint * (level - 1) + coalesce((SELECT sum(capacity_bonus * quantity) FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = players.id), 0)::bigint AS capacity, coalesce((SELECT sum(CASE WHEN multi_slot THEN ((quantity + stack_limit - 1) / stack_limit) * slot_cost ELSE slot_cost END) FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = players.id), 0)::bigint AS used FROM players WHERE id = $1")
        .bind(id).fetch_one(&mut **transaction).await?)
}

pub(super) async fn inventory(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<Inventory, Error> {
    let (capacity, used) = slots(transaction, id).await?;
    let items = sqlx::query_as("SELECT item_id, name, quantity::text, calories, capacity_bonus, asset, category FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = $1 AND item_id <> 'gold' ORDER BY item_id")
        .bind(id).fetch_all(&mut **transaction).await?;
    Ok(Inventory {
        capacity,
        used,
        items,
    })
}

async fn lock(
    transaction: &mut Transaction<'_, Postgres>,
    world_id: i64,
    id: i64,
) -> Result<LockedPlayer, Error> {
    let mut player: LockedPlayer = sqlx::query_as("SELECT id, died_at IS NULL AS alive, last_ate_at <= now() - interval '7 days' AS starving FROM players WHERE world_id = $1 AND id = $2 FOR UPDATE")
        .bind(world_id).bind(id).fetch_one(&mut **transaction).await?;
    if player.alive && !player.starving {
        player.alive = rest(transaction, id).await?;
    }
    Ok(player)
}

async fn die(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
    cause: &str,
) -> Result<(), Error> {
    let grave_id: i64 = sqlx::query_scalar("INSERT INTO graves (world_id, player_id, player_uuid, player_name, cause, kind, lifetime_gold, position_x, position_y, position_z, lived_days, friends_count, born_at) SELECT world_id, id, uuid, username, $2, CASE WHEN lifetime_gold > 1000000 THEN 'mausoleum' WHEN lifetime_gold >= 1000 THEN 'monument' ELSE 'headstone' END, lifetime_gold, position_x, position_y, position_z, greatest(0, floor(extract(epoch FROM (now() - created_at)) / 86400))::bigint, (SELECT count(*) FROM player_friendships WHERE player_id = players.id OR friend_id = players.id), created_at FROM players WHERE id = $1 RETURNING id")
        .bind(id).bind(cause).fetch_one(&mut **transaction).await?;
    sqlx::query("INSERT INTO grave_inventory (grave_id, item_id, quantity) SELECT $1, item_id, quantity FROM player_inventory WHERE player_id = $2")
        .bind(grave_id).bind(id).execute(&mut **transaction).await?;
    sqlx::query("DELETE FROM player_inventory WHERE player_id = $1")
        .bind(id)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM player_equipment WHERE player_id = $1")
        .bind(id)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("UPDATE players SET died_at = now(), death_cause = $2, health = 0 WHERE id = $1")
        .bind(id)
        .bind(cause)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM player_sessions WHERE player_id = $1")
        .bind(id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

pub(super) async fn lock_alive(
    transaction: &mut Transaction<'_, Postgres>,
    world_id: i64,
    id: i64,
) -> Result<bool, Error> {
    let player = lock(transaction, world_id, id).await?;
    if player.alive && player.starving {
        die(transaction, id, "starvation").await?;
    }
    Ok(player.alive && !player.starving)
}

#[derive(Debug, Serialize, FromRow)]
pub(super) struct Obituary {
    name: String,
    born_at: String,
    lived_days: String,
    lifetime_gold: String,
    friends_count: String,
}

pub(super) async fn obituary(
    transaction: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<Obituary, Error> {
    Ok(sqlx::query_as("SELECT player_name AS name, to_char(born_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS born_at, lived_days::text, lifetime_gold::text, friends_count::text FROM graves WHERE player_id = $1")
        .bind(id).fetch_one(&mut **transaction).await?)
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(super) enum DamageCause {
    Disease,
    Fall,
    FallingTree,
    HostilePlayer,
}

#[allow(dead_code)]
pub(super) async fn apply_damage(
    state: &AppState,
    id: i64,
    damage: i32,
    cause: DamageCause,
) -> Result<(), Error> {
    if !(1..=100).contains(&damage) {
        return Err(Error::Status(StatusCode::BAD_REQUEST, "invalid damage"));
    }
    let mut transaction = state.pool.begin().await?;
    let player = lock(&mut transaction, state.world_id, id).await?;
    if player.alive {
        if player.starving {
            die(&mut transaction, id, "starvation").await?;
        } else {
            let health: i32 = sqlx::query_scalar("UPDATE players SET health = greatest(0, health - $2) WHERE id = $1 RETURNING health")
                .bind(id).bind(damage).fetch_one(&mut *transaction).await?;
            if health == 0 {
                let cause = match cause {
                    DamageCause::Disease => "disease",
                    DamageCause::Fall => "fall",
                    DamageCause::FallingTree => "falling_tree",
                    DamageCause::HostilePlayer => "hostile_player",
                };
                die(&mut transaction, id, cause).await?;
            }
        }
    }
    transaction.commit().await?;
    Ok(())
}

pub(super) async fn settle(state: &AppState, id: i64) -> Result<(), Error> {
    let mut transaction = state.pool.begin().await?;
    let player = lock(&mut transaction, state.world_id, id).await?;
    if player.alive && player.starving {
        die(&mut transaction, player.id, "starvation").await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub(super) async fn settle_world(state: &AppState) -> Result<(), Error> {
    let mut after = 0;
    loop {
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM players WHERE world_id = $1 AND died_at IS NULL AND id > $2 ORDER BY id LIMIT 100")
            .bind(state.world_id).bind(after).fetch_all(&state.pool).await?;
        if ids.is_empty() {
            return Ok(());
        }
        for id in ids {
            settle(state, id).await?;
            after = id;
        }
        tokio::task::yield_now().await;
    }
}

async fn eat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Eat>,
) -> Result<Json<players::Player>, Error> {
    let id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    settle(&state, id).await?;
    let mut transaction = state.pool.begin().await?;
    let player = lock(&mut transaction, state.world_id, id).await?;
    if !player.alive {
        return Err(Error::Status(StatusCode::GONE, "player is dead"));
    }
    if player.starving {
        die(&mut transaction, id, "starvation").await?;
        transaction.commit().await?;
        return Err(Error::Status(StatusCode::GONE, "player is dead"));
    }
    let food: Option<(i64, i32)> = sqlx::query_as("SELECT quantity, calories FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = $1 AND item_id = $2 AND calories > 0")
        .bind(id).bind(&input.item_id).fetch_optional(&mut *transaction).await?;
    let (quantity, calories) = food.ok_or(Error::Status(
        StatusCode::BAD_REQUEST,
        "edible item not found",
    ))?;
    if quantity == 1 {
        sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
            .bind(id)
            .bind(&input.item_id)
            .execute(&mut *transaction)
            .await?;
    } else {
        sqlx::query("UPDATE player_inventory SET quantity = quantity - 1 WHERE player_id = $1 AND item_id = $2").bind(id).bind(&input.item_id).execute(&mut *transaction).await?;
    }
    sqlx::query("UPDATE players SET health = least(100, health + CASE WHEN last_ate_at > now() - interval '6048 seconds' THEN 5 ELSE 0 END), last_ate_at = now(), calories_consumed = calories_consumed + $2, food = 100 WHERE id = $1")
        .bind(id).bind(i64::from(calories)).execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(Json(players::profile(&state, id).await?))
}

async fn require_reachable(
    transaction: &mut Transaction<'_, Postgres>,
    world_id: i64,
    player_id: i64,
    grave_id: i64,
) -> Result<(), Error> {
    let nearby: Option<bool> = sqlx::query_scalar("SELECT coalesce(players.position_x IS NOT NULL AND graves.position_x IS NOT NULL AND (players.position_x - graves.position_x)^2 + (players.position_y - graves.position_y)^2 + (players.position_z - graves.position_z)^2 <= 9, false) FROM graves JOIN players ON players.id = $3 WHERE graves.id = $1 AND graves.world_id = $2 FOR UPDATE OF graves")
        .bind(grave_id).bind(world_id).bind(player_id).fetch_optional(&mut **transaction).await?;
    if nearby != Some(true) {
        return Err(Error::Status(
            StatusCode::FORBIDDEN,
            "grave is not within reach",
        ));
    }
    Ok(())
}

async fn grave(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Grave>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    settle(&state, player_id).await?;
    let mut transaction = state.pool.begin().await?;
    let player = lock(&mut transaction, state.world_id, player_id).await?;
    if !player.alive || player.starving {
        return Err(Error::Status(StatusCode::CONFLICT, "player is dead"));
    }
    require_reachable(&mut transaction, state.world_id, player_id, id).await?;
    let mut grave: Grave = sqlx::query_as("SELECT id::text, player_uuid::text, player_name, kind, cause, died_at::text, coalesce((SELECT quantity FROM grave_inventory WHERE grave_id = graves.id AND item_id = 'gold'), 0)::text AS gold, position_x, position_y, position_z FROM graves WHERE world_id = $1 AND id = $2 FOR SHARE")
        .bind(state.world_id).bind(id).fetch_optional(&mut *transaction).await?
        .ok_or(Error::Status(StatusCode::NOT_FOUND, "grave not found"))?;
    grave.items = sqlx::query_as("SELECT item_id, name, quantity::text, calories, capacity_bonus, asset, category FROM grave_inventory JOIN item_types ON item_types.id = item_id WHERE grave_id = $1 AND item_id <> 'gold' ORDER BY item_id")
        .bind(id).fetch_all(&mut *transaction).await?;
    grave.obituary = Some(sqlx::query_as("SELECT player_name AS name, to_char(born_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS born_at, lived_days::text, lifetime_gold::text, friends_count::text FROM graves WHERE id = $1")
        .bind(id).fetch_one(&mut *transaction).await?);
    transaction.commit().await?;
    Ok(Json(grave))
}

async fn loot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(grave_id): Path<i64>,
    Json(input): Json<Loot>,
) -> Result<Json<players::Player>, Error> {
    let amount = input
        .quantity
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(Error::Status(StatusCode::BAD_REQUEST, "invalid quantity"))?;
    let id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    settle(&state, id).await?;
    let mut transaction = state.pool.begin().await?;
    let player = lock(&mut transaction, state.world_id, id).await?;
    if !player.alive || player.starving {
        return Err(Error::Status(StatusCode::CONFLICT, "player is dead"));
    }
    require_reachable(&mut transaction, state.world_id, id, grave_id).await?;
    let (capacity, used) = slots(&mut transaction, id).await?;
    let source: Option<Stackable> = sqlx::query_as("SELECT quantity, stack_limit, capacity_bonus, multi_slot, slot_cost::bigint AS slot_cost FROM grave_inventory JOIN item_types ON item_types.id = item_id WHERE grave_id = $1 AND item_id = $2")
        .bind(grave_id).bind(&input.item_id).fetch_optional(&mut *transaction).await?;
    let Stackable {
        quantity: available,
        stack_limit: limit,
        capacity_bonus: bonus,
        multi_slot,
        slot_cost,
    } = source.ok_or(if input.item_id == "gold" {
        Error::Status(
            StatusCode::CONFLICT,
            "insufficient gold or inventory capacity",
        )
    } else {
        Error::Status(StatusCode::NOT_FOUND, "item not found")
    })?;
    let owned: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2",
    )
    .bind(id)
    .bind(&input.item_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let total = owned
        .unwrap_or(0)
        .checked_add(amount)
        .ok_or(Error::Status(StatusCode::CONFLICT, "stack overflow"))?;
    let fits = if multi_slot {
        // Several slots of `limit` items each: only the additional slots must be free.
        let slots_for = |count: i64| (count / limit + i64::from(count % limit != 0)) * slot_cost;
        used + (slots_for(total) - slots_for(owned.unwrap_or(0))) <= capacity
    } else {
        total <= limit
            && (owned.is_some() || used + slot_cost <= capacity + i64::from(bonus) * amount)
    };
    if amount > available || !fits {
        return Err(Error::Status(
            StatusCode::CONFLICT,
            "insufficient items or inventory capacity",
        ));
    }
    sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, $2, $3) ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = EXCLUDED.quantity")
        .bind(id).bind(&input.item_id).bind(total).execute(&mut *transaction).await?;
    if amount == available {
        sqlx::query("DELETE FROM grave_inventory WHERE grave_id = $1 AND item_id = $2")
            .bind(grave_id)
            .bind(&input.item_id)
            .execute(&mut *transaction)
            .await?;
    } else {
        sqlx::query("UPDATE grave_inventory SET quantity = quantity - $3 WHERE grave_id = $1 AND item_id = $2").bind(grave_id).bind(&input.item_id).bind(amount).execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    Ok(Json(players::profile(&state, id).await?))
}
