//! Accounts and characters: registration, login (Argon2id), expiring sessions, the profile that
//! every action answers with, and equipment.

use super::movement::Position;
use super::{ApiError, AppState};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use std::time::Duration;

static PASSWORD_JOBS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
    character: Option<String>,
}

#[derive(Serialize, FromRow)]
pub(super) struct Stats {
    gold: String,
    health: i32,
    stamina: i32,
    food: i32,
    water: i32,
    /// Mana now (it regenerates by the clock) and its maximum.
    mana: i32,
    mana_max: i32,
    level: i32,
    experience: i64,
    /// Experience at which the current level began, and at which the next one does.
    #[sqlx(skip)]
    level_experience: i64,
    #[sqlx(skip)]
    next_level_experience: i64,
    /// `100 * level + 10 * days lived`, the number the hall of fame ranks by.
    score: String,
}

impl Stats {
    fn with_progress(mut self) -> Self {
        self.level_experience = super::experience::experience_for(self.level);
        self.next_level_experience = super::experience::experience_for(self.level + 1);
        self
    }
}

pub(super) async fn stats(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> Result<Stats, Error> {
    let stats: Stats = sqlx::query_as(&format!("SELECT coalesce((SELECT quantity FROM player_inventory WHERE player_id = players.id AND item_id = 'gold'), 0)::text AS gold, health, stamina, greatest(0, least(100, ceil(100 * (1 - extract(epoch FROM (now() - last_ate_at)) / 604800))))::integer AS food, water, floor({mana})::integer AS mana, {mana_max} AS mana_max, level, experience, (100::bigint * level + 10 * greatest(0, floor(extract(epoch FROM (coalesce(died_at, now()) - created_at)) / 86400))::bigint)::text AS score FROM players WHERE id = $1", mana = super::magic::mana_now_sql(), mana_max = super::magic::mana_max()))
        .bind(id).fetch_one(&mut **transaction).await?;
    Ok(stats.with_progress())
}

#[derive(Serialize, FromRow)]
pub(super) struct Player {
    username: String,
    character: String,
    /// Flag the player chose to show next to their name (two capital letters), if any.
    flag: Option<String>,
    #[sqlx(flatten)]
    stats: Stats,
    #[sqlx(skip)]
    life: Option<super::survival::Life>,
    #[sqlx(skip)]
    inventory: Option<super::survival::Inventory>,
    #[sqlx(skip)]
    position: Option<Position>,
    #[sqlx(skip)]
    equipment: Equipment,
}

/// What the character holds in hand; `None` when nothing is held or the item is no longer owned.
#[derive(Serialize, Default)]
pub(super) struct Equipment {
    hand: Option<String>,
    /// The shield, if any.
    offhand: Option<String>,
    /// The armour worn on the body and on the hands.
    body: Option<String>,
    hands: Option<String>,
    /// Percent of the damage that the worn armour takes away.
    defense: i32,
    /// True while a block lasts.
    blocking: bool,
}

#[derive(Serialize)]
struct Session {
    token: String,
    player: Player,
}

#[derive(Serialize)]
struct DeathNotice {
    obituary: super::survival::Obituary,
}

#[derive(Debug)]
pub(super) enum Error {
    Database(sqlx::Error),
    Status(StatusCode, &'static str),
    Obituary(super::survival::Obituary),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Self::Database(error) => ApiError(error).into_response(),
            Self::Status(status, message) => (status, message).into_response(),
            Self::Obituary(obituary) => {
                (StatusCode::GONE, Json(DeathNotice { obituary })).into_response()
            }
        }
    }
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players", post(register))
        .route("/players/login", post(login))
        .route("/players/me", get(me))
        .route("/players/me/move", post(super::movement::walk))
        .route("/players/me/flag", put(set_flag))
        .route("/players/session", delete(logout))
        .merge(super::survival::routes())
        .layer(DefaultBodyLimit::max(2048))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                response
            },
        ))
}

fn validate(credentials: &Credentials) -> Result<(), Error> {
    if !(3..=32).contains(&credentials.username.len())
        || !credentials
            .username
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'_' || value == b'-')
        || (!credentials.password.is_empty() && !(8..=128).contains(&credentials.password.len()))
    {
        return Err(Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid credentials format",
        ));
    }
    Ok(())
}

fn valid_character(character: &str) -> bool {
    matches!(
        character,
        "protagonists/criminalMaleA"
            | "protagonists/cyborgFemaleA"
            | "protagonists/skaterFemaleA"
            | "protagonists/skaterMaleA"
            | "retro/humanFemaleA"
            | "retro/humanMaleA"
            | "retro/zombieFemaleA"
            | "retro/zombieMaleA"
            | "survivors/survivorFemaleA"
            | "survivors/survivorMaleB"
            | "survivors/zombieA"
            | "survivors/zombieC"
            | "quaternius/adventurer"
            | "quaternius/adventurer_woman"
            | "quaternius/hooded_adventurer_woman"
            | "quaternius/character_animated"
            | "quaternius/hoodie_character"
            | "quaternius/punk"
            | "quaternius/punk_woman"
            | "quaternius/animated_woman"
            | "quaternius/animated_woman_2"
            | "quaternius/suit_woman"
            | "quaternius/worker_woman"
            | "quaternius/soldier_woman"
            | "quaternius/sci_fi_woman"
            | "quaternius/witch"
    )
}

async fn password_job(password: String, stored: Option<String>) -> Result<String, Error> {
    // Hashing a password is deliberately expensive, so only a few run at a time. A burst of logins
    // (the start of an event) waits its turn for a moment; only a server that stays busy refuses.
    let busy = || Error::Status(StatusCode::TOO_MANY_REQUESTS, "authentication busy");
    let permit = tokio::time::timeout(Duration::from_secs(3), PASSWORD_JOBS.acquire())
        .await
        .map_err(|_| busy())?
        .map_err(|_| busy())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let engine = Argon2::default();
        if let Some(stored) = stored {
            let parsed = PasswordHash::new(&stored)
                .map_err(|_| Error::Status(StatusCode::UNAUTHORIZED, "invalid credentials"))?;
            engine
                .verify_password(password.as_bytes(), &parsed)
                .map_err(|_| Error::Status(StatusCode::UNAUTHORIZED, "invalid credentials"))?;
            Ok(stored)
        } else {
            engine
                .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
                .map(|hash| hash.to_string())
                .map_err(|_| {
                    Error::Status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "authentication unavailable",
                    )
                })
        }
    })
    .await
    .map_err(|_| {
        Error::Status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication unavailable",
        )
    })?
}

pub(super) async fn profile(state: &AppState, player_id: i64) -> Result<Player, Error> {
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let mut player: Player = sqlx::query_as(&format!("SELECT username, character, flag, coalesce((SELECT quantity FROM player_inventory WHERE player_id = players.id AND item_id = 'gold'), 0)::text AS gold, health, stamina, greatest(0, least(100, ceil(100 * (1 - extract(epoch FROM (now() - last_ate_at)) / 604800))))::integer AS food, water, floor({mana})::integer AS mana, {mana_max} AS mana_max, level, experience, (100::bigint * level + 10 * greatest(0, floor(extract(epoch FROM (coalesce(died_at, now()) - created_at)) / 86400))::bigint)::text AS score FROM players WHERE id = $1 AND world_id = $2 FOR SHARE", mana = super::magic::mana_now_sql(), mana_max = super::magic::mana_max()))
        .bind(player_id).bind(state.world_id).fetch_one(&mut *transaction).await?;
    player.stats = player.stats.with_progress();
    player.life = Some(super::survival::life(&mut transaction, player_id).await?);
    player.inventory = Some(super::survival::inventory(&mut transaction, player_id).await?);
    player.position = sqlx::query_as("SELECT position_x AS x, position_y AS y, position_z AS z, movement_sequence::text AS sequence, movement_airborne AS airborne, movement_support AS on_object FROM players WHERE id = $1 AND position_x IS NOT NULL")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    #[allow(clippy::type_complexity)]
    let held: Option<(Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as("SELECT CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = hand) THEN hand END, CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = offhand) THEN offhand END, CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = body) THEN body END, CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = hands) THEN hands END FROM player_equipment WHERE player_id = $1")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    (
        player.equipment.hand,
        player.equipment.offhand,
        player.equipment.body,
        player.equipment.hands,
    ) = held.unwrap_or_default();
    player.equipment.defense = [&player.equipment.body, &player.equipment.hands]
        .into_iter()
        .flatten()
        .filter_map(|item| super::combat::armor(item))
        .map(|armor| armor.defense)
        .sum();
    player.equipment.blocking = player.equipment.offhand.is_some()
        && sqlx::query_scalar(
            "SELECT coalesce(blocked_until > now(), false) FROM players WHERE id = $1",
        )
        .bind(player_id)
        .fetch_one(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(player)
}

async fn ensure_position(
    state: &AppState,
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<(), Error> {
    let (world_id, existing): (i64, Option<f64>) =
        sqlx::query_as("SELECT world_id, position_x FROM players WHERE id = $1 FOR UPDATE")
            .bind(player_id)
            .fetch_one(&mut **transaction)
            .await?;
    if existing.is_some() {
        return Ok(());
    }
    // A datadisk may say where new characters appear.
    if let Some(position) = super::story::world::spawn_position(state).await {
        sqlx::query("UPDATE players SET position_x = $2, position_y = $3, position_z = $4 WHERE id = $1 AND position_x IS NULL")
            .bind(player_id).bind(position[0]).bind(position[1]).bind(position[2])
            .execute(&mut **transaction).await?;
        return Ok(());
    }
    let unavailable = || Error::Status(StatusCode::SERVICE_UNAVAILABLE, "no safe spawn available");
    let (size, pixels, seed, sha256): (i32, Vec<u8>, String, String) = sqlx::query_as(
        "SELECT face_size, pixels, seed, sha256 FROM heightmaps WHERE world_id = $1",
    )
    .bind(world_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(unavailable)?;
    let position = tokio::task::spawn_blocking(move || {
        super::environment::spawn_position(size as usize, &pixels, &seed, &sha256)
    })
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    sqlx::query("UPDATE players SET position_x = $2, position_y = $3, position_z = $4 WHERE id = $1 AND position_x IS NULL")
        .bind(player_id).bind(position[0]).bind(position[1]).bind(position[2])
        .execute(&mut **transaction).await?;
    Ok(())
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|value| format!("{value:02x}")).collect()
}

async fn register(
    State(state): State<AppState>,
    Json(credentials): Json<Credentials>,
) -> Result<(StatusCode, Json<Session>), Error> {
    validate(&credentials)?;
    let character = credentials
        .character
        .as_deref()
        .unwrap_or("retro/humanMaleA");
    if !valid_character(character) {
        return Err(Error::Status(StatusCode::BAD_REQUEST, "invalid character"));
    }
    let password_hash = password_job(credentials.password, None).await?;
    let mut transaction = state.pool.begin().await?;
    let player_id: Option<i64> = sqlx::query_scalar("INSERT INTO players (world_id, username, password_hash, character, created_at) SELECT $1, $2, $3, $4, greatest(clock_timestamp(), coalesce(max(created_at) + interval '1 microsecond', clock_timestamp())) FROM players WHERE world_id = $1 AND lower(username) = lower($2) ON CONFLICT DO NOTHING RETURNING id")
        .bind(state.world_id).bind(credentials.username).bind(password_hash).bind(character).fetch_optional(&mut *transaction).await?;
    let player_id = player_id.ok_or(Error::Status(
        StatusCode::CONFLICT,
        "username already exists",
    ))?;
    ensure_position(&state, &mut transaction, player_id).await?;
    let token = new_token();
    sqlx::query("INSERT INTO player_sessions (token_hash, player_id) VALUES ($1, $2)")
        .bind(Sha256::digest(token.as_bytes()).to_vec())
        .bind(player_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Session {
            token,
            player: profile(&state, player_id).await?,
        }),
    ))
}

async fn login(
    State(state): State<AppState>,
    Json(credentials): Json<Credentials>,
) -> Result<Json<Session>, Error> {
    validate(&credentials)?;
    let stored: Option<(i64, String, bool)> = sqlx::query_as(
        "SELECT id, password_hash, banned_at IS NOT NULL FROM players WHERE world_id = $1 AND lower(username) = lower($2) ORDER BY (died_at IS NULL) DESC, created_at DESC, id DESC LIMIT 1",
    )
    .bind(state.world_id)
    .bind(credentials.username)
    .fetch_optional(&state.pool)
    .await?;
    let Some((player_id, password_hash, banned)) = stored else {
        password_job(credentials.password, None).await?;
        return Err(Error::Status(
            StatusCode::UNAUTHORIZED,
            "invalid credentials",
        ));
    };
    password_job(credentials.password, Some(password_hash)).await?;
    if banned {
        return Err(Error::Status(StatusCode::FORBIDDEN, "account banned"));
    }
    let token = new_token();
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let obituary = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(obituary));
    }
    ensure_position(&state, &mut transaction, player_id).await?;
    sqlx::query("DELETE FROM player_sessions WHERE player_id = $1 AND expires_at <= now()")
        .bind(player_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO player_sessions (token_hash, player_id) VALUES ($1, $2)")
        .bind(Sha256::digest(token.as_bytes()).to_vec())
        .bind(player_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(Json(Session {
        token,
        player: profile(&state, player_id).await?,
    }))
}

pub(super) fn token_hash(headers: &HeaderMap) -> Result<Vec<u8>, Error> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| value.len() == 64 && value.bytes().all(|value| value.is_ascii_hexdigit()))
        .ok_or(Error::Status(
            StatusCode::UNAUTHORIZED,
            "authentication required",
        ))?;
    Ok(Sha256::digest(token.as_bytes()).to_vec())
}

pub(super) async fn player_id(state: &AppState, hash: &[u8]) -> Result<i64, Error> {
    sqlx::query_scalar("SELECT players.id FROM player_sessions JOIN players ON players.id = player_sessions.player_id WHERE token_hash = $1 AND expires_at > now() AND world_id = $2 AND died_at IS NULL AND banned_at IS NULL")
        .bind(hash).bind(state.world_id).fetch_optional(&state.pool).await?
        .ok_or(Error::Status(StatusCode::UNAUTHORIZED, "session expired"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FlagRequest {
    flag: Option<String>,
}

/// Shows (or hides, with `null`) the player's flag. A display choice only.
async fn set_flag(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<FlagRequest>,
) -> Result<Json<Player>, Error> {
    let player_id = player_id(&state, &token_hash(&headers)?).await?;
    if request
        .flag
        .as_deref()
        .is_some_and(|flag| flag.len() != 2 || !flag.bytes().all(|byte| byte.is_ascii_uppercase()))
    {
        return Err(Error::Status(StatusCode::BAD_REQUEST, "invalid flag"));
    }
    sqlx::query("UPDATE players SET flag = $2 WHERE id = $1")
        .bind(player_id)
        .bind(&request.flag)
        .execute(&state.pool)
        .await?;
    Ok(Json(profile(&state, player_id).await?))
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Player>, Error> {
    let player_id = player_id(&state, &token_hash(&headers)?).await?;
    Ok(Json(profile(&state, player_id).await?))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, Error> {
    let hash = token_hash(&headers)?;
    player_id(&state, &hash).await?;
    sqlx::query("DELETE FROM player_sessions WHERE token_hash = $1")
        .bind(hash)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
