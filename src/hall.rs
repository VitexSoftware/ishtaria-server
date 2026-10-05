//! The hall of fame: the ten most successful characters of the world, living or dead.
//! The score is `100 * level + 10 * days lived`; the wealth they reached is shown next to it.
//!
//! A dead character keeps their wealth only while the grave is untouched: once anyone has
//! taken something from it, the amount shown is zero.

use super::players::Error;
use super::AppState;
use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;
use sqlx::FromRow;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/hall-of-fame", get(hall_of_fame))
}

#[derive(Serialize, FromRow)]
struct Entry {
    name: String,
    level: i32,
    /// `100 * level + 10 * lived_days`.
    score: String,
    /// Wealth reached, as an exact decimal string; zero for a dead character whose grave was robbed.
    wealth: String,
    lived_days: String,
    alive: bool,
    /// Whether the grave of a dead character has been robbed.
    looted: bool,
}

async fn hall_of_fame(State(state): State<AppState>) -> Result<Json<Vec<Entry>>, Error> {
    Ok(Json(
        sqlx::query_as("SELECT name, level, (100::bigint * level + 10 * lived)::text AS score, wealth::text AS wealth, lived::text AS lived_days, alive, looted FROM (SELECT players.username AS name, players.level, CASE WHEN players.died_at IS NOT NULL AND coalesce(graves.looted_at IS NOT NULL, true) THEN 0 ELSE players.lifetime_gold END AS wealth, greatest(0, floor(extract(epoch FROM (coalesce(players.died_at, now()) - players.created_at)) / 86400))::bigint AS lived, players.died_at IS NULL AS alive, players.died_at IS NOT NULL AND coalesce(graves.looted_at IS NOT NULL, true) AS looted, players.created_at, players.id FROM players LEFT JOIN graves ON graves.player_id = players.id WHERE players.world_id = $1 AND players.banned_at IS NULL) ranked ORDER BY 100::bigint * level + 10 * lived DESC, wealth DESC, created_at, id LIMIT 10")
            .bind(state.world_id)
            .fetch_all(&state.pool)
            .await?,
    ))
}
