use super::building::{finish_portal, two_worlds};
use super::harvest::{cooldown_reset, find_object, harvest_request, position_of, teleport};
use super::*;

async fn stats_of(router: &Router, token: &str) -> serde_json::Value {
    response_json(player_request(router, "GET", "/players/me", "", token).await).await
}

async fn experience_of(pool: &PgPool, username: &str) -> (i64, i32) {
    sqlx::query_as("SELECT experience, level FROM players WHERE username = $1")
        .bind(username)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn swings_and_crafting_give_experience_and_levels(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "learner").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let profile = stats_of(&router, &token).await;
    assert_eq!(profile["stats"]["level"], 1);
    assert_eq!(profile["stats"]["experience"], 0);
    assert_eq!(profile["stats"]["level_experience"], 0);
    assert_eq!(profile["stats"]["next_level_experience"], 50);

    // Every swing is one point, whether or not the tree falls.
    let spawn = position_of(&pool, player_id).await;
    let terrain = movement::terrain(&state).await.unwrap();
    let tree = find_object(&terrain, spawn, "tree");
    teleport(&pool, player_id, tree.position).await;
    let reply = response_json(harvest_request(&router, &token, &tree.id).await).await;
    assert_eq!(reply["xp"], 1);
    assert_eq!(reply["player"]["stats"]["experience"], 1);
    assert_eq!(experience_of(&pool, "learner").await, (1, 1));

    // A failed swing gives nothing.
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(experience_of(&pool, "learner").await.0, 1);

    // Reaching a threshold raises the level, and the capacity with it.
    sqlx::query("UPDATE players SET experience = 49")
        .execute(&pool)
        .await
        .unwrap();
    cooldown_reset(&pool).await;
    let reply = response_json(harvest_request(&router, &token, &tree.id).await).await;
    assert_eq!(reply["player"]["stats"]["experience"], 50);
    assert_eq!(reply["player"]["stats"]["level"], 2);
    assert_eq!(reply["player"]["stats"]["level_experience"], 50);
    assert_eq!(reply["player"]["stats"]["next_level_experience"], 150);
    assert_eq!(reply["player"]["inventory"]["capacity"], 110);

    // Crafting is worth more than a swing.
    super::building::give(&pool, "learner", &[("oak_log", 2), ("stone", 8)]).await;
    let craft = |recipe: &'static str, count: &'static str| {
        let router = router.clone();
        let token = token.clone();
        async move {
            player_request(
                &router,
                "POST",
                "/players/me/craft",
                &serde_json::json!({ "recipe": recipe, "count": count }).to_string(),
                &token,
            )
            .await
        }
    };
    let before = experience_of(&pool, "learner").await.0;
    assert_eq!(craft("chop_oak_log", "2").await.status(), StatusCode::OK);
    assert_eq!(
        experience_of(&pool, "learner").await.0,
        before + 6,
        "3 points for each log chopped"
    );
    assert_eq!(craft("oak_plank", "2").await.status(), StatusCode::OK);
    assert_eq!(
        experience_of(&pool, "learner").await.0,
        before + 6 + 8,
        "4 points for each batch of planks"
    );
    assert_eq!(craft("stone_block", "2").await.status(), StatusCode::OK);
    assert_eq!(
        experience_of(&pool, "learner").await.0,
        before + 6 + 8 + 10,
        "5 points for each stone block"
    );
    assert_eq!(
        craft("oak_plank", "50").await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        experience_of(&pool, "learner").await.0,
        before + 24,
        "a refused craft gives nothing"
    );

    // Levels beyond 30 count for the score but give no more inventory slots.
    sqlx::query("UPDATE players SET experience = $1, level = 1")
        .bind(25_i64 * 40 * 39)
        .execute(&pool)
        .await
        .unwrap();
    super::building::give(&pool, "learner", &[("stone", 8)]).await;
    cooldown_reset(&pool).await;
    let profile = stats_of(&router, &token).await;
    let (_, level) = experience_of(&pool, "learner").await;
    assert_eq!(level, 1, "levels only change when experience is gained");
    let gained = response_json(craft("stone_block", "1").await).await;
    assert_eq!(gained["stats"]["level"], 40);
    assert_eq!(
        gained["inventory"]["capacity"],
        100 + 10 * 29,
        "capacity stops growing at level 30"
    );
    let _ = profile;
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn building_a_portal_gives_the_most_experience(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = two_worlds(&pool, "approve", "approve").await;
    assert_eq!(experience_of(&pool, "vitex").await.0, 0);
    finish_portal(&pool, &p.a, "vitex", &p.vitex, "brana-sever").await;
    // 5 points for each of 10 + 20 + 2 units, and 500 for the finished end.
    let (experience, level) = experience_of(&pool, "vitex").await;
    assert_eq!(experience, 5 * (10 + 20 + 2) + 500);
    assert_eq!(level, 5, "660 points are level 5");
    assert_eq!(
        experience_of(&pool, "anna").await.0,
        0,
        "the other world's player is not affected"
    );
}
