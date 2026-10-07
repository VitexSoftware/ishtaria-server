//! Magic (ADR 0010): mana, scrolls and a few spells with effects the server implements.
//!
//! What a spell costs, how long it rests and which of the fixed effects it has is data in
//! `etc/spells.json`. The server checks that the spell is known, its cooldown, mana and (for a
//! bolt) the target and range, then does the whole effect in one transaction with the
//! character locked. Mana regenerates by the clock and is stored with the time it was changed.

use super::creatures::{self, SpellStrike};
use super::players::{self, Error};
use super::shops::take;
use super::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Effect {
    /// Health back (never above 100).
    Heal { health: i32 },
    /// Stamina back (never above 100).
    Refresh { stamina: i32 },
    /// Less damage from creatures for a while.
    Ward { seconds: i32 },
    /// Work points against an animal within range, like a weapon swing.
    Bolt { points: i32, range_m: f64 },
}

#[derive(Deserialize)]
struct Spell {
    id: String,
    name: String,
    scroll: String,
    mana: i32,
    cooldown_ms: i64,
    min_level: i32,
    effect: Effect,
}

#[derive(Deserialize)]
struct Spells {
    version: u32,
    mana_max: f64,
    mana_regen_per_second: f64,
    ward_damage_factor: f64,
    spells: Vec<Spell>,
}

fn spells() -> &'static Spells {
    static CONFIG: OnceLock<Spells> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Spells = serde_json::from_str(include_str!("../etc/spells.json"))
            .expect("bundled spells must be valid");
        assert_eq!(config.version, 1);
        assert!((0.0..=1.0).contains(&config.ward_damage_factor));
        config
    })
}

/// The share of creature damage that gets through a ward.
pub(super) fn ward_damage_factor() -> f64 {
    spells().ward_damage_factor
}

/// Items the configuration refers to, for checking them against the item catalog.
#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    spells()
        .spells
        .iter()
        .map(|spell| spell.scroll.clone())
        .collect()
}

/// SQL for the mana a character has now (the caller supplies the row alias).
pub(super) fn mana_now_sql() -> String {
    let config = spells();
    format!(
        "least({max}, mana + greatest(0, extract(epoch FROM (clock_timestamp() - mana_updated_at))) * {rate})",
        max = config.mana_max,
        rate = config.mana_regen_per_second
    )
}

pub(super) fn mana_max() -> i32 {
    spells().mana_max as i32
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/spells", get(known))
        .route("/players/me/learn", post(learn))
        .route("/players/me/cast", post(cast))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize)]
struct SpellView {
    id: String,
    name: String,
    mana: i32,
    cooldown_ms: i64,
    /// What the spell does: `heal`, `refresh`, `ward` or `bolt`.
    effect: &'static str,
    /// How far a bolt reaches (metres), for the others zero.
    range_m: f64,
    /// Milliseconds until the spell can be cast again.
    ready_in_ms: i64,
}

#[derive(Serialize)]
struct Known {
    mana: i32,
    mana_max: i32,
    spells: Vec<SpellView>,
}

fn effect_name(effect: &Effect) -> &'static str {
    match effect {
        Effect::Heal { .. } => "heal",
        Effect::Refresh { .. } => "refresh",
        Effect::Ward { .. } => "ward",
        Effect::Bolt { .. } => "bolt",
    }
}

fn range_of(effect: &Effect) -> f64 {
    match effect {
        Effect::Bolt { range_m, .. } => *range_m,
        _ => 0.0,
    }
}

/// The spells the character has learned, in the order of the configuration.
async fn known(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Known>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT spell_id, greatest(0, extract(epoch FROM (coalesce(cooldown_until, now()) - now())) * 1000)::bigint FROM player_spells WHERE player_id = $1")
        .bind(player_id).fetch_all(&state.pool).await?;
    let mana: f64 = sqlx::query_scalar(&format!(
        "SELECT {}::float8 FROM players WHERE id = $1",
        mana_now_sql()
    ))
    .bind(player_id)
    .fetch_one(&state.pool)
    .await?;
    let spells = spells()
        .spells
        .iter()
        .filter_map(|spell| {
            let (_, ready) = rows.iter().find(|(id, _)| id == &spell.id)?;
            Some(SpellView {
                id: spell.id.clone(),
                name: spell.name.clone(),
                mana: spell.mana,
                cooldown_ms: spell.cooldown_ms,
                effect: effect_name(&spell.effect),
                range_m: range_of(&spell.effect),
                ready_in_ms: *ready,
            })
        })
        .collect();
    Ok(Json(Known {
        mana: mana.floor() as i32,
        mana_max: mana_max(),
        spells,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Learn {
    item_id: String,
}

#[derive(Serialize)]
struct Learned {
    spell: String,
    player: players::Player,
}

/// Reads a scroll: the spell is learned for good and the scroll is used up.
async fn learn(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Learn>,
) -> Result<Json<Learned>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let spell = spells()
        .spells
        .iter()
        .find(|spell| spell.scroll == input.item_id)
        .ok_or_else(|| status(StatusCode::CONFLICT, "this is not a scroll"))?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let level: i32 = sqlx::query_scalar("SELECT level FROM players WHERE id = $1")
        .bind(player_id)
        .fetch_one(&mut *transaction)
        .await?;
    if level < spell.min_level {
        return Err(status(StatusCode::FORBIDDEN, "level too low"));
    }
    let new: Option<i64> = sqlx::query_scalar("INSERT INTO player_spells (player_id, spell_id) VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING 1::bigint")
        .bind(player_id).bind(&spell.id).fetch_optional(&mut *transaction).await?;
    if new.is_none() {
        return Err(status(StatusCode::CONFLICT, "spell already known"));
    }
    take(&mut transaction, player_id, &spell.scroll, 1).await?;
    super::experience::grant(&mut transaction, player_id, 5).await?;
    transaction.commit().await?;
    Ok(Json(Learned {
        spell: spell.id.clone(),
        player: players::profile(&state, player_id).await?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cast {
    spell: String,
    /// The animal a bolt is aimed at.
    #[serde(default)]
    object_id: Option<String>,
}

#[derive(Serialize)]
struct Cast_ {
    spell: String,
    effect: &'static str,
    strike: Option<SpellStrike>,
    /// The model of the animal that was hit.
    target: Option<String>,
    player: players::Player,
}

fn distance(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// An animal may have walked this far since the client last saw it.
const WALK_TOLERANCE_M: f64 = 3.0;

async fn cast(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Cast>,
) -> Result<Json<Cast_>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let spell = spells()
        .spells
        .iter()
        .find(|spell| spell.id == input.spell)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown spell"))?;
    // A bolt needs its animal; the lookup happens before the locks.
    let target = match (&spell.effect, input.object_id.as_deref()) {
        (Effect::Bolt { .. }, Some(id)) => Some(creatures::spell_target(&state, id).await?),
        (Effect::Bolt { .. }, None) => {
            return Err(status(StatusCode::BAD_REQUEST, "this spell needs a target"))
        }
        _ => None,
    };
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let row: Option<(f64, f64, f64)> = sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let caster = row
        .map(|(x, y, z)| [x, y, z])
        .ok_or_else(|| status(StatusCode::CONFLICT, "player position unavailable"))?;
    let rest: Option<Option<i64>> = sqlx::query_scalar("SELECT (extract(epoch FROM (cooldown_until - clock_timestamp())) * 1000)::bigint FROM player_spells WHERE player_id = $1 AND spell_id = $2 FOR UPDATE")
        .bind(player_id).bind(&spell.id).fetch_optional(&mut *transaction).await?;
    let Some(rest) = rest else {
        return Err(status(StatusCode::FORBIDDEN, "spell not known"));
    };
    if rest.is_some_and(|milliseconds| milliseconds > 0) {
        return Err(status(
            StatusCode::TOO_MANY_REQUESTS,
            "the spell is resting",
        ));
    }
    let mana: f64 = sqlx::query_scalar(&format!(
        "SELECT {}::float8 FROM players WHERE id = $1",
        mana_now_sql()
    ))
    .bind(player_id)
    .fetch_one(&mut *transaction)
    .await?;
    if mana < f64::from(spell.mana) {
        return Err(status(StatusCode::CONFLICT, "not enough mana"));
    }
    let mut strike = None;
    let mut target_model = None;
    match &spell.effect {
        Effect::Heal { health } => {
            sqlx::query("UPDATE players SET health = least(100, health + $2) WHERE id = $1")
                .bind(player_id)
                .bind(health)
                .execute(&mut *transaction)
                .await?;
        }
        Effect::Refresh { stamina } => {
            sqlx::query("UPDATE players SET stamina = least(100, stamina + $2) WHERE id = $1")
                .bind(player_id)
                .bind(stamina)
                .execute(&mut *transaction)
                .await?;
        }
        Effect::Ward { seconds } => {
            sqlx::query("UPDATE players SET warded_until = clock_timestamp() + make_interval(secs => $2) WHERE id = $1")
                .bind(player_id).bind(seconds).execute(&mut *transaction).await?;
        }
        Effect::Bolt { points, range_m } => {
            let (target, terrain) = target.as_ref().expect("a bolt has a target");
            if distance(caster, target.position) > range_m + WALK_TOLERANCE_M {
                return Err(status(StatusCode::FORBIDDEN, "target is out of range"));
            }
            let hit = creatures::spell_strike(
                state.world_id,
                &mut transaction,
                terrain,
                player_id,
                target,
                caster,
                *points,
            )
            .await?;
            if hit.counter.as_ref().is_some_and(|counter| counter.died) {
                let notice = super::survival::obituary(&mut transaction, player_id).await?;
                transaction.commit().await?;
                return Err(Error::Obituary(notice));
            }
            target_model = Some(target.model.clone());
            strike = Some(hit);
        }
    }
    sqlx::query("UPDATE players SET mana = $2, mana_updated_at = clock_timestamp() WHERE id = $1")
        .bind(player_id)
        .bind(mana - f64::from(spell.mana))
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE player_spells SET cooldown_until = clock_timestamp() + make_interval(secs => $3) WHERE player_id = $1 AND spell_id = $2")
        .bind(player_id).bind(&spell.id).bind(spell.cooldown_ms as f64 / 1000.0)
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(Json(Cast_ {
        spell: spell.id.clone(),
        effect: effect_name(&spell.effect),
        strike,
        target: target_model,
        player: players::profile(&state, player_id).await?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_spells_are_consistent() {
        let config = spells();
        assert!(config.mana_max > 0.0 && config.mana_regen_per_second > 0.0);
        let mut ids: Vec<_> = config
            .spells
            .iter()
            .map(|spell| spell.id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), config.spells.len(), "spell ids are unique");
        for spell in &config.spells {
            assert!(spell.scroll.starts_with("scroll_"), "{}", spell.id);
            assert!(0 < spell.mana && f64::from(spell.mana) <= config.mana_max);
            assert!(spell.cooldown_ms > 0 && spell.min_level >= 1);
            match &spell.effect {
                Effect::Heal { health } => assert!((1..=100).contains(health)),
                Effect::Refresh { stamina } => assert!((1..=100).contains(stamina)),
                Effect::Ward { seconds } => assert!((1..=60).contains(seconds)),
                Effect::Bolt { points, range_m } => {
                    assert!((1..=10).contains(points) && *range_m > 0.0 && *range_m <= 30.0)
                }
            }
        }
    }
}
