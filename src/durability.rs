//! Wear of tools, weapons and shields.
//!
//! Every item type with `max_durability` wears out. A stack of such items has one piece in
//! use whose remaining durability is stored with the stack (`NULL` is a new piece). A piece
//! that wears out is destroyed; the next piece of the stack is new.

use super::players::Error;

/// What happened to the item that was used.
#[derive(Clone, Copy, serde::Serialize)]
pub(super) struct Wear {
    pub durability: i32,
    pub max_durability: i32,
    /// The piece wore out and is gone.
    pub broken: bool,
}

/// Wears the piece in use by `amount`. Items without durability are left alone.
pub(super) async fn wear(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    item_id: &str,
    amount: i32,
) -> Result<Option<Wear>, Error> {
    let row: Option<(i64, Option<i32>, Option<i32>)> = sqlx::query_as("SELECT quantity, durability, max_durability FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = $1 AND item_id = $2 FOR UPDATE OF player_inventory")
        .bind(player_id).bind(item_id).fetch_optional(&mut **transaction).await?;
    let Some((quantity, durability, Some(max))) = row else {
        return Ok(None);
    };
    let left = durability.unwrap_or(max) - amount.max(0);
    if left > 0 {
        sqlx::query(
            "UPDATE player_inventory SET durability = $3 WHERE player_id = $1 AND item_id = $2",
        )
        .bind(player_id)
        .bind(item_id)
        .bind(left)
        .execute(&mut **transaction)
        .await?;
        return Ok(Some(Wear {
            durability: left,
            max_durability: max,
            broken: false,
        }));
    }
    if quantity <= 1 {
        sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
            .bind(player_id)
            .bind(item_id)
            .execute(&mut **transaction)
            .await?;
        sqlx::query("UPDATE player_equipment SET hand = CASE WHEN hand = $2 THEN NULL ELSE hand END, offhand = CASE WHEN offhand = $2 THEN NULL ELSE offhand END, body = CASE WHEN body = $2 THEN NULL ELSE body END, hands = CASE WHEN hands = $2 THEN NULL ELSE hands END WHERE player_id = $1")
            .bind(player_id).bind(item_id).execute(&mut **transaction).await?;
    } else {
        sqlx::query("UPDATE player_inventory SET quantity = quantity - 1, durability = NULL WHERE player_id = $1 AND item_id = $2")
            .bind(player_id).bind(item_id).execute(&mut **transaction).await?;
    }
    Ok(Some(Wear {
        durability: 0,
        max_durability: max,
        broken: true,
    }))
}
