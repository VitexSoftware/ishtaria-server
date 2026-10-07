//! Fishing with a rod from the shore.
//!
//! The server checks the rod in hand, water within reach, stamina and the rate, rolls the
//! catch itself and wears the rod. The sea and fresh water give the same fish.

use super::gathering::{add_items, Gained};
use super::movement::WaterNear;
use super::placing::config;
use super::players::{self, Error};
use super::AppState;
use axum::{extract::State, http::HeaderMap, http::StatusCode, routing::post, Json, Router};
use rand::{rngs::OsRng, Rng};
use serde::Serialize;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/players/me/fish", post(fish))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize)]
struct Reply {
    caught: bool,
    items: Vec<Gained>,
    wear: Option<super::durability::Wear>,
    player: players::Player,
}

async fn fish(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Reply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let fishing = &config().fishing;
    let terrain = super::movement::terrain(&state).await?;
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
    let rod: Option<String> = sqlx::query_scalar("SELECT hand FROM player_equipment WHERE player_id = $1 AND hand = 'fishing_rod' AND EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = 'fishing_rod')")
        .bind(player_id).fetch_optional(&mut *transaction).await?.flatten();
    if rod.is_none() {
        return Err(status(StatusCode::CONFLICT, "required tool missing"));
    }
    if since.is_some_and(|seconds| seconds * 1000.0 < fishing.cooldown_ms as f64) {
        return Err(status(StatusCode::TOO_MANY_REQUESTS, "too fast"));
    }
    if stamina < fishing.stamina_cost {
        return Err(status(StatusCode::CONFLICT, "too exhausted"));
    }
    if terrain.water_near([x, y, z], fishing.water_reach_m) == WaterNear::None {
        return Err(status(StatusCode::CONFLICT, "no water within reach"));
    }
    sqlx::query("UPDATE players SET stamina = stamina - $2, last_gathered_at = clock_timestamp() WHERE id = $1")
        .bind(player_id).bind(fishing.stamina_cost).execute(&mut *transaction).await?;
    let wear = super::durability::wear(&mut transaction, player_id, "fishing_rod", 1).await?;
    let caught = OsRng.gen_bool(fishing.catch_chance);
    let mut items = Vec::new();
    if caught {
        items = add_items(&mut transaction, player_id, &[(fishing.catch.clone(), 1)]).await?;
        super::experience::grant(&mut transaction, player_id, 1).await?;
    }
    transaction.commit().await?;
    Ok(Json(Reply {
        caught,
        items,
        wear,
        player: players::profile(&state, player_id).await?,
    }))
}
