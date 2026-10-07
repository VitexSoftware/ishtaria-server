//! Players in the neighbourhood and text chat (ADR 0007).
//!
//! The server relays a message and forgets it: a spoken line reaches the players standing
//! nearby as a short-lived event, a whisper goes to a friend and waits in the mailbox for
//! seven days when the friend is offline. No history and no language are stored.

use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A player is seen while the client has been active within this time.
const PRESENT_SECONDS: i32 = 30;
/// Players farther than this are neither listed nor hear a spoken line (metres).
const HEARING_M: f64 = 60.0;
const SIGHT_M: f64 = 150.0;
const MAX_NEARBY: i64 = 50;
const MAX_CHARACTERS: usize = 500;
/// Minimum time between two messages of one sender.
const CHAT_INTERVAL_MS: i32 = 1_000;
const MAILBOX_DAYS: i32 = 7;
const MAILBOX_MESSAGES: i64 = 100;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/nearby", get(nearby))
        .route("/chat/say", post(say))
        .route("/chat/whisper", post(whisper))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize, FromRow)]
struct Neighbour {
    name: String,
    character: String,
    flag: Option<String>,
    level: i32,
    x: f64,
    y: f64,
    z: f64,
}

/// Other living players that are present and within sight of the caller.
async fn nearby(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Neighbour>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let neighbours = sqlx::query_as(&format!("SELECT other.username AS name, other.character, other.flag, other.level, other.position_x AS x, other.position_y AS y, other.position_z AS z FROM players me JOIN players other ON other.world_id = me.world_id AND other.id <> me.id WHERE me.id = $1 AND me.position_x IS NOT NULL AND other.position_x IS NOT NULL AND other.died_at IS NULL AND other.banned_at IS NULL AND coalesce(greatest(other.last_seen_at, other.last_active_at) > clock_timestamp() - make_interval(secs => {PRESENT_SECONDS}), false) AND (other.position_x - me.position_x)^2 + (other.position_y - me.position_y)^2 + (other.position_z - me.position_z)^2 <= $2 ORDER BY (other.position_x - me.position_x)^2 + (other.position_y - me.position_y)^2 + (other.position_z - me.position_z)^2 LIMIT {MAX_NEARBY}"))
        .bind(player_id)
        .bind(SIGHT_M * SIGHT_M)
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(neighbours))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SayRequest {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WhisperRequest {
    to: String,
    text: String,
}

#[derive(Serialize)]
struct Delivered {
    delivered: i64,
}

/// A message is plain text: trimmed, not empty, at most 500 characters, no control characters.
fn clean(text: &str) -> Result<String, Error> {
    let text = text.trim();
    if text.is_empty()
        || text.chars().count() > MAX_CHARACTERS
        || text.chars().any(|character| character.is_control())
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid message"));
    }
    Ok(text.to_owned())
}

/// Lets the sender talk at most once per interval; the check and the stamp are one statement.
async fn throttle(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<(), Error> {
    let allowed: Option<i64> = sqlx::query_scalar(&format!("UPDATE players SET last_chat_at = clock_timestamp() WHERE id = $1 AND (last_chat_at IS NULL OR last_chat_at < clock_timestamp() - make_interval(secs => {}.0 / 1000)) RETURNING id", CHAT_INTERVAL_MS))
        .bind(player_id)
        .fetch_optional(&mut **transaction)
        .await?;
    allowed
        .map(|_| ())
        .ok_or_else(|| status(StatusCode::TOO_MANY_REQUESTS, "you are talking too fast"))
}

/// Speaks to the present players within hearing distance.
async fn say(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SayRequest>,
) -> Result<Json<Delivered>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let text = clean(&input.text)?;
    let mut transaction = state.pool.begin().await?;
    throttle(&mut transaction, player_id).await?;
    let delivered = sqlx::query(&format!("INSERT INTO player_events (recipient_id, kind, subject, body) SELECT other.id, 'say', me.username, $2 FROM players me JOIN players other ON other.world_id = me.world_id AND other.id <> me.id WHERE me.id = $1 AND me.position_x IS NOT NULL AND other.position_x IS NOT NULL AND other.died_at IS NULL AND other.banned_at IS NULL AND coalesce(greatest(other.last_seen_at, other.last_active_at) > clock_timestamp() - make_interval(secs => {PRESENT_SECONDS}), false) AND (other.position_x - me.position_x)^2 + (other.position_y - me.position_y)^2 + (other.position_z - me.position_z)^2 <= $3"))
        .bind(player_id)
        .bind(&text)
        .bind(HEARING_M * HEARING_M)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
    transaction.commit().await?;
    Ok(Json(Delivered {
        delivered: delivered as i64,
    }))
}

/// Whispers to a friend, now or from the mailbox when they are away.
async fn whisper(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WhisperRequest>,
) -> Result<(StatusCode, Json<Delivered>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let text = clean(&input.text)?;
    if input.to.is_empty() || input.to.chars().count() > 32 {
        return Err(status(StatusCode::BAD_REQUEST, "invalid name"));
    }
    let mut transaction = state.pool.begin().await?;
    throttle(&mut transaction, player_id).await?;
    // Only friends can be whispered to; a stranger gets the same answer as a missing name.
    let friend: Option<i64> = sqlx::query_scalar("SELECT other.id FROM players other JOIN player_friendships ON (player_friendships.player_id = $1 AND player_friendships.friend_id = other.id) OR (player_friendships.friend_id = $1 AND player_friendships.player_id = other.id) WHERE lower(other.username) = lower($2) AND other.world_id = $3 AND other.died_at IS NULL AND other.banned_at IS NULL")
        .bind(player_id)
        .bind(&input.to)
        .bind(state.world_id)
        .fetch_optional(&mut *transaction)
        .await?;
    let friend = friend.ok_or_else(|| status(StatusCode::NOT_FOUND, "friend not found"))?;
    // Serialise senders to one recipient so the mailbox bound holds under concurrency.
    sqlx::query("SELECT id FROM players WHERE id = $1 FOR UPDATE")
        .bind(friend)
        .execute(&mut *transaction)
        .await?;
    let waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM player_events WHERE recipient_id = $1 AND kind = 'whisper'",
    )
    .bind(friend)
    .fetch_one(&mut *transaction)
    .await?;
    if waiting >= MAILBOX_MESSAGES {
        return Err(status(
            StatusCode::CONFLICT,
            "the mailbox of the friend is full",
        ));
    }
    sqlx::query(&format!("INSERT INTO player_events (recipient_id, kind, subject, body, expires_at) VALUES ($1, 'whisper', (SELECT username FROM players WHERE id = $2), $3, now() + make_interval(days => {MAILBOX_DAYS}))"))
        .bind(friend)
        .bind(player_id)
        .bind(&text)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(Delivered { delivered: 1 })))
}
