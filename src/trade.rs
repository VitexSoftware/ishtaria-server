//! Trading between players.
//!
//! A player opens an exchange with another who stands close by. Each side puts items (and gold)
//! on the table, the offer of either side can be changed until both accept, and changing an
//! offer withdraws both acceptances. When both have accepted, the server checks everything
//! again and swaps the items in one transaction: all or nothing. An exchange is delivery state,
//! not history: it is deleted when it ends, and an exchange nobody finishes expires.
//!
//! A piece that has been used (stored with its remaining durability) cannot be offered, so
//! trading can never turn a worn tool into a new one.

use super::gathering::add_items;
use super::players::{self, Error};
use super::shops::take;
use super::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};

/// Both traders must be this close (metres).
const TRADE_RANGE_M: f64 = 10.0;
/// An exchange that is not finished within this time is dropped.
const EXPIRES_MINUTES: i32 = 5;
const MAX_STACKS: usize = 10;
const MAX_QUANTITY: i64 = 1_000_000_000;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/trades", post(open))
        .route("/trades/current", get(current).delete(cancel))
        .route("/trades/current/offer", put(offer))
        .route("/trades/current/accept", post(accept))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize, FromRow, Clone)]
struct Stack {
    item_id: String,
    name: String,
    quantity: String,
}

#[derive(Serialize)]
struct View {
    /// The other trader.
    with: String,
    mine: Vec<Stack>,
    theirs: Vec<Stack>,
    i_accepted: bool,
    they_accepted: bool,
}

#[derive(FromRow)]
struct Row {
    id: i64,
    initiator_id: i64,
    partner_id: i64,
    initiator_accepted: bool,
    partner_accepted: bool,
}

/// Locks two characters in a fixed order (so two exchanges never wait for each other) and
/// returns whether both live.
async fn lock_pair(
    transaction: &mut Transaction<'_, Postgres>,
    world_id: i64,
    first: i64,
    second: i64,
) -> Result<bool, Error> {
    let mut alive = true;
    for id in [first.min(second), first.max(second)] {
        alive &= super::survival::lock_alive(transaction, world_id, id).await?;
    }
    Ok(alive)
}

async fn close_enough(
    transaction: &mut Transaction<'_, Postgres>,
    first: i64,
    second: i64,
) -> Result<bool, Error> {
    let close: Option<bool> = sqlx::query_scalar("SELECT (a.position_x - b.position_x)^2 + (a.position_y - b.position_y)^2 + (a.position_z - b.position_z)^2 <= $3 FROM players a, players b WHERE a.id = $1 AND b.id = $2 AND a.position_x IS NOT NULL AND b.position_x IS NOT NULL")
        .bind(first).bind(second).bind(TRADE_RANGE_M * TRADE_RANGE_M)
        .fetch_optional(&mut **transaction).await?;
    Ok(close.unwrap_or(false))
}

/// The exchange of the character, dropping those that expired.
async fn find(
    transaction: &mut Transaction<'_, Postgres>,
    player_id: i64,
) -> Result<Option<Row>, Error> {
    sqlx::query(&format!(
        "DELETE FROM trades WHERE created_at < now() - make_interval(mins => {EXPIRES_MINUTES})"
    ))
    .execute(&mut **transaction)
    .await?;
    Ok(sqlx::query_as("SELECT id, initiator_id, partner_id, initiator_accepted, partner_accepted FROM trades WHERE initiator_id = $1 OR partner_id = $1 FOR UPDATE")
        .bind(player_id).fetch_optional(&mut **transaction).await?)
}

async fn offered(
    transaction: &mut Transaction<'_, Postgres>,
    trade_id: i64,
    player_id: i64,
) -> Result<Vec<Stack>, Error> {
    Ok(sqlx::query_as("SELECT item_id, name, quantity::text FROM trade_offers JOIN item_types ON item_types.id = item_id WHERE trade_id = $1 AND player_id = $2 ORDER BY item_id")
        .bind(trade_id).bind(player_id).fetch_all(&mut **transaction).await?)
}

async fn view(
    transaction: &mut Transaction<'_, Postgres>,
    trade: &Row,
    player_id: i64,
) -> Result<View, Error> {
    let other = if trade.initiator_id == player_id {
        trade.partner_id
    } else {
        trade.initiator_id
    };
    let with: String = sqlx::query_scalar("SELECT username FROM players WHERE id = $1")
        .bind(other)
        .fetch_one(&mut **transaction)
        .await?;
    let initiator = trade.initiator_id == player_id;
    Ok(View {
        with,
        mine: offered(transaction, trade.id, player_id).await?,
        theirs: offered(transaction, trade.id, other).await?,
        i_accepted: if initiator {
            trade.initiator_accepted
        } else {
            trade.partner_accepted
        },
        they_accepted: if initiator {
            trade.partner_accepted
        } else {
            trade.initiator_accepted
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    with: String,
}

/// Asks a player nearby to trade.
async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Open>,
) -> Result<(StatusCode, Json<View>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if input.with.is_empty() || input.with.chars().count() > 32 {
        return Err(status(StatusCode::BAD_REQUEST, "invalid name"));
    }
    let mut transaction = state.pool.begin().await?;
    let partner: Option<i64> = sqlx::query_scalar("SELECT id FROM players WHERE world_id = $1 AND lower(username) = lower($2) AND died_at IS NULL AND banned_at IS NULL")
        .bind(state.world_id).bind(&input.with).fetch_optional(&mut *transaction).await?;
    let partner = partner.ok_or_else(|| status(StatusCode::NOT_FOUND, "player not found"))?;
    if partner == player_id {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "you cannot trade with yourself",
        ));
    }
    if !lock_pair(&mut transaction, state.world_id, player_id, partner).await? {
        return Err(status(StatusCode::NOT_FOUND, "player not found"));
    }
    if !close_enough(&mut transaction, player_id, partner).await? {
        return Err(status(StatusCode::FORBIDDEN, "the player is too far away"));
    }
    // Looking up either side removes expired exchanges; neither may be in another one.
    if find(&mut transaction, player_id).await?.is_some()
        || find(&mut transaction, partner).await?.is_some()
    {
        return Err(status(StatusCode::CONFLICT, "already trading"));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO trades (initiator_id, partner_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(player_id)
    .bind(partner)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO player_events (recipient_id, kind, subject) VALUES ($2, 'trade', (SELECT username FROM players WHERE id = $1))")
        .bind(player_id).bind(partner).execute(&mut *transaction).await?;
    let trade = Row {
        id,
        initiator_id: player_id,
        partner_id: partner,
        initiator_accepted: false,
        partner_accepted: false,
    };
    let reply = view(&mut transaction, &trade, player_id).await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(reply)))
}

async fn current(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<View>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    let trade = find(&mut transaction, player_id)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no exchange"))?;
    let reply = view(&mut transaction, &trade, player_id).await?;
    transaction.commit().await?;
    Ok(Json(reply))
}

async fn cancel(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    sqlx::query("DELETE FROM trades WHERE initiator_id = $1 OR partner_id = $1")
        .bind(player_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What one trader gives: from whom, to whom, which stacks.
type Gift = (i64, i64, Vec<(String, i64)>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    item_id: String,
    quantity: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    items: Vec<Item>,
}

/// Checks that the character owns each stack of the offer and that no piece of it is worn.
async fn check_owned(
    transaction: &mut Transaction<'_, Postgres>,
    player_id: i64,
    items: &[(String, i64)],
) -> Result<(), Error> {
    for (item, quantity) in items {
        let row: Option<(i64, Option<i32>)> = sqlx::query_as("SELECT quantity, durability FROM player_inventory WHERE player_id = $1 AND item_id = $2 FOR UPDATE")
            .bind(player_id).bind(item).fetch_optional(&mut **transaction).await?;
        match row {
            Some((owned, _)) if owned < *quantity => {
                return Err(status(StatusCode::CONFLICT, "not enough to offer"))
            }
            Some((_, Some(_))) => {
                return Err(status(StatusCode::CONFLICT, "a worn item cannot be traded"))
            }
            None => return Err(status(StatusCode::CONFLICT, "not enough to offer")),
            Some(_) => {}
        }
    }
    Ok(())
}

/// Replaces what the character puts on the table; both acceptances are withdrawn.
async fn offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Offer>,
) -> Result<Json<View>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut items: Vec<(String, i64)> = Vec::new();
    for item in &input.items {
        if !(1..=MAX_QUANTITY).contains(&item.quantity)
            || item.item_id.len() > 64
            || items.iter().any(|(id, _)| id == &item.item_id)
        {
            return Err(status(StatusCode::BAD_REQUEST, "invalid offer"));
        }
        items.push((item.item_id.clone(), item.quantity));
    }
    if items.len() > MAX_STACKS {
        return Err(status(StatusCode::BAD_REQUEST, "invalid offer"));
    }
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let mut trade = find(&mut transaction, player_id)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no exchange"))?;
    check_owned(&mut transaction, player_id, &items).await?;
    sqlx::query("DELETE FROM trade_offers WHERE trade_id = $1 AND player_id = $2")
        .bind(trade.id)
        .bind(player_id)
        .execute(&mut *transaction)
        .await?;
    for (item, quantity) in &items {
        sqlx::query(
            "INSERT INTO trade_offers (trade_id, player_id, item_id, quantity) VALUES ($1, $2, $3, $4)",
        )
        .bind(trade.id)
        .bind(player_id)
        .bind(item)
        .bind(quantity)
        .execute(&mut *transaction)
        .await
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid offer"))?;
    }
    sqlx::query(
        "UPDATE trades SET initiator_accepted = false, partner_accepted = false WHERE id = $1",
    )
    .bind(trade.id)
    .execute(&mut *transaction)
    .await?;
    trade.initiator_accepted = false;
    trade.partner_accepted = false;
    let reply = view(&mut transaction, &trade, player_id).await?;
    transaction.commit().await?;
    Ok(Json(reply))
}

#[derive(Serialize)]
struct Accepted {
    /// True when both accepted and the items changed hands.
    done: bool,
    trade: Option<View>,
    player: players::Player,
}

/// Swaps what is on the table, once both have accepted. Everything is checked again first.
async fn swap(
    transaction: &mut Transaction<'_, Postgres>,
    world_id: i64,
    trade: &Row,
) -> Result<(), Error> {
    if !lock_pair(transaction, world_id, trade.initiator_id, trade.partner_id).await? {
        return Err(status(StatusCode::CONFLICT, "a trader is gone"));
    }
    if !close_enough(transaction, trade.initiator_id, trade.partner_id).await? {
        return Err(status(StatusCode::FORBIDDEN, "the player is too far away"));
    }
    let mut gives: Vec<Gift> = Vec::new();
    for (giver, taker) in [
        (trade.initiator_id, trade.partner_id),
        (trade.partner_id, trade.initiator_id),
    ] {
        let items: Vec<(String, i64)> = sqlx::query_as(
            "SELECT item_id, quantity FROM trade_offers WHERE trade_id = $1 AND player_id = $2",
        )
        .bind(trade.id)
        .bind(giver)
        .fetch_all(&mut **transaction)
        .await?;
        check_owned(transaction, giver, &items).await?;
        gives.push((giver, taker, items));
    }
    for (giver, _, items) in &gives {
        for (item, quantity) in items {
            take(transaction, *giver, item, *quantity).await?;
        }
    }
    for (_, taker, items) in &gives {
        add_items(transaction, *taker, items).await?;
    }
    sqlx::query("DELETE FROM trades WHERE id = $1")
        .bind(trade.id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn accept(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Accepted>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    // Both traders are locked in the order of their ids before anything else, so two
    // players accepting at the same moment cannot wait for each other.
    let pair: Option<(i64, i64)> = sqlx::query_as(
        "SELECT initiator_id, partner_id FROM trades WHERE initiator_id = $1 OR partner_id = $1",
    )
    .bind(player_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some((first, second)) = pair {
        lock_pair(&mut transaction, state.world_id, first, second).await?;
    }
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let mut trade = find(&mut transaction, player_id)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no exchange"))?;
    let initiator = trade.initiator_id == player_id;
    if initiator {
        trade.initiator_accepted = true;
    } else {
        trade.partner_accepted = true;
    }
    sqlx::query("UPDATE trades SET initiator_accepted = $2, partner_accepted = $3 WHERE id = $1")
        .bind(trade.id)
        .bind(trade.initiator_accepted)
        .bind(trade.partner_accepted)
        .execute(&mut *transaction)
        .await?;
    if !(trade.initiator_accepted && trade.partner_accepted) {
        let reply = view(&mut transaction, &trade, player_id).await?;
        transaction.commit().await?;
        return Ok(Json(Accepted {
            done: false,
            trade: Some(reply),
            player: players::profile(&state, player_id).await?,
        }));
    }
    let trade_id = trade.id;
    match swap(&mut transaction, state.world_id, &trade).await {
        Ok(()) => {
            transaction.commit().await?;
            Ok(Json(Accepted {
                done: true,
                trade: None,
                player: players::profile(&state, player_id).await?,
            }))
        }
        Err(error) => {
            // Nothing changed hands; both must look at the table again before accepting.
            drop(transaction);
            sqlx::query("UPDATE trades SET initiator_accepted = false, partner_accepted = false WHERE id = $1")
                .bind(trade_id).execute(&state.pool).await?;
            Err(error)
        }
    }
}
