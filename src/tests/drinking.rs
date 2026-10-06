use super::*;

async fn water_of(pool: &PgPool, id: i64) -> f64 {
    sqlx::query_scalar("SELECT (water + water_fraction)::float8 FROM players WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn sleeping_characters_keep_their_water_and_food_and_drink_restore_it(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "sleeper").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Hours without a client or movement cost nothing: the character is asleep.
    sqlx::query("UPDATE players SET water = 60, water_fraction = 0, last_active_at = now() - interval '5 hours', last_seen_at = now() - interval '5 hours', survival_updated_at = now() - interval '5 hours' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    assert!((water_of(&pool, id).await - 60.0).abs() < 1e-6);
    let health: i32 = sqlx::query_scalar("SELECT health FROM players WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(health, 100, "no dehydration while asleep");

    // A character whose client is polling is awake and thirsty: 100 s cost one point.
    sqlx::query("UPDATE players SET last_seen_at = now(), survival_updated_at = now() - interval '100 seconds' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    let awake = water_of(&pool, id).await;
    assert!((awake - 59.0).abs() < 0.05, "awake drain: {awake}");

    // Fruit holds water; bread and cheese hardly any; the reserve never exceeds 100.
    sqlx::query("UPDATE players SET water = 50, water_fraction = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"apple"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert!(
        (profile["stats"]["water"].as_f64().unwrap() - 58.0).abs() < 1.5,
        "{}",
        profile["stats"]
    );
    let apple = profile["inventory"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["item_id"] == "apple")
        .unwrap();
    assert_eq!(
        apple["water"], 8,
        "clients are told how much water food holds"
    );
    sqlx::query("UPDATE players SET water = 97, water_fraction = 0.5 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"apple"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert!(
        profile["stats"]["water"].as_i64().unwrap() >= 99,
        "{}",
        profile["stats"]
    );
    assert!(water_of(&pool, id).await > 99.9 && water_of(&pool, id).await <= 100.0 + 1e-9);

    // Drinking needs fresh water within reach; the spawn of this test world has none.
    let refused = player_request(&router, "POST", "/players/me/drink", "", &token).await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let unauthenticated = player_request(&router, "POST", "/players/me/drink", "", "").await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
}
