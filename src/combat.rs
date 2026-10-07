//! Armour, blocking and the counter-attacks of dangerous animals.
//!
//! Which armour goes where, how much it takes away and which animals strike back is data in
//! `etc/combat.json`. The server decides every outcome: a client only swings at an animal and
//! learns from the reply what it cost. Armour and shields wear out with the blows they stop.

use super::durability::{self, Wear};
use super::players::Error;
use super::survival::{self, DamageCause};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Deserialize)]
pub(super) struct Armor {
    /// `body` or `hands`.
    pub slot: String,
    /// Percent of the damage that this piece takes away.
    pub defense: i32,
}

#[derive(Deserialize)]
struct Fierce {
    damage: i32,
    /// How likely the animal bites back after a hit it survives.
    chance: f64,
}

#[derive(Deserialize)]
struct Combat {
    version: u32,
    max_defense_percent: i32,
    block_damage_factor: f64,
    shield_wear: i32,
    armor_wear: i32,
    armor: HashMap<String, Armor>,
    fierce: HashMap<String, Fierce>,
}

fn combat() -> &'static Combat {
    static CONFIG: OnceLock<Combat> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Combat = serde_json::from_str(include_str!("../etc/combat.json"))
            .expect("bundled combat rules must be valid");
        assert_eq!(config.version, 1);
        assert!((0.0..=1.0).contains(&config.block_damage_factor));
        assert!((0..=100).contains(&config.max_defense_percent));
        config
    })
}

/// The slot an armour item is worn in, if it is armour.
pub(super) fn armor(item: &str) -> Option<&'static Armor> {
    combat().armor.get(item)
}

/// Items the configuration refers to, for checking them against the item catalog.
#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    combat().armor.keys().cloned().collect()
}

/// Damage that gets through `defense` percent of armour, rounded down but never to nothing.
fn after_armor(damage: i32, defense: i32) -> i32 {
    let percent = defense.clamp(0, combat().max_defense_percent);
    (damage * (100 - percent) / 100).max(1)
}

/// What a blow cost the character.
#[derive(Serialize)]
pub(super) struct Counter {
    /// The damage the animal meant to do.
    pub attempted: i32,
    /// The damage that reached the character after armour and shield.
    pub damage: i32,
    /// The shield was up and took most of the blow.
    pub blocked: bool,
    /// A ward (spell) was active and softened the blow.
    pub warded: bool,
    /// Percent of the blow that armour took away.
    pub defense: i32,
    pub died: bool,
    pub wear: Vec<Wear>,
}

/// The armour a living character wears that is still in their inventory, with its defense.
async fn worn(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<Vec<(String, i32)>, Error> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = body) THEN body END, CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = hands) THEN hands END FROM player_equipment WHERE player_id = $1")
        .bind(player_id).fetch_optional(&mut **transaction).await?;
    let (body, hands) = row.unwrap_or_default();
    Ok([body, hands]
        .into_iter()
        .flatten()
        .filter_map(|item| armor(&item).map(|armor| (item, armor.defense)))
        .collect())
}

/// Lets the animal named `model` bite back at a character who just hit it and left it alive.
/// Returns `None` when the animal is not dangerous or this time it did not strike.
/// The caller holds the character locked and alive in `transaction`.
pub(super) async fn counter_attack(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    model: &str,
) -> Result<Option<Counter>, Error> {
    let config = combat();
    let Some(fierce) = config.fierce.get(model) else {
        return Ok(None);
    };
    if !OsRng.gen_bool(fierce.chance.clamp(0.0, 1.0)) {
        return Ok(None);
    }
    let pieces = worn(transaction, player_id).await?;
    let defense: i32 = pieces.iter().map(|(_, defense)| defense).sum();
    let defense = defense.min(config.max_defense_percent);
    let mut damage = after_armor(fierce.damage, defense);
    // The shield in the other hand only helps while a block lasts.
    let shield: Option<String> = sqlx::query_scalar("SELECT offhand FROM player_equipment JOIN players ON players.id = player_id WHERE player_id = $1 AND coalesce(blocked_until > clock_timestamp(), false) AND EXISTS (SELECT 1 FROM player_inventory WHERE player_inventory.player_id = $1 AND item_id = offhand)")
        .bind(player_id).fetch_optional(&mut **transaction).await?.flatten();
    let blocked = shield.is_some();
    if blocked {
        damage = ((f64::from(damage) * config.block_damage_factor).round() as i32).max(1);
    }
    // A ward (spell) takes a share off what armour and shield left.
    let warded: bool = sqlx::query_scalar(
        "SELECT coalesce(warded_until > clock_timestamp(), false) FROM players WHERE id = $1",
    )
    .bind(player_id)
    .fetch_one(&mut **transaction)
    .await?;
    if warded {
        damage = ((f64::from(damage) * super::magic::ward_damage_factor()).round() as i32).max(1);
    }
    let mut wear = Vec::new();
    for (item, _) in &pieces {
        wear.extend(durability::wear(transaction, player_id, item, config.armor_wear).await?);
    }
    if let Some(shield) = &shield {
        wear.extend(durability::wear(transaction, player_id, shield, config.shield_wear).await?);
    }
    let died = survival::hurt(transaction, player_id, damage, DamageCause::Creature).await?;
    Ok(Some(Counter {
        attempted: fierce.damage,
        damage,
        blocked,
        warded,
        defense,
        died,
        wear,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armor_takes_a_share_but_never_everything() {
        assert_eq!(after_armor(10, 0), 10);
        assert_eq!(after_armor(10, 30), 7);
        assert_eq!(after_armor(10, 95), 4, "defense is capped");
        assert_eq!(after_armor(1, 60), 1, "a blow always hurts a little");
    }

    #[test]
    fn bundled_rules_are_consistent() {
        let config = combat();
        for armor in config.armor.values() {
            assert!(["body", "hands"].contains(&armor.slot.as_str()));
            assert!((1..=config.max_defense_percent).contains(&armor.defense));
        }
        for fierce in config.fierce.values() {
            assert!((1..=100).contains(&fierce.damage));
            assert!((0.0..=1.0).contains(&fierce.chance));
        }
    }
}
