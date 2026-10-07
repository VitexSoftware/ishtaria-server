use super::building::give;
use super::*;

async fn strike(
    pool: &PgPool,
    state: &AppState,
    player_id: i64,
    model: &str,
) -> crate::combat::Counter {
    // The animal bites back by chance; ask until it does.
    for _ in 0..400 {
        let mut transaction = pool.begin().await.unwrap();
        assert!(
            survival::lock_alive(&mut transaction, state.world_id, player_id)
                .await
                .unwrap()
        );
        let counter = crate::combat::counter_attack(&mut transaction, player_id, model)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        if let Some(counter) = counter {
            return counter;
        }
    }
    panic!("{model} never struck");
}

async fn health(pool: &PgPool, id: i64) -> i32 {
    sqlx::query_scalar("SELECT health FROM players WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn equip(router: &Router, token: &str, item: &str) -> serde_json::Value {
    response_json(
        player_request(
            router,
            "POST",
            "/players/me/equip",
            &serde_json::json!({ "item_id": item }).to_string(),
            token,
        )
        .await,
    )
    .await
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn armour_is_worn_by_slot_and_counted(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "knight").await;
    give(
        &pool,
        "knight",
        &[
            ("armor_metal", 1),
            ("armor_leather", 1),
            ("glove", 1),
            ("apple", 1),
        ],
    )
    .await;

    let worn = equip(&router, &token, "armor_metal").await;
    assert_eq!(worn["equipment"]["body"], "armor_metal");
    assert_eq!(worn["equipment"]["defense"], 35);
    let worn = equip(&router, &token, "glove").await;
    assert_eq!(worn["equipment"]["hands"], "glove");
    assert_eq!(worn["equipment"]["defense"], 40);
    assert_eq!(
        worn["equipment"]["hand"], "axe",
        "the weapon hand is untouched"
    );
    // A second body armour replaces the first.
    let worn = equip(&router, &token, "armor_leather").await;
    assert_eq!(worn["equipment"]["body"], "armor_leather");
    assert_eq!(worn["equipment"]["defense"], 20);
    for item in ["apple", "nothing"] {
        assert_eq!(
            player_request(
                &router,
                "POST",
                "/players/me/equip",
                &serde_json::json!({ "item_id": item }).to_string(),
                &token
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
    }
    let bare = response_json(
        player_request(&router, "DELETE", "/players/me/equip?slot=body", "", &token).await,
    )
    .await;
    assert!(bare["equipment"]["body"].is_null());
    assert_eq!(bare["equipment"]["defense"], 5);
    assert_eq!(
        player_request(&router, "DELETE", "/players/me/equip?slot=feet", "", &token)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn dangerous_animals_bite_back_and_armour_helps(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "knight").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // A harmless animal never bites.
    let mut transaction = pool.begin().await.unwrap();
    for _ in 0..50 {
        assert!(
            crate::combat::counter_attack(&mut transaction, id, "animal.pig")
                .await
                .unwrap()
                .is_none()
        );
    }
    transaction.commit().await.unwrap();
    assert_eq!(health(&pool, id).await, 100);

    // Bare, a wolf does its full damage.
    let bare = strike(&pool, &state, id, "animal.wolf").await;
    assert_eq!(
        (bare.attempted, bare.damage, bare.blocked, bare.defense),
        (8, 8, false, 0)
    );
    assert_eq!(health(&pool, id).await, 92);

    // Armour takes a share, wears, and a broken piece is gone.
    give(&pool, "knight", &[("armor_metal", 1), ("glove", 1)]).await;
    equip(&router, &token, "armor_metal").await;
    equip(&router, &token, "glove").await;
    let armoured = strike(&pool, &state, id, "animal.wolf").await;
    assert_eq!((armoured.damage, armoured.defense), (4, 40));
    assert_eq!(armoured.wear.len(), 2);
    assert_eq!(health(&pool, id).await, 88);
    sqlx::query("UPDATE player_inventory SET durability = 1 WHERE item_id = 'glove'")
        .execute(&pool)
        .await
        .unwrap();
    let broken = strike(&pool, &state, id, "animal.wolf").await;
    assert!(
        broken.wear.iter().any(|wear| wear.broken),
        "the glove wore out"
    );
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert!(profile["equipment"]["hands"].is_null());
    assert_eq!(profile["equipment"]["defense"], 35);

    // A raised shield takes most of the blow, and wears.
    sqlx::query("UPDATE player_equipment SET body = NULL WHERE player_id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    give(&pool, "knight", &[("shield_round", 1)]).await;
    equip(&router, &token, "shield_round").await;
    let down = strike(&pool, &state, id, "animal.wolf").await;
    assert!(!down.blocked, "a shield that is not raised does not help");
    sqlx::query("UPDATE players SET blocked_until = clock_timestamp() + interval '2 seconds', health = 100 WHERE id = $1").bind(id).execute(&pool).await.unwrap();
    let blocked = strike(&pool, &state, id, "animal.wolf").await;
    assert!(blocked.blocked);
    assert_eq!(blocked.damage, 2);
    assert!(!blocked.wear.is_empty());

    // A blow that takes the last health kills for good and leaves a grave.
    sqlx::query("UPDATE players SET health = 1, blocked_until = NULL WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM player_equipment")
        .execute(&pool)
        .await
        .unwrap();
    let fatal = strike(&pool, &state, id, "animal.wolf").await;
    assert!(fatal.died);
    let cause: String = sqlx::query_scalar("SELECT cause FROM graves WHERE player_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cause, "creature");
    assert_eq!(
        player_request(&router, "GET", "/players/me", "", &token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn weapons_and_armour_are_made_from_ore_and_hides(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "smith").await;
    give(
        &pool,
        "smith",
        &[("iron_ore", 20), ("pine_wood", 10), ("hide", 8)],
    )
    .await;
    let craft = |recipe: &str, count: u32| {
        let body = serde_json::json!({ "recipe": recipe, "count": count.to_string() }).to_string();
        let router = router.clone();
        let token = token.clone();
        async move {
            player_request(&router, "POST", "/players/me/craft", &body, &token)
                .await
                .status()
        }
    };
    assert_eq!(
        craft("make_sword_2", 1).await,
        StatusCode::CONFLICT,
        "no ingots yet"
    );
    assert_eq!(
        craft("smelt_iron_pine", 2).await,
        StatusCode::CONFLICT,
        "ore is smelted at a fire"
    );
    // A campfire and an anvil stand where the smith stands.
    sqlx::query("INSERT INTO placed_objects (world_id, kind, position_x, position_y, position_z) SELECT world_id, kind, position_x, position_y, position_z FROM players, (VALUES ('campfire'), ('anvil')) AS station(kind) WHERE username = 'smith'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(craft("smelt_iron_pine", 2).await, StatusCode::OK);
    assert_eq!(craft("make_dagger", 1).await, StatusCode::OK);
    assert_eq!(craft("make_leather_armor", 1).await, StatusCode::OK);
    assert_eq!(
        craft("make_leather_armor", 1).await,
        StatusCode::CONFLICT,
        "the hides are used up"
    );
    let stock = |item: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory WHERE item_id = $1")
                .bind(item).fetch_one(&pool).await.unwrap()
        }
    };
    assert_eq!(stock("iron_ingot").await, 0, "two ingots became a dagger");
    assert_eq!(stock("dagger").await, 1);
    assert_eq!(stock("armor_leather").await, 1);
    equip(&router, &token, "armor_leather").await;
    equip(&router, &token, "dagger").await;
}
