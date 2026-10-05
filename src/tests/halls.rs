use super::*;

async fn wealth_row(router: &Router) -> Vec<(String, String, bool, bool)> {
    response_json(request(router, "/hall-of-fame").await)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_owned(),
                entry["wealth"].as_str().unwrap().to_owned(),
                entry["alive"].as_bool().unwrap(),
                entry["looted"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn the_hall_of_fame_ranks_the_richest_characters(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let empty = response_json(request(&router, "/hall-of-fame").await).await;
    assert!(empty.as_array().unwrap().is_empty());

    for index in 0..13 {
        sqlx::query("INSERT INTO players (world_id, username, password_hash, created_at) VALUES ($1, $2, 'x', now() - make_interval(days => $3))")
            .bind(world_id).bind(format!("hero-{index:02}")).bind(index + 1).execute(&pool).await.unwrap();
    }
    // Score = 100 * level + 10 * days. hero-04 is 5 days old.
    for (name, level, wealth) in [
        ("hero-05", 3, 5000_i64),
        ("hero-02", 2, 9_000_000_000_000),
        ("hero-07", 1, 5000),
        ("hero-09", 6, 250),
        ("hero-04", 6, 100),
    ] {
        sqlx::query("UPDATE players SET level = $2, lifetime_gold = $3 WHERE username = $1")
            .bind(name)
            .bind(level)
            .bind(wealth)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE players SET banned_at = now(), level = 50 WHERE username = 'hero-12'")
        .execute(&pool)
        .await
        .unwrap();

    let hall = response_json(request(&router, "/hall-of-fame").await).await;
    let hall = hall.as_array().unwrap();
    assert_eq!(hall.len(), 10, "ten at most");
    let names: Vec<&str> = hall
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    // hero-09: 600 + 100 = 700, hero-04: 600 + 50 = 650, hero-05: 300 + 60 = 360, hero-02: 200 + 30 = 230.
    assert_eq!(
        &names[..4],
        ["hero-09", "hero-04", "hero-05", "hero-02"],
        "ranked by score"
    );
    assert!(!names.contains(&"hero-12"), "banned players are not listed");
    assert_eq!(hall[0]["level"], 6);
    assert_eq!(hall[0]["score"], "700");
    assert_eq!(hall[0]["lived_days"], "10");
    assert_eq!(
        hall[0]["wealth"], "250",
        "wealth is shown but does not rank"
    );
    assert_eq!(
        hall[3]["wealth"], "9000000000000",
        "wealth is an exact string"
    );
    assert_eq!(hall[0]["alive"], true);
    assert_eq!(hall[0]["looted"], false);
    assert!(hall[0].get("password_hash").is_none() && hall[0].get("id").is_none());
    assert_eq!(
        request(&router, "/hall-of-fame?x=1").await.status(),
        StatusCode::OK
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_robbed_grave_takes_the_wealth_from_the_hall_of_fame(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let rich = create_session(&router, "rich-dead").await;
    let _ = rich;
    let poor = create_session(&router, "robber").await;
    let ids: Vec<(String, i64)> =
        sqlx::query_as("SELECT username, id FROM players ORDER BY username")
            .fetch_all(&pool)
            .await
            .unwrap();
    let rich_id = ids.iter().find(|(name, _)| name == "rich-dead").unwrap().1;
    sqlx::query("UPDATE players SET lifetime_gold = 5000000, level = 9 WHERE id = $1")
        .bind(rich_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET position_x = 0, position_y = 0, position_z = 0 WHERE true")
        .execute(&pool)
        .await
        .unwrap();
    survival::apply_damage(&state, rich_id, 100, survival::DamageCause::Disease)
        .await
        .unwrap();

    // The dead character keeps their wealth, marked as dead, while the grave is untouched.
    let hall = wealth_row(&router).await;
    assert_eq!(
        hall[0],
        ("rich-dead".into(), "5000000".into(), false, false)
    );
    assert_eq!(hall[1].0, "robber");
    let both = response_json(request(&router, "/hall-of-fame").await).await;
    assert_eq!(both[0]["name"], "rich-dead");
    assert_eq!(
        both[0]["level"], 9,
        "a dead character keeps the level reached"
    );

    // Taking anything from the grave removes the wealth from the hall.
    let grave_id: i64 = sqlx::query_scalar("SELECT id FROM graves")
        .fetch_one(&pool)
        .await
        .unwrap();
    let response = player_request(
        &router,
        "POST",
        &format!("/graves/{grave_id}/loot"),
        r#"{"item_id":"gold","quantity":"50"}"#,
        &poor,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let hall = wealth_row(&router).await;
    let dead = hall.iter().find(|row| row.0 == "rich-dead").unwrap();
    assert_eq!(dead, &("rich-dead".to_owned(), "0".to_owned(), false, true));
    assert_eq!(
        hall[0].0, "rich-dead",
        "the score of the dead stays, only the wealth is gone"
    );
    let looted: bool = sqlx::query_scalar("SELECT looted_at IS NOT NULL FROM graves")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(looted);
    // The memorial itself stays permanent: the record of the life does not change.
    let lifetime: i64 = sqlx::query_scalar("SELECT lifetime_gold FROM graves")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lifetime, 5_000_000);
}
