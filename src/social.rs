//! Friends and the short-lived events of friends.
//!
//! A friendship is mutual: one player sends a request and the other accepts. A friend is
//! listed with the name and the world they are in, and whether they are online (they are
//! while their client polls `/events`). Events such as "a friend reached a level" are
//! delivered once and forgotten; the server keeps no history.

use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

const MAX_FRIENDS: i64 = 50;
const MAX_OUTGOING: i64 = 20;
const ONLINE_SECONDS: i32 = 30;
/// Events not fetched within this time are dropped.
const EVENT_LIFETIME_SECONDS: i32 = 120;
const MAX_EVENTS: i64 = 50;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/friends", get(list_friends))
        .route("/friends/{name}", delete(remove_friend))
        .route("/friends/requests", get(list_requests).post(request_friend))
        .route("/friends/requests/{id}/accept", post(accept_request))
        .route("/friends/requests/{id}", delete(drop_request))
        .route("/events", get(events))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize, FromRow)]
struct Friend {
    name: String,
    level: i32,
    /// The flag the friend chose to show, if any.
    flag: Option<String>,
    online: bool,
    /// The world the friend is in; for now always this world.
    #[sqlx(skip)]
    world: String,
}

async fn list_friends(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Friend>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let world: String = sqlx::query_scalar("SELECT server_name FROM worlds WHERE id = $1")
        .bind(state.world_id)
        .fetch_one(&state.pool)
        .await?;
    let mut friends: Vec<Friend> = sqlx::query_as(&format!("SELECT other.username AS name, other.level, other.flag, coalesce(other.died_at IS NULL AND other.last_seen_at > now() - make_interval(secs => {ONLINE_SECONDS}), false) AS online FROM player_friendships JOIN players other ON other.id = CASE WHEN player_friendships.player_id = $1 THEN player_friendships.friend_id ELSE player_friendships.player_id END WHERE (player_friendships.player_id = $1 OR player_friendships.friend_id = $1) AND other.died_at IS NULL ORDER BY online DESC, lower(other.username) LIMIT {MAX_FRIENDS}"))
        .bind(player_id).fetch_all(&state.pool).await?;
    for friend in &mut friends {
        friend.world.clone_from(&world);
    }
    Ok(Json(friends))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    username: String,
}

#[derive(Serialize)]
struct Outcome {
    status: &'static str,
}

fn ordered(first: i64, second: i64) -> (i64, i64) {
    (first.min(second), first.max(second))
}

async fn friend_count(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<i64, Error> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM player_friendships WHERE player_id = $1 OR friend_id = $1",
    )
    .bind(player_id)
    .fetch_one(&mut **transaction)
    .await?)
}

/// Makes two players friends, if neither has too many.
async fn befriend(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    first: i64,
    second: i64,
) -> Result<(), Error> {
    if friend_count(transaction, first).await? >= MAX_FRIENDS
        || friend_count(transaction, second).await? >= MAX_FRIENDS
    {
        return Err(status(StatusCode::CONFLICT, "too many friends"));
    }
    let (low, high) = ordered(first, second);
    sqlx::query("INSERT INTO player_friendships (player_id, friend_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(low).bind(high).execute(&mut **transaction).await?;
    sqlx::query("DELETE FROM friend_requests WHERE (from_id = $1 AND to_id = $2) OR (from_id = $2 AND to_id = $1)")
        .bind(first).bind(second).execute(&mut **transaction).await?;
    Ok(())
}

async fn request_friend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Named>,
) -> Result<(StatusCode, Json<Outcome>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if input.username.is_empty() || input.username.chars().count() > 32 {
        return Err(status(StatusCode::BAD_REQUEST, "invalid name"));
    }
    let mut transaction = state.pool.begin().await?;
    let target: Option<i64> = sqlx::query_scalar("SELECT id FROM players WHERE world_id = $1 AND lower(username) = lower($2) AND died_at IS NULL AND banned_at IS NULL")
        .bind(state.world_id).bind(&input.username).fetch_optional(&mut *transaction).await?;
    let target = target.ok_or_else(|| status(StatusCode::NOT_FOUND, "player not found"))?;
    if target == player_id {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "you cannot befriend yourself",
        ));
    }
    let (low, high) = ordered(player_id, target);
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM player_friendships WHERE player_id = $1 AND friend_id = $2)",
    )
    .bind(low)
    .bind(high)
    .fetch_one(&mut *transaction)
    .await?;
    if already {
        return Err(status(StatusCode::CONFLICT, "already friends"));
    }
    let answered: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM friend_requests WHERE from_id = $1 AND to_id = $2)",
    )
    .bind(target)
    .bind(player_id)
    .fetch_one(&mut *transaction)
    .await?;
    if answered {
        befriend(&mut transaction, player_id, target).await?;
        transaction.commit().await?;
        return Ok((StatusCode::OK, Json(Outcome { status: "friends" })));
    }
    let outgoing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM friend_requests WHERE from_id = $1")
            .bind(player_id)
            .fetch_one(&mut *transaction)
            .await?;
    if outgoing >= MAX_OUTGOING {
        return Err(status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many pending requests",
        ));
    }
    sqlx::query(
        "INSERT INTO friend_requests (from_id, to_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(player_id)
    .bind(target)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Outcome {
            status: "requested",
        }),
    ))
}

#[derive(Serialize, FromRow)]
struct Pending {
    id: i64,
    name: String,
}

#[derive(Serialize)]
struct Requests {
    incoming: Vec<Pending>,
    outgoing: Vec<Pending>,
}

async fn list_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Requests>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let incoming = sqlx::query_as("SELECT friend_requests.id, players.username AS name FROM friend_requests JOIN players ON players.id = from_id WHERE to_id = $1 AND players.died_at IS NULL ORDER BY friend_requests.id LIMIT 50")
        .bind(player_id).fetch_all(&state.pool).await?;
    let outgoing = sqlx::query_as("SELECT friend_requests.id, players.username AS name FROM friend_requests JOIN players ON players.id = to_id WHERE from_id = $1 AND players.died_at IS NULL ORDER BY friend_requests.id LIMIT 50")
        .bind(player_id).fetch_all(&state.pool).await?;
    Ok(Json(Requests { incoming, outgoing }))
}

async fn accept_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Outcome>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let from: Option<i64> = sqlx::query_scalar(
        "SELECT from_id FROM friend_requests WHERE id = $1 AND to_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(player_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let from = from.ok_or_else(|| status(StatusCode::NOT_FOUND, "request not found"))?;
    befriend(&mut transaction, from, player_id).await?;
    transaction.commit().await?;
    Ok(Json(Outcome { status: "friends" }))
}

async fn drop_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let removed =
        sqlx::query("DELETE FROM friend_requests WHERE id = $1 AND (to_id = $2 OR from_id = $2)")
            .bind(id)
            .bind(player_id)
            .execute(&state.pool)
            .await?
            .rows_affected();
    if removed == 0 {
        return Err(status(StatusCode::NOT_FOUND, "request not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_friend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<StatusCode, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let removed = sqlx::query("DELETE FROM player_friendships USING players WHERE players.id = CASE WHEN player_friendships.player_id = $1 THEN player_friendships.friend_id ELSE player_friendships.player_id END AND (player_friendships.player_id = $1 OR player_friendships.friend_id = $1) AND lower(players.username) = lower($2) AND players.world_id = $3")
        .bind(player_id).bind(&name).bind(state.world_id)
        .execute(&state.pool).await?.rows_affected();
    if removed == 0 {
        return Err(status(StatusCode::NOT_FOUND, "friend not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct After {
    #[serde(default)]
    after: i64,
}

#[derive(Serialize, FromRow)]
struct Event {
    id: i64,
    kind: String,
    subject: String,
    level: Option<i32>,
}

#[derive(Serialize)]
struct Events {
    events: Vec<Event>,
    /// Pass this as `after` next time; events up to it are then forgotten.
    last: i64,
}

/// Delivers the events of friends once. The poll also tells the server that the player is
/// online; events up to `after` have arrived and are deleted, old ones are dropped.
async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<After>,
) -> Result<Json<Events>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if query.after < 0 {
        return Err(status(StatusCode::BAD_REQUEST, "invalid position"));
    }
    let mut transaction = state.pool.begin().await?;
    sqlx::query("UPDATE players SET last_seen_at = now() WHERE id = $1")
        .bind(player_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM player_events WHERE recipient_id = $1 AND id <= $2")
        .bind(player_id)
        .bind(query.after)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(&format!("DELETE FROM player_events WHERE created_at < now() - make_interval(secs => {EVENT_LIFETIME_SECONDS})"))
        .execute(&mut *transaction)
        .await?;
    let events: Vec<Event> = sqlx::query_as(&format!("SELECT id, kind, subject, level FROM player_events WHERE recipient_id = $1 AND id > $2 ORDER BY id LIMIT {MAX_EVENTS}"))
        .bind(player_id).bind(query.after).fetch_all(&mut *transaction).await?;
    transaction.commit().await?;
    let last = events.last().map_or(query.after, |event| event.id);
    Ok(Json(Events { events, last }))
}
