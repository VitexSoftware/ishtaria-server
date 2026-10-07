use super::building::give;
use super::creatures::animal_near;
use super::harvest::{position_of, teleport};
use super::*;

async fn post(router: &Router, token: &str, path: &str, body: serde_json::Value) -> Response {
    player_request(router, "POST", path, &body.to_string(), token).await
}

async fn learn(router: &Router, token: &str, scroll: &str) -> StatusCode {
    post(
        router,
        token,
        "/players/me/learn",
        serde_json::json!({"item_id": scroll}),
    )
    .await
    .status()
}

async fn cast(router: &Router, token: &str, spell: &str, target: Option<&str>) -> Response {
    let body = match target {
        Some(id) => serde_json::json!({"spell": spell, "object_id": id}),
        None => serde_json::json!({"spell": spell}),
    };
    post(router, token, "/players/me/cast", body).await
}

async fn rest(pool: &PgPool) {
    sqlx::query("UPDATE player_spells SET cooldown_until = NULL")
        .execute(pool)
        .await
        .unwrap();
}

async fn column(pool: &PgPool, column: &str) -> f64 {
    sqlx::query_scalar(&format!("SELECT {column}::float8 FROM players"))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn spells_are_learned_from_scrolls_for_good(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "mage").await;
    give(
        &pool,
        "mage",
        &[("scroll_firebolt", 2), ("scroll_refresh", 1), ("apple", 1)],
    )
    .await;

    assert_eq!(
        learn(&router, &token, "apple").await,
        StatusCode::CONFLICT,
        "not a scroll"
    );
    assert_eq!(
        learn(&router, &token, "scroll_ward").await,
        StatusCode::FORBIDDEN,
        "no such scroll in the bag yet the level is too low first"
    );
    assert_eq!(
        learn(&router, &token, "scroll_firebolt").await,
        StatusCode::FORBIDDEN,
        "level 2 is needed"
    );
    sqlx::query("UPDATE players SET level = 4")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        learn(&router, &token, "scroll_ward").await,
        StatusCode::CONFLICT,
        "a scroll that is not owned teaches nothing"
    );
    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM player_spells")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(known, 0, "the failed reading left no trace");

    assert_eq!(
        learn(&router, &token, "scroll_firebolt").await,
        StatusCode::OK
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT quantity FROM player_inventory WHERE item_id = 'scroll_firebolt'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 1, "a scroll is used up");
    assert_eq!(
        learn(&router, &token, "scroll_firebolt").await,
        StatusCode::CONFLICT,
        "already known"
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT quantity FROM player_inventory WHERE item_id = 'scroll_firebolt'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 1, "a refused reading keeps the scroll");
    assert_eq!(
        learn(&router, &token, "scroll_refresh").await,
        StatusCode::OK
    );

    let list =
        response_json(player_request(&router, "GET", "/players/me/spells", "", &token).await).await;
    let ids: Vec<_> = list["spells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|spell| spell["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["refresh", "firebolt"], "in the order of the book");
    assert_eq!(list["mana"], 100);
    assert_eq!(list["spells"][1]["effect"], "bolt");
    assert_eq!(list["spells"][1]["range_m"], 14.0);
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert_eq!(
        (
            profile["stats"]["mana"].as_i64(),
            profile["stats"]["mana_max"].as_i64()
        ),
        (Some(100), Some(100))
    );
    assert_eq!(
        player_request(&router, "GET", "/players/me/spells", "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn casting_costs_mana_rests_and_heals(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "mage").await;
    sqlx::query("UPDATE players SET level = 4")
        .execute(&pool)
        .await
        .unwrap();
    give(&pool, "mage", &[("scroll_heal", 1), ("scroll_refresh", 1)]).await;

    assert_eq!(
        cast(&router, &token, "heal", None).await.status(),
        StatusCode::FORBIDDEN,
        "not learned"
    );
    assert_eq!(
        cast(&router, &token, "unicorn", None).await.status(),
        StatusCode::NOT_FOUND
    );
    learn(&router, &token, "scroll_heal").await;
    learn(&router, &token, "scroll_refresh").await;

    sqlx::query("UPDATE players SET health = 40, stamina = 30")
        .execute(&pool)
        .await
        .unwrap();
    let healed = response_json(cast(&router, &token, "heal", None).await).await;
    assert_eq!(healed["effect"], "heal");
    assert_eq!(healed["player"]["stats"]["health"], 70);
    assert_eq!(healed["player"]["stats"]["mana"], 75, "25 mana were spent");
    assert_eq!(
        cast(&router, &token, "heal", None).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the spell rests"
    );
    assert_eq!(
        column(&pool, "health").await as i64,
        70,
        "a refused cast changes nothing"
    );

    // Another spell is not resting; stamina comes back, never above 100.
    let fresh = response_json(cast(&router, &token, "refresh", None).await).await;
    assert_eq!(fresh["player"]["stats"]["stamina"], 70);
    assert_eq!(fresh["player"]["stats"]["mana"], 60);
    rest(&pool).await;
    sqlx::query("UPDATE players SET health = 95")
        .execute(&pool)
        .await
        .unwrap();
    let capped = response_json(cast(&router, &token, "heal", None).await).await;
    assert_eq!(
        capped["player"]["stats"]["health"], 100,
        "health stays within bounds"
    );

    // Without mana nothing happens; mana comes back by the clock, offline too.
    rest(&pool).await;
    sqlx::query("UPDATE players SET mana = 5, mana_updated_at = clock_timestamp(), health = 10")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        cast(&router, &token, "heal", None).await.status(),
        StatusCode::CONFLICT,
        "not enough mana"
    );
    assert_eq!(column(&pool, "health").await as i64, 10);
    sqlx::query("UPDATE players SET mana_updated_at = clock_timestamp() - interval '100 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    let list =
        response_json(player_request(&router, "GET", "/players/me/spells", "", &token).await).await;
    assert_eq!(list["mana"], 45, "5 + 100 s x 0.4");
    sqlx::query(
        "UPDATE players SET mana = 100, mana_updated_at = clock_timestamp() - interval '1 day'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let list =
        response_json(player_request(&router, "GET", "/players/me/spells", "", &token).await).await;
    assert_eq!(list["mana"], 100, "mana never exceeds the maximum");
    assert_eq!(
        cast(&router, &token, "heal", None).await.status(),
        StatusCode::OK
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_ward_softens_the_bite_of_creatures(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "mage").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET level = 4")
        .execute(&pool)
        .await
        .unwrap();
    give(&pool, "mage", &[("scroll_ward", 1)]).await;
    learn(&router, &token, "scroll_ward").await;

    let bite = |pool: PgPool| async move {
        for _ in 0..400 {
            let mut transaction = pool.begin().await.unwrap();
            assert!(survival::lock_alive(&mut transaction, 1, id).await.unwrap());
            let counter = crate::combat::counter_attack(&mut transaction, id, "animal.wolf")
                .await
                .unwrap();
            transaction.commit().await.unwrap();
            if let Some(counter) = counter {
                return counter;
            }
        }
        panic!("the wolf never struck");
    };
    let bare = bite(pool.clone()).await;
    assert_eq!((bare.damage, bare.warded), (8, false));
    assert_eq!(
        cast(&router, &token, "ward", None).await.status(),
        StatusCode::OK
    );
    let warded = bite(pool.clone()).await;
    assert_eq!(
        (warded.damage, warded.warded),
        (4, true),
        "half of the bite gets through"
    );
    sqlx::query("UPDATE players SET warded_until = clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(bite(pool.clone()).await.damage, 8, "a ward does not last");
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_bolt_hits_animals_from_afar(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "mage").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET level = 4")
        .execute(&pool)
        .await
        .unwrap();
    give(&pool, "mage", &[("scroll_firebolt", 1)]).await;
    learn(&router, &token, "scroll_firebolt").await;
    let terrain = movement::terrain(&state).await.unwrap();
    let animal = animal_near(&terrain, None);
    let home = position_of(&pool, id).await;

    assert_eq!(
        cast(&router, &token, "firebolt", None).await.status(),
        StatusCode::BAD_REQUEST,
        "a bolt needs a target"
    );
    assert_eq!(
        cast(&router, &token, "firebolt", Some("nonsense"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // Far away the bolt does not reach, and nothing is spent.
    teleport(&pool, id, [home[0] + 80.0, home[1], home[2]]).await;
    let animal = terrain
        .animal_by_id(&animal.id, movement::unix_now_ms())
        .unwrap();
    assert_eq!(
        cast(&router, &token, "firebolt", Some(&animal.id))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        column(&pool, "mana").await as i64,
        100,
        "an out-of-range cast keeps the mana"
    );

    // From ten metres it hits; a small animal may fall at once and drop its loot.
    let animal = terrain
        .animal_by_id(&animal.id, movement::unix_now_ms())
        .unwrap();
    let near = [
        animal.position[0] + 10.0,
        animal.position[1],
        animal.position[2],
    ];
    teleport(&pool, id, near).await;
    let animal = terrain
        .animal_by_id(&animal.id, movement::unix_now_ms())
        .unwrap();
    let reply = cast(&router, &token, "firebolt", Some(&animal.id)).await;
    let reply = response_json(reply).await;
    assert_eq!(reply["effect"], "bolt");
    assert_eq!(reply["target"], animal.model);
    let state_of = reply["strike"]["state"].as_str().unwrap();
    assert!(["hit", "depleted"].contains(&state_of));
    if state_of == "depleted" {
        assert!(
            !reply["strike"]["items"].as_array().unwrap().is_empty(),
            "the kill drops loot"
        );
    }
    assert!(
        reply["strike"]["counter"].is_null(),
        "ten metres away nothing bites back"
    );
    assert_eq!(reply["player"]["stats"]["mana"], 80);
}
