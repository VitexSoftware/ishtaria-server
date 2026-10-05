//! Gathering resources from generated trees and rocks, and crafting.
//!
//! Resources are described in `etc/resources.json` and recipes in
//! `etc/recipes.json`. The server regenerates the object a player names, checks
//! reach, stamina and rate, and decides the outcome; clients only send
//! intentions. A harvested object is stored as a change of the generated world
//! (`world_object_state`) and grows back after a while.
//!
//! Tools (axe for felling, pickaxe for mining, sword for defence) are ordinary
//! inventory items; every new character starts with these three. A felled tree
//! leaves a stump and a log that fills ten inventory slots until it is chopped.

use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Drop {
    item: String,
    min: i64,
    max: i64,
}

#[derive(Deserialize)]
struct Bonus {
    item: String,
    chance: f64,
    min: i64,
    max: i64,
}

#[derive(Deserialize, Clone)]
struct Leaves {
    model: String,
    scale: f64,
}

#[derive(Deserialize)]
struct Resource {
    id: String,
    kind: String,
    /// Tools that work on it and the work points of one swing with each. The pickaxe
    /// fells a tree too, but with half the effect of the axe.
    tools: std::collections::HashMap<String, i32>,
    hits: i32,
    regrow_minutes: i64,
    models: Vec<String>,
    drops: Vec<Drop>,
    #[serde(default)]
    bonus: Vec<Bonus>,
    /// Remains shown in place of the object while it grows back.
    #[serde(default)]
    leaves: Option<Leaves>,
}

#[derive(Deserialize)]
struct Resources {
    version: u32,
    reach_m: f64,
    cooldown_ms: i64,
    stamina_cost: i32,
    forget_hits_after_minutes: i64,
    /// Work points of one swing of the best tool; a resource needs `hits` such swings.
    best_swing_points: i32,
    resources: Vec<Resource>,
}

#[derive(Deserialize, Serialize, Clone)]
struct Stack {
    item: String,
    quantity: i64,
}

#[derive(Deserialize)]
struct Recipe {
    id: String,
    /// Basic tool the character needs (for example the axe to chop logs).
    #[serde(default)]
    tool: Option<String>,
    /// Experience for each item crafted.
    #[serde(default)]
    xp: i64,
    inputs: Vec<Stack>,
    outputs: Vec<Stack>,
}

#[derive(Deserialize)]
struct Recipes {
    version: u32,
    recipes: Vec<Recipe>,
}

fn resources() -> &'static Resources {
    static CONFIG: OnceLock<Resources> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Resources = serde_json::from_str(include_str!("../etc/resources.json"))
            .expect("bundled resources must be valid");
        assert_eq!(config.version, 1);
        config
    })
}

fn recipes() -> &'static Recipes {
    static CONFIG: OnceLock<Recipes> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Recipes = serde_json::from_str(include_str!("../etc/recipes.json"))
            .expect("bundled recipes must be valid");
        assert_eq!(config.version, 1);
        config
    })
}

/// What a client may know about a harvestable object.
#[derive(Serialize)]
pub(super) struct HarvestInfo {
    kind: &'static str,
    /// The best tool, and every tool that works.
    tool: String,
    tools: Vec<String>,
    hits: i32,
}

/// Harvest details for a model, or nothing when it is only scenery.
pub(super) fn harvest_info(model: &str) -> Option<HarvestInfo> {
    resource_of(model).map(|resource| {
        let mut tools: Vec<(&String, &i32)> = resource.tools.iter().collect();
        tools.sort_by(|first, second| second.1.cmp(first.1).then(first.0.cmp(second.0)));
        HarvestInfo {
            kind: &resource.kind,
            tool: tools[0].0.clone(),
            tools: tools.into_iter().map(|(tool, _)| tool.clone()).collect(),
            hits: resource.hits,
        }
    })
}

/// What replaces a harvested object of this model while it grows back.
pub(super) fn leaves_for(model: &str) -> Option<(String, f64)> {
    resource_of(model)?
        .leaves
        .as_ref()
        .map(|leaves| (leaves.model.clone(), leaves.scale))
}

/// `*` matches any run of characters (at most one per pattern).
fn matches(pattern: &str, model: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == model,
        Some((prefix, suffix)) => {
            model.len() >= prefix.len() + suffix.len()
                && model.starts_with(prefix)
                && model.ends_with(suffix)
        }
    }
}

fn resource_of(model: &str) -> Option<&'static Resource> {
    resources().resources.iter().find(|resource| {
        resource
            .models
            .iter()
            .any(|pattern| matches(pattern, model))
    })
}

#[cfg(test)]
pub(super) struct ResourceInfo {
    pub hits: i32,
    pub log: String,
}

#[cfg(test)]
pub(super) fn resource_kind(model: &str) -> Option<&'static str> {
    resource_of(model).map(|resource| resource.kind.as_str())
}

#[cfg(test)]
pub(super) fn resource_info(model: &str) -> Option<ResourceInfo> {
    resource_of(model).map(|resource| ResourceInfo {
        hits: resource.hits,
        log: resource.drops[0].item.clone(),
    })
}

/// Every item named by resources and recipes.
#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for resource in &resources().resources {
        items.extend(resource.tools.keys().cloned());
        items.extend(resource.drops.iter().map(|drop| drop.item.clone()));
        items.extend(resource.bonus.iter().map(|bonus| bonus.item.clone()));
    }
    for recipe in &recipes().recipes {
        items.extend(
            recipe
                .inputs
                .iter()
                .chain(&recipe.outputs)
                .map(|stack| stack.item.clone()),
        );
    }
    items
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/harvest", post(harvest))
        .route("/players/me/craft", post(craft))
        .route("/players/me/equip", post(equip).delete(unequip))
        .route("/players/me/block", post(block))
        .route("/recipes", get(list_recipes))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Harvest {
    object_id: String,
}

#[derive(Serialize)]
pub(super) struct Gained {
    item_id: String,
    name: String,
    quantity: String,
}

#[derive(Serialize)]
struct HarvestReply {
    object_id: String,
    resource: &'static str,
    kind: &'static str,
    /// The tool in hand that was used.
    tool: String,
    /// `hit` while the object still stands, `depleted` once harvested.
    state: &'static str,
    hits: i32,
    hits_required: i32,
    /// Experience gained by this swing.
    xp: i64,
    /// Wear of the tool after this swing; `broken` when it was destroyed.
    wear: Option<super::durability::Wear>,
    items: Vec<Gained>,
    player: players::Player,
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

/// Adds items to the inventory, honouring stack limits and free slots.
pub(super) async fn add_items(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    items: &[(String, i64)],
) -> Result<Vec<Gained>, Error> {
    let (capacity, mut used) = super::survival::slots(transaction, player_id).await?;
    let mut gained = Vec::new();
    for (item, quantity) in items {
        let (name, limit, multi_slot, cost): (String, i64, bool, i64) = sqlx::query_as(
            "SELECT name, stack_limit, multi_slot, slot_cost::bigint FROM item_types WHERE id = $1",
        )
        .bind(item)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| status(StatusCode::INTERNAL_SERVER_ERROR, "unknown item"))?;
        let owned: Option<i64> = sqlx::query_scalar(
            "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2 FOR UPDATE",
        )
        .bind(player_id)
        .bind(item)
        .fetch_optional(&mut **transaction)
        .await?;
        let total = owned
            .unwrap_or(0)
            .checked_add(*quantity)
            .ok_or_else(|| status(StatusCode::CONFLICT, "stack overflow"))?;
        let slots_for = |count: i64| (count / limit + i64::from(count % limit != 0)) * cost;
        let extra = if multi_slot {
            slots_for(total) - slots_for(owned.unwrap_or(0))
        } else if total > limit {
            return Err(status(StatusCode::CONFLICT, "stack is full"));
        } else if owned.is_none() {
            cost
        } else {
            0
        };
        if used + extra > capacity {
            return Err(status(StatusCode::CONFLICT, "inventory is full"));
        }
        used += extra;
        sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, $2, $3) ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = EXCLUDED.quantity")
            .bind(player_id)
            .bind(item)
            .bind(total)
            .execute(&mut **transaction)
            .await?;
        gained.push(Gained {
            item_id: item.clone(),
            name,
            quantity: quantity.to_string(),
        });
    }
    Ok(gained)
}

fn roll(resource: &Resource) -> Vec<(String, i64)> {
    let mut rng = OsRng;
    let mut items: Vec<(String, i64)> = Vec::new();
    for drop in &resource.drops {
        items.push((drop.item.clone(), rng.gen_range(drop.min..=drop.max)));
    }
    for bonus in &resource.bonus {
        if rng.gen_bool(bonus.chance) {
            items.push((bonus.item.clone(), rng.gen_range(bonus.min..=bonus.max)));
        }
    }
    items
}

async fn harvest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Harvest>,
) -> Result<Json<HarvestReply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if input.object_id.len() > 100 || !input.object_id.is_ascii() {
        return Err(status(StatusCode::BAD_REQUEST, "invalid object"));
    }
    let config = resources();
    let terrain = super::movement::terrain(&state).await?;
    let object = terrain
        .object_by_id(&input.object_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "object not found"))?;
    let resource = resource_of(&object.model)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "object cannot be harvested"))?;

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
    let distance = ((x - object.position[0]).powi(2)
        + (y - object.position[1]).powi(2)
        + (z - object.position[2]).powi(2))
    .sqrt();
    if distance > config.reach_m + object.collision_radius_m {
        return Err(status(StatusCode::FORBIDDEN, "object is out of reach"));
    }
    if since.is_some_and(|seconds| seconds * 1000.0 < config.cooldown_ms as f64) {
        return Err(status(StatusCode::TOO_MANY_REQUESTS, "too fast"));
    }
    if stamina < config.stamina_cost {
        return Err(status(StatusCode::CONFLICT, "too exhausted"));
    }
    // Only the tool in hand counts, and only while the character still owns it.
    let equipped: Option<String> = sqlx::query_scalar("SELECT hand FROM player_equipment WHERE player_id = $1 AND EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = hand)")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let tool = equipped.ok_or_else(|| status(StatusCode::CONFLICT, "required tool missing"))?;
    let points = *resource
        .tools
        .get(&tool)
        .ok_or_else(|| status(StatusCode::CONFLICT, "required tool missing"))?;
    sqlx::query("INSERT INTO world_object_state (world_id, object_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(state.world_id).bind(&input.object_id).execute(&mut *transaction).await?;
    let (hits, depleted, stale): (i32, bool, bool) = sqlx::query_as("SELECT hits, coalesce(depleted_until > now(), false), last_hit_at < now() - make_interval(mins => $3) FROM world_object_state WHERE world_id = $1 AND object_id = $2 FOR UPDATE")
        .bind(state.world_id).bind(&input.object_id).bind(config.forget_hits_after_minutes as i32)
        .fetch_one(&mut *transaction).await?;
    if depleted {
        return Err(status(StatusCode::CONFLICT, "already harvested"));
    }
    // Hits are stored as work points: a swing of the axe on a tree is worth two, of the pickaxe one.
    let progress = if stale { points } else { hits + points };
    let needed = resource.hits * config.best_swing_points;
    sqlx::query("UPDATE players SET stamina = stamina - $2, last_gathered_at = clock_timestamp() WHERE id = $1")
        .bind(player_id).bind(config.stamina_cost).execute(&mut *transaction).await?;
    // The tool wears out with every swing.
    let wear = super::durability::wear(&mut transaction, player_id, &tool, 1).await?;
    // Every swing is worth one point of experience.
    super::experience::grant(&mut transaction, player_id, 1).await?;
    let mut items = Vec::new();
    let mut regrow_at = None;
    if progress >= needed {
        items = add_items(&mut transaction, player_id, &roll(resource)).await?;
        let until: i64 = sqlx::query_scalar("UPDATE world_object_state SET hits = 0, last_hit_at = now(), depleted_until = now() + make_interval(mins => $3) WHERE world_id = $1 AND object_id = $2 RETURNING extract(epoch FROM depleted_until)::bigint")
            .bind(state.world_id).bind(&input.object_id).bind(resource.regrow_minutes as i32)
            .fetch_one(&mut *transaction).await?;
        regrow_at = Some(until);
    } else {
        sqlx::query("UPDATE world_object_state SET hits = $3, last_hit_at = now() WHERE world_id = $1 AND object_id = $2")
            .bind(state.world_id).bind(&input.object_id).bind(progress)
            .execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    if let Some(until) = regrow_at {
        terrain.mark_removed(&input.object_id, until, leaves_for(&object.model));
    }
    Ok(Json(HarvestReply {
        object_id: input.object_id,
        resource: &resource.id,
        kind: &resource.kind,
        tool,
        state: if regrow_at.is_some() {
            "depleted"
        } else {
            "hit"
        },
        // Swings with the tool in hand: more of them are needed with a weaker tool.
        hits: if regrow_at.is_some() {
            0
        } else {
            (progress + points - 1) / points
        },
        hits_required: (needed + points - 1) / points,
        xp: 1,
        wear,
        items,
        player: players::profile(&state, player_id).await?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Equip {
    item_id: String,
}

/// Puts a tool or weapon from the inventory in hand.
async fn equip(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Equip>,
) -> Result<Json<players::Player>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let category: Option<String> = sqlx::query_scalar("SELECT category FROM item_types JOIN player_inventory ON player_inventory.item_id = item_types.id WHERE player_id = $1 AND item_types.id = $2")
        .bind(player_id).bind(&input.item_id).fetch_optional(&mut *transaction).await?;
    match category.as_deref() {
        None => return Err(status(StatusCode::CONFLICT, "item not found")),
        Some("tool" | "weapon") => {
            sqlx::query("INSERT INTO player_equipment (player_id, hand) VALUES ($1, $2) ON CONFLICT (player_id) DO UPDATE SET hand = EXCLUDED.hand")
                .bind(player_id).bind(&input.item_id).execute(&mut *transaction).await?;
        }
        // A shield goes in the other hand.
        Some("shield") => {
            sqlx::query("INSERT INTO player_equipment (player_id, offhand) VALUES ($1, $2) ON CONFLICT (player_id) DO UPDATE SET offhand = EXCLUDED.offhand")
                .bind(player_id).bind(&input.item_id).execute(&mut *transaction).await?;
        }
        Some(_) => return Err(status(StatusCode::CONFLICT, "this item cannot be equipped")),
    }
    transaction.commit().await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Slot {
    /// `hand` (default) or `offhand`.
    slot: Option<String>,
}

/// Takes the item out of the hand, or the shield out of the other hand with `?slot=offhand`.
async fn unequip(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(input): Query<Slot>,
) -> Result<Json<players::Player>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let sql = match input.slot.as_deref() {
        None | Some("hand") => "UPDATE player_equipment SET hand = NULL WHERE player_id = $1",
        Some("offhand") => "UPDATE player_equipment SET offhand = NULL WHERE player_id = $1",
        Some(_) => return Err(status(StatusCode::BAD_REQUEST, "invalid slot")),
    };
    sqlx::query(sql)
        .bind(player_id)
        .execute(&state.pool)
        .await?;
    sqlx::query(
        "DELETE FROM player_equipment WHERE player_id = $1 AND hand IS NULL AND offhand IS NULL",
    )
    .bind(player_id)
    .execute(&state.pool)
    .await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

/// How long one block lasts; the client renews it while the button is held.
const BLOCK_SECONDS: i32 = 2;

#[derive(Serialize)]
struct BlockReply {
    blocking_seconds: i32,
    wear: Option<super::durability::Wear>,
    player: players::Player,
}

/// Raises the shield in the other hand. A block lasts a couple of seconds and wears the
/// shield once each time it is (re)started.
async fn block(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<BlockReply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    let shield: Option<String> = sqlx::query_scalar("SELECT offhand FROM player_equipment WHERE player_id = $1 AND EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = offhand)")
        .bind(player_id).fetch_optional(&mut *transaction).await?.flatten();
    let shield = shield.ok_or_else(|| status(StatusCode::CONFLICT, "no shield in hand"))?;
    // Renewing a block that is still running is free; only a fresh block wears the shield.
    let fresh: Option<i64> = sqlx::query_scalar("UPDATE players SET blocked_until = clock_timestamp() + make_interval(secs => $2) WHERE id = $1 AND (blocked_until IS NULL OR blocked_until < clock_timestamp()) RETURNING id")
        .bind(player_id).bind(BLOCK_SECONDS).fetch_optional(&mut *transaction).await?;
    let wear = if fresh.is_some() {
        super::durability::wear(&mut transaction, player_id, &shield, 1).await?
    } else {
        sqlx::query("UPDATE players SET blocked_until = clock_timestamp() + make_interval(secs => $2) WHERE id = $1")
            .bind(player_id).bind(BLOCK_SECONDS).execute(&mut *transaction).await?;
        None
    };
    if wear.is_some_and(|wear| wear.broken) {
        sqlx::query("UPDATE players SET blocked_until = NULL WHERE id = $1")
            .bind(player_id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(Json(BlockReply {
        blocking_seconds: BLOCK_SECONDS,
        wear,
        player: players::profile(&state, player_id).await?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Craft {
    recipe: String,
    count: String,
}

async fn craft(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Craft>,
) -> Result<Json<players::Player>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let count = input
        .count
        .parse::<i64>()
        .ok()
        .filter(|count| (1..=100).contains(count))
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid count"))?;
    let recipe = recipes()
        .recipes
        .iter()
        .find(|recipe| recipe.id == input.recipe)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "recipe not found"))?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(notice));
    }
    if let Some(tool) = recipe.tool.as_deref() {
        let owns: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = $2)",
        )
        .bind(player_id)
        .bind(tool)
        .fetch_one(&mut *transaction)
        .await?;
        if !owns {
            return Err(status(StatusCode::CONFLICT, "required tool missing"));
        }
    }
    for input in &recipe.inputs {
        let need = input.quantity * count;
        let owned: Option<i64> = sqlx::query_scalar(
            "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2 FOR UPDATE",
        )
        .bind(player_id)
        .bind(&input.item)
        .fetch_optional(&mut *transaction)
        .await?;
        let owned = owned.unwrap_or(0);
        if owned < need {
            return Err(status(StatusCode::CONFLICT, "missing ingredients"));
        }
        if owned == need {
            sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
                .bind(player_id)
                .bind(&input.item)
                .execute(&mut *transaction)
                .await?;
        } else {
            sqlx::query("UPDATE player_inventory SET quantity = quantity - $3 WHERE player_id = $1 AND item_id = $2")
                .bind(player_id).bind(&input.item).bind(need).execute(&mut *transaction).await?;
        }
    }
    let outputs: Vec<(String, i64)> = recipe
        .outputs
        .iter()
        .map(|output| (output.item.clone(), output.quantity * count))
        .collect();
    add_items(&mut transaction, player_id, &outputs).await?;
    super::experience::grant(&mut transaction, player_id, recipe.xp * count).await?;
    transaction.commit().await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

#[derive(Serialize)]
struct RecipeInfo {
    id: String,
    tool: Option<String>,
    inputs: Vec<RecipeItem>,
    outputs: Vec<RecipeItem>,
}

#[derive(Serialize)]
struct RecipeItem {
    item_id: String,
    name: String,
    quantity: String,
}

async fn list_recipes(State(state): State<AppState>) -> Result<Json<Vec<RecipeInfo>>, Error> {
    let names: std::collections::HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT id, name FROM item_types")
            .fetch_all(&state.pool)
            .await?
            .into_iter()
            .collect();
    let describe = |stacks: &[Stack]| {
        stacks
            .iter()
            .map(|stack| RecipeItem {
                item_id: stack.item.clone(),
                name: names.get(&stack.item).cloned().unwrap_or_default(),
                quantity: stack.quantity.to_string(),
            })
            .collect()
    };
    Ok(Json(
        recipes()
            .recipes
            .iter()
            .map(|recipe| RecipeInfo {
                id: recipe.id.clone(),
                tool: recipe.tool.clone(),
                inputs: describe(&recipe.inputs),
                outputs: describe(&recipe.outputs),
            })
            .collect(),
    ))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn patterns_match_prefix_suffix_and_exact() {
        assert!(matches("nature.tree_pine*", "nature.tree_pineTallA"));
        assert!(matches("nature.tree_*_fall", "nature.tree_simple_fall"));
        assert!(!matches("nature.tree_*_fall", "nature.tree_simple"));
        assert!(matches("nature.oak", "nature.oak") && !matches("nature.oak", "nature.oak2"));
        assert!(!matches("a*b", "a"));
    }

    #[test]
    fn every_tree_and_rock_model_has_one_resource_and_decor_has_none() {
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("../etc/world_objects.json")).unwrap();
        let mut trees = 0;
        let mut rocks = 0;
        for object in catalog["objects"].as_array().unwrap() {
            let model = object["id"].as_str().unwrap();
            let name = model.split_once('.').unwrap().1;
            let is_tree = name.starts_with("tree") || name == "oak" || name.contains("pine");
            let is_rock = name.starts_with("rock") || name.starts_with("stone");
            match resource_of(model) {
                Some(resource) if is_tree => {
                    assert_eq!(resource.kind, "tree", "{model}");
                    trees += 1;
                }
                Some(resource) if is_rock => {
                    assert_eq!(resource.kind, "rock", "{model}");
                    rocks += 1;
                }
                Some(resource) => panic!("{model} must not be harvestable as {}", resource.id),
                None => assert!(!is_tree && !is_rock, "{model} has no resource"),
            }
        }
        assert!(trees >= 60 && rocks >= 40, "{trees} trees, {rocks} rocks");
    }

    #[test]
    fn configuration_is_consistent() {
        let config = resources();
        for resource in &config.resources {
            assert!(
                !resource.tools.is_empty()
                    && resource
                        .tools
                        .iter()
                        .all(|(tool, points)| ["axe", "pickaxe"].contains(&tool.as_str())
                            && (1..=config.best_swing_points).contains(points))
                    && resource
                        .tools
                        .values()
                        .any(|points| *points == config.best_swing_points),
                "{}",
                resource.id
            );
            assert!(resource.hits > 0 && resource.regrow_minutes > 0);
            for drop in &resource.drops {
                assert!(0 < drop.min && drop.min <= drop.max, "{}", resource.id);
            }
            for bonus in &resource.bonus {
                assert!(
                    (0.0..=1.0).contains(&bonus.chance) && 0 < bonus.min && bonus.min <= bonus.max
                );
            }
        }
        for recipe in &recipes().recipes {
            assert!(recipe.tool.as_deref().is_none_or(|tool| tool == "axe"));
            assert!(
                !recipe.inputs.is_empty() && !recipe.outputs.is_empty(),
                "{}",
                recipe.id
            );
            assert!(recipe
                .inputs
                .iter()
                .chain(&recipe.outputs)
                .all(|stack| stack.quantity > 0));
        }
    }
}
