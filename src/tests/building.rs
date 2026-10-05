use super::pacts::{world, Network, World};
use super::*;
use std::sync::Arc;

pub(super) struct Pair {
    pub(super) network: Arc<Network>,
    pub(super) a: World,
    pub(super) b: World,
    pub(super) vitex: String,
    pub(super) anna: String,
}

/// Two worlds with one player each.
pub(super) async fn two_worlds(pool: &PgPool, policy_a: &str, policy_b: &str) -> Pair {
    let network = Arc::new(Network::default());
    let a = world(pool, &network, "svet-a", policy_a).await;
    let b = world(pool, &network, "svet-b", policy_b).await;
    let vitex = create_session(&a.router, "vitex").await;
    let anna = create_session(&b.router, "anna").await;
    Pair {
        network,
        a,
        b,
        vitex,
        anna,
    }
}

pub(super) async fn give(pool: &PgPool, username: &str, items: &[(&str, i64)]) {
    for (item, quantity) in items {
        sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) SELECT id, $2, $3 FROM players WHERE username = $1 ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = EXCLUDED.quantity")
            .bind(username).bind(item).bind(quantity).execute(pool).await.unwrap();
    }
}

/// Starts a portal where the player stands.
pub(super) async fn build_portal(world: &World, token: &str, name: &str) -> Response {
    player_request(
        &world.router,
        "POST",
        "/portals/build",
        &serde_json::json!({ "portal_name": name }).to_string(),
        token,
    )
    .await
}

pub(super) async fn deliver(
    world: &World,
    token: &str,
    portal: &str,
    item: &str,
    quantity: &str,
) -> Response {
    player_request(
        &world.router,
        "POST",
        &format!("/portals/mine/{portal}/contribute"),
        &serde_json::json!({ "item_id": item, "quantity": quantity }).to_string(),
        token,
    )
    .await
}

/// Builds a portal and delivers every material; returns its id.
pub(super) async fn finish_portal(
    pool: &PgPool,
    world: &World,
    username: &str,
    token: &str,
    name: &str,
) -> String {
    let response = build_portal(world, token, name).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    give(
        pool,
        username,
        &[
            ("stone_block", 10),
            ("pine_plank", 12),
            ("oak_plank", 8),
            ("quartz_crystal", 2),
        ],
    )
    .await;
    for (item, quantity) in [
        ("stone_block", "10"),
        ("pine_plank", "12"),
        ("oak_plank", "8"),
        ("quartz_crystal", "2"),
    ] {
        assert_eq!(
            deliver(world, token, &id, item, quantity).await.status(),
            StatusCode::OK,
            "{item}"
        );
    }
    id
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_portal_is_built_on_its_own_from_delivered_materials(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = two_worlds(&pool, "open", "open").await;

    // Names are checked, sessions belong to one world, and the player must be alive and standing.
    for bad in ["", "Brana", "a b", &"x".repeat(65)] {
        assert_eq!(
            build_portal(&p.a, &p.vitex, bad).await.status(),
            StatusCode::BAD_REQUEST,
            "{bad:?}"
        );
    }
    assert_eq!(
        build_portal(&p.b, &p.vitex, "brana-sever").await.status(),
        StatusCode::UNAUTHORIZED
    );
    let placed = build_portal(&p.a, &p.vitex, "brana-sever").await;
    assert_eq!(placed.status(), StatusCode::CREATED);
    let placed = response_json(placed).await;
    let id = placed["id"].as_str().unwrap().to_owned();
    assert_eq!(placed["state"], "building");
    assert!(placed["site"].is_array() && placed["peer_host"].is_null());
    assert_eq!(placed["requirements"].as_array().unwrap().len(), 3);
    assert_eq!(placed["requirements"][0]["required"], "10");
    // A second portal on the same spot is too close; the name is taken in this world.
    assert_eq!(
        build_portal(&p.a, &p.vitex, "brana-jih").await.status(),
        StatusCode::CONFLICT,
        "too close"
    );
    sqlx::query("UPDATE players SET position_x = position_x + 100 WHERE username = 'vitex'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        build_portal(&p.a, &p.vitex, "brana-sever").await.status(),
        StatusCode::CONFLICT,
        "name in use"
    );
    sqlx::query("UPDATE players SET position_x = position_x - 100 WHERE username = 'vitex'")
        .execute(&pool)
        .await
        .unwrap();
    let portal: (String, Option<String>, String) =
        sqlx::query_as("SELECT name, peer, state FROM portals WHERE world_id = $1")
            .bind(p.a.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(portal, ("brana-sever".into(), None, "building".into()));
    let listed =
        response_json(player_request(&p.a.router, "GET", "/portals/mine", "", &p.vitex).await)
            .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(
        player_request(&p.a.router, "GET", "/portals/mine", "", &p.anna)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Deliveries are checked: item, amount, ownership and reach.
    give(&pool, "vitex", &[("stone_block", 4), ("apple", 3)]).await;
    for (item, quantity, status) in [
        ("apple", "1", StatusCode::BAD_REQUEST),
        ("stone_block", "0", StatusCode::BAD_REQUEST),
        ("stone_block", "x", StatusCode::BAD_REQUEST),
        ("stone_block", "5", StatusCode::CONFLICT),
        ("stone_block", "11", StatusCode::CONFLICT),
        ("quartz_crystal", "1", StatusCode::CONFLICT),
    ] {
        assert_eq!(
            deliver(&p.a, &p.vitex, &id, item, quantity).await.status(),
            status,
            "{item} x {quantity}"
        );
    }
    sqlx::query("UPDATE players SET position_x = position_x + 100 WHERE username = 'vitex'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        deliver(&p.a, &p.vitex, &id, "stone_block", "1")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE players SET position_x = position_x - 100 WHERE username = 'vitex'")
        .execute(&pool)
        .await
        .unwrap();
    let partial = response_json(deliver(&p.a, &p.vitex, &id, "stone_block", "4").await).await;
    assert_eq!(partial["requirements"][0]["contributed"], "4");
    assert_eq!(partial["state"], "building");
    let left: i64 = sqlx::query_scalar("SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory WHERE item_id = 'stone_block'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(left, 0, "delivered items leave the inventory");

    // The last delivery finishes the portal; it then takes no more.
    give(
        &pool,
        "vitex",
        &[
            ("stone_block", 6),
            ("pine_plank", 12),
            ("oak_plank", 8),
            ("quartz_crystal", 2),
        ],
    )
    .await;
    for (item, quantity) in [
        ("stone_block", "6"),
        ("pine_plank", "12"),
        ("oak_plank", "8"),
        ("quartz_crystal", "2"),
    ] {
        assert_eq!(
            deliver(&p.a, &p.vitex, &id, item, quantity).await.status(),
            StatusCode::OK
        );
    }
    let state: (String, bool) = sqlx::query_as(
        "SELECT state, local_built_at IS NOT NULL FROM portal_pacts WHERE id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, ("built".into(), true));
    assert_eq!(
        deliver(&p.a, &p.vitex, &id, "stone_block", "1")
            .await
            .status(),
        StatusCode::CONFLICT,
        "a finished portal takes no more"
    );
    assert_eq!(
        player_request(
            &p.a.router,
            "GET",
            &format!("/portals/mine/{id}"),
            "",
            &p.vitex
        )
        .await
        .status(),
        StatusCode::OK
    );
    let rendered = response_json(request(&p.a.router, "/world/portals?x=1&y=1&z=1").await).await;
    assert!(rendered.is_array());

    // A finished portal that is closed stays as a ruin with the record of its materials.
    assert_eq!(
        player_request(
            &p.a.router,
            "DELETE",
            &format!("/portals/mine/{id}"),
            "",
            &p.vitex
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        player_request(
            &p.a.router,
            "DELETE",
            &format!("/portals/mine/{id}"),
            "",
            &p.vitex
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let ruin: (String, i64) = sqlx::query_as("SELECT (SELECT state FROM portals WHERE world_id = $1), (SELECT sum(quantity) FROM portal_contributions)::bigint")
        .bind(p.a.id).fetch_one(&pool).await.unwrap();
    assert_eq!(ruin, ("closed".into(), 32));
}
