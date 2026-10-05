use super::harvest::{cooldown_reset, find_object, harvest_request, position_of, teleport};
use super::*;

async fn equip(router: &Router, token: &str, item: &str) -> serde_json::Value {
    let body = serde_json::json!({ "item_id": item }).to_string();
    response_json(player_request(router, "POST", "/players/me/equip", &body, token).await).await
}

fn item<'a>(profile: &'a serde_json::Value, id: &str) -> Option<&'a serde_json::Value> {
    profile["inventory"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["item_id"] == id)
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn tools_wear_out_and_are_destroyed(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "smith").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let spawn = position_of(&pool, player_id).await;
    let terrain = movement::terrain(&state).await.unwrap();
    let tree = find_object(&terrain, spawn, "tree");
    teleport(&pool, player_id, tree.position).await;

    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    let axe = item(&profile, "axe").unwrap();
    assert_eq!(
        (axe["durability"].as_i64(), axe["max_durability"].as_i64()),
        (Some(120), Some(120)),
        "a new tool"
    );
    assert!(
        item(&profile, "apple").unwrap()["max_durability"].is_null(),
        "food does not wear"
    );

    let swing = response_json(harvest_request(&router, &token, &tree.id).await).await;
    assert_eq!(swing["wear"]["durability"], 119);
    assert_eq!(swing["wear"]["broken"], false);
    assert_eq!(item(&swing["player"], "axe").unwrap()["durability"], 119);

    // The last bit of durability breaks the piece: the tool is gone and out of hand.
    sqlx::query("UPDATE player_inventory SET durability = 1 WHERE item_id = 'axe'")
        .execute(&pool)
        .await
        .unwrap();
    cooldown_reset(&pool).await;
    let last = response_json(harvest_request(&router, &token, &tree.id).await).await;
    assert_eq!(last["wear"]["broken"], true);
    assert!(item(&last["player"], "axe").is_none());
    assert!(last["player"]["equipment"]["hand"].is_null());
    cooldown_reset(&pool).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::CONFLICT,
        "no tool, no harvest"
    );

    // The next piece of a stack is new.
    sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity, durability) VALUES ($1, 'pickaxe', 1, 1) ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = 2, durability = 1")
        .bind(player_id).execute(&pool).await.unwrap();
    equip(&router, &token, "pickaxe").await;
    cooldown_reset(&pool).await;
    let rock = find_object(&terrain, spawn, "rock");
    teleport(&pool, player_id, rock.position).await;
    let broken = response_json(harvest_request(&router, &token, &rock.id).await).await;
    assert_eq!(broken["wear"]["broken"], true);
    let spare = item(&broken["player"], "pickaxe").unwrap();
    assert_eq!(
        (spare["quantity"].as_str(), spare["durability"].as_i64()),
        (Some("1"), Some(120))
    );
    assert_eq!(
        broken["player"]["equipment"]["hand"], "pickaxe",
        "the spare stays in hand"
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_shield_in_the_other_hand_blocks(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "guard").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(block(&router, "").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        block(&router, &token).await.status(),
        StatusCode::CONFLICT,
        "no shield yet"
    );
    assert_eq!(
        equip_status(&router, &token, "apple").await,
        StatusCode::CONFLICT
    );
    sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, 'shield_round', 1)")
        .bind(player_id).execute(&pool).await.unwrap();
    let held = equip(&router, &token, "shield_round").await;
    assert_eq!(held["equipment"]["offhand"], "shield_round");
    assert_eq!(
        held["equipment"]["hand"], "axe",
        "the tool stays in the dominant hand"
    );
    assert_eq!(held["equipment"]["blocking"], false);

    let first = response_json(block(&router, &token).await).await;
    assert_eq!(first["blocking_seconds"], 2);
    assert_eq!(first["wear"]["durability"], 149);
    assert_eq!(first["player"]["equipment"]["blocking"], true);
    let renewed = response_json(block(&router, &token).await).await;
    assert!(
        renewed["wear"].is_null(),
        "renewing a running block does not wear the shield again"
    );
    assert_eq!(
        item(&renewed["player"], "shield_round").unwrap()["durability"],
        149
    );

    // Once the block has ended, the next one wears the shield again.
    sqlx::query("UPDATE players SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        response_json(block(&router, &token).await).await["wear"]["durability"],
        148
    );

    // A shield that wears out is destroyed and the block ends.
    sqlx::query("UPDATE players SET blocked_until = NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE player_inventory SET durability = 1 WHERE item_id = 'shield_round'")
        .execute(&pool)
        .await
        .unwrap();
    let broken = response_json(block(&router, &token).await).await;
    assert_eq!(broken["wear"]["broken"], true);
    assert!(broken["player"]["equipment"]["offhand"].is_null());
    assert_eq!(broken["player"]["equipment"]["blocking"], false);
    assert_eq!(broken["player"]["equipment"]["hand"], "axe");

    // Taking the shield out of the other hand leaves the hand alone.
    sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, 'shield_round', 1)")
        .bind(player_id).execute(&pool).await.unwrap();
    equip(&router, &token, "shield_round").await;
    let bare = response_json(
        player_request(
            &router,
            "DELETE",
            "/players/me/equip?slot=offhand",
            "",
            &token,
        )
        .await,
    )
    .await;
    assert!(bare["equipment"]["offhand"].is_null());
    assert_eq!(bare["equipment"]["hand"], "axe");
    assert_eq!(
        player_request(&router, "DELETE", "/players/me/equip?slot=feet", "", &token)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

async fn equip_status(router: &Router, token: &str, item: &str) -> StatusCode {
    let body = serde_json::json!({ "item_id": item }).to_string();
    player_request(router, "POST", "/players/me/equip", &body, token)
        .await
        .status()
}

async fn block(router: &Router, token: &str) -> Response {
    player_request(router, "POST", "/players/me/block", "", token).await
}
