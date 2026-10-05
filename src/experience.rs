//! Experience and levels.
//!
//! Every swing at a tree or a rock is worth 1 point; crafting is worth more (set per recipe
//! in `etc/recipes.json`) and building a portal still more: 5 points for every unit of
//! material delivered and 500 for a finished end. A character reaches level `n` with
//! `25 * n * (n - 1)` points: 50 for level 2, 150 for level 3, 500 for level 5, 2250 for
//! level 10, 9500 for level 20. The score of a character in the hall of fame is
//! `100 * level + 10 * days lived`.

use super::players::Error;

/// Experience for finishing one end of a portal, on top of the points per delivered unit.
pub(super) const BUILD_END_XP: i64 = 500;
/// Experience for each unit of material delivered to a construction site.
pub(super) const BUILD_UNIT_XP: i64 = 5;
/// Levels above this one still count for the score but give no more inventory slots.
pub(super) const MAX_CAPACITY_LEVEL: i32 = 30;

/// Total experience a character needs to be at `level`.
pub(super) fn experience_for(level: i32) -> i64 {
    25 * i64::from(level) * i64::from(level - 1)
}

/// The level a character with this much experience has.
pub(super) fn level_for(experience: i64) -> i32 {
    let mut level = ((1.0 + (1.0 + 4.0 * experience as f64 / 25.0).sqrt()) / 2.0).floor() as i32;
    level = level.max(1);
    while experience_for(level + 1) <= experience {
        level += 1;
    }
    while level > 1 && experience_for(level) > experience {
        level -= 1;
    }
    level
}

/// Adds experience to a character and raises the level when a threshold is passed.
/// Returns the new level and experience.
pub(super) async fn grant(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    amount: i64,
) -> Result<(i32, i64), Error> {
    let (experience, old_level): (i64, i32) = sqlx::query_as(
        "UPDATE players SET experience = experience + $2 WHERE id = $1 RETURNING experience, level",
    )
    .bind(player_id)
    .bind(amount)
    .fetch_one(&mut **transaction)
    .await?;
    let level = level_for(experience);
    if level > old_level {
        // A new level restores health, stamina and water (with five seconds before they start to
        // fall again), and friends are told.
        sqlx::query("UPDATE players SET level = $2, health = 100, health_fraction = 0, stamina = 100, stamina_fraction = 0, water = 100, water_fraction = 0, activity_seconds = 0, survival_updated_at = clock_timestamp() + interval '5 seconds' WHERE id = $1")
            .bind(player_id)
            .bind(level)
            .execute(&mut **transaction)
            .await?;
        sqlx::query("INSERT INTO player_events (recipient_id, kind, subject, level) SELECT CASE WHEN player_id = $1 THEN friend_id ELSE player_id END, 'level_up', (SELECT username FROM players WHERE id = $1), $2 FROM player_friendships WHERE player_id = $1 OR friend_id = $1")
            .bind(player_id)
            .bind(level)
            .execute(&mut **transaction)
            .await?;
    }
    Ok((level, experience))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn thresholds_follow_the_table() {
        for (level, experience) in [
            (1, 0),
            (2, 50),
            (3, 150),
            (4, 300),
            (5, 500),
            (10, 2250),
            (20, 9500),
            (30, 21750),
            (50, 61250),
        ] {
            assert_eq!(experience_for(level), experience, "level {level}");
            assert_eq!(level_for(experience), level, "at {experience}");
            assert_eq!(
                level_for(experience.max(1) - i64::from(level > 1)),
                if level > 1 { level - 1 } else { 1 },
                "one point short of level {level}"
            );
        }
    }

    #[test]
    fn level_and_threshold_are_inverse() {
        let mut previous = 1;
        for experience in 0..200_000 {
            let level = level_for(experience);
            assert!(level >= previous && level - previous <= 1, "{experience}");
            assert!(
                experience_for(level) <= experience && experience < experience_for(level + 1),
                "{experience}"
            );
            previous = level;
        }
        assert_eq!(level_for(i64::MAX / 4), level_for(i64::MAX / 4));
    }
}
