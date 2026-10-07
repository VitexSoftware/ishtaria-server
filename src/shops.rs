//! Merchants: NPCs with a `shop:<id>` tag buy and sell for gold.
//!
//! What a shop sells and buys, and at which price, is data in `etc/shops.json`. The server
//! checks that the character stands near the merchant, that they own what they sell or can pay
//! for what they buy, and does the exchange in one transaction. A merchant never runs out of
//! goods, but pays at most `daily_sell_limit_gold` a day to one character, so selling cannot
//! mint unlimited gold. Selling pays less than buying costs, so there is no profit in a loop.

use super::gathering::add_items;
use super::players::{self, Error};
use super::story::world::{self, NpcDescriptor, TALK_RANGE_M};
use super::AppState;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

/// The most one request may trade; the daily limit and the inventory bound the rest.
const MAX_QUANTITY: i64 = 1000;

#[derive(Deserialize)]
struct Price {
    item: String,
    price: i64,
}

#[derive(Deserialize)]
struct Shop {
    sells: Vec<Price>,
    buys: Vec<Price>,
}

#[derive(Deserialize)]
struct Shops {
    version: u32,
    daily_sell_limit_gold: i64,
    shops: HashMap<String, Shop>,
}

fn shops() -> &'static Shops {
    static CONFIG: OnceLock<Shops> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Shops = serde_json::from_str(include_str!("../etc/shops.json"))
            .expect("bundled shops must be valid");
        assert_eq!(config.version, 1);
        config
    })
}

/// Items the configuration refers to, for checking them against the item catalog.
#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    shops()
        .shops
        .values()
        .flat_map(|shop| shop.sells.iter().chain(&shop.buys))
        .map(|price| price.item.clone())
        .collect()
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/shops/{npc_id}", get(goods))
        .route("/shops/{npc_id}/buy", post(buy))
        .route("/shops/{npc_id}/sell", post(sell))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize)]
struct Good {
    item_id: String,
    name: String,
    price: String,
}

#[derive(Serialize)]
struct Goods {
    npc_id: String,
    /// What the merchant sells (the character pays) and buys (the character is paid).
    sells: Vec<Good>,
    buys: Vec<Good>,
    /// The most gold this merchant pays one character a day, and how much is already used.
    daily_limit: String,
    sold_today: String,
}

/// The merchant `npc_id` and the shop it keeps.
async fn merchant(
    state: &AppState,
    npc_id: &str,
) -> Result<(NpcDescriptor, &'static str, &'static Shop), Error> {
    let story = world::world(state)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "this world has no story"))?;
    let npc = story
        .npcs
        .iter()
        .find(|npc| npc.id == npc_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown merchant"))?;
    let (id, shop) = npc
        .shop
        .as_deref()
        .and_then(|id| shops().shops.get_key_value(id))
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown merchant"))?;
    Ok((npc.clone(), id.as_str(), shop))
}

fn near(position: (f64, f64, f64), npc: &NpcDescriptor) -> bool {
    world::distance([position.0, position.1, position.2], npc) <= TALK_RANGE_M
}

async fn sold_today(
    executor: impl sqlx::PgExecutor<'_>,
    player_id: i64,
    shop_id: &str,
) -> Result<i64, Error> {
    let gold: Option<i64> = sqlx::query_scalar("SELECT gold FROM shop_sales WHERE player_id = $1 AND shop_id = $2 AND window_start > now() - interval '24 hours'")
        .bind(player_id).bind(shop_id).fetch_optional(executor).await?;
    Ok(gold.unwrap_or(0))
}

async fn goods(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(npc_id): Path<String>,
) -> Result<Json<Goods>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let (npc, shop_id, shop) = merchant(&state, &npc_id).await?;
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?;
    if !position.is_some_and(|position| near(position, &npc)) {
        return Err(status(StatusCode::FORBIDDEN, "merchant is out of reach"));
    }
    let names: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT id, name FROM item_types")
            .fetch_all(&state.pool)
            .await?
            .into_iter()
            .collect();
    let describe = |prices: &[Price]| {
        prices
            .iter()
            .map(|price| Good {
                item_id: price.item.clone(),
                name: names.get(&price.item).cloned().unwrap_or_default(),
                price: price.price.to_string(),
            })
            .collect()
    };
    Ok(Json(Goods {
        npc_id,
        sells: describe(&shop.sells),
        buys: describe(&shop.buys),
        daily_limit: shops().daily_sell_limit_gold.to_string(),
        sold_today: sold_today(&state.pool, player_id, shop_id)
            .await?
            .to_string(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Deal {
    item_id: String,
    quantity: i64,
}

/// Locks the character, checks they live and stand near the merchant.
async fn begin(
    state: &AppState,
    player_id: i64,
    npc: &NpcDescriptor,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, Error> {
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let position: Option<(f64, f64, f64)> = sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    if !position.is_some_and(|position| near(position, npc)) {
        return Err(status(StatusCode::FORBIDDEN, "merchant is out of reach"));
    }
    Ok(transaction)
}

/// Takes `quantity` of an item from the character's inventory, or fails without changing anything.
/// A stack cannot hold zero, so taking all of it removes the row.
pub(super) async fn take(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    item: &str,
    quantity: i64,
) -> Result<(), Error> {
    let owned: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2 FOR UPDATE",
    )
    .bind(player_id)
    .bind(item)
    .fetch_optional(&mut **transaction)
    .await?;
    match owned {
        Some(owned) if owned > quantity => {
            sqlx::query("UPDATE player_inventory SET quantity = quantity - $3 WHERE player_id = $1 AND item_id = $2")
                .bind(player_id).bind(item).bind(quantity).execute(&mut **transaction).await?;
        }
        Some(owned) if owned == quantity => {
            sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
                .bind(player_id)
                .bind(item)
                .execute(&mut **transaction)
                .await?;
        }
        _ => return Err(status(StatusCode::CONFLICT, "not enough to pay")),
    }
    Ok(())
}

fn quantity(deal: &Deal) -> Result<i64, Error> {
    if (1..=MAX_QUANTITY).contains(&deal.quantity) {
        Ok(deal.quantity)
    } else {
        Err(status(StatusCode::BAD_REQUEST, "invalid quantity"))
    }
}

async fn buy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(npc_id): Path<String>,
    Json(deal): Json<Deal>,
) -> Result<Json<players::Player>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let quantity = quantity(&deal)?;
    let (npc, _, shop) = merchant(&state, &npc_id).await?;
    let price = shop
        .sells
        .iter()
        .find(|price| price.item == deal.item_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "the merchant does not sell this"))?;
    let cost = price
        .price
        .checked_mul(quantity)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid quantity"))?;
    let mut transaction = begin(&state, player_id, &npc).await?;
    take(&mut transaction, player_id, "gold", cost).await?;
    add_items(
        &mut transaction,
        player_id,
        &[(price.item.clone(), quantity)],
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

async fn sell(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(npc_id): Path<String>,
    Json(deal): Json<Deal>,
) -> Result<Json<players::Player>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let quantity = quantity(&deal)?;
    let (npc, shop_id, shop) = merchant(&state, &npc_id).await?;
    let price = shop
        .buys
        .iter()
        .find(|price| price.item == deal.item_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "the merchant does not buy this"))?;
    let earned = price
        .price
        .checked_mul(quantity)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid quantity"))?;
    let mut transaction = begin(&state, player_id, &npc).await?;
    // The daily limit is checked and counted in one statement, under the character's row lock.
    let limit = shops().daily_sell_limit_gold;
    let total: i64 = sqlx::query_scalar("INSERT INTO shop_sales (player_id, shop_id, gold) VALUES ($1, $2, $3) ON CONFLICT (player_id, shop_id) DO UPDATE SET window_start = CASE WHEN shop_sales.window_start > now() - interval '24 hours' THEN shop_sales.window_start ELSE now() END, gold = CASE WHEN shop_sales.window_start > now() - interval '24 hours' THEN shop_sales.gold + EXCLUDED.gold ELSE EXCLUDED.gold END RETURNING gold")
        .bind(player_id).bind(shop_id).bind(earned).fetch_one(&mut *transaction).await?;
    if total > limit {
        return Err(status(
            StatusCode::CONFLICT,
            "the merchant pays no more today",
        ));
    }
    take(&mut transaction, player_id, &deal.item_id, quantity).await?;
    add_items(&mut transaction, player_id, &[("gold".to_owned(), earned)]).await?;
    transaction.commit().await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selling_never_pays_what_buying_costs() {
        let config = shops();
        assert!(config.daily_sell_limit_gold > 0);
        for shop in config.shops.values() {
            for bought in &shop.buys {
                assert!(bought.price > 0, "{}", bought.item);
                if let Some(sold) = shop.sells.iter().find(|sold| sold.item == bought.item) {
                    assert!(
                        bought.price < sold.price,
                        "{} is a money machine",
                        bought.item
                    );
                }
            }
            for sold in &shop.sells {
                assert!(sold.price > 0, "{}", sold.item);
            }
        }
    }
}
