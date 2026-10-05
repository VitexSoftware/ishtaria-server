use super::pacts::{accept, invitation_code, world, Network, World};
use super::*;
use std::sync::Arc;

pub(super) struct Pair {
    pub(super) network: Arc<Network>,
    pub(super) a: World,
    pub(super) b: World,
    pub(super) vitex: String,
    pub(super) anna: String,
    pub(super) pact_a: String,
    pub(super) pact_b: String,
}

pub(super) async fn pact_between_worlds(pool: &PgPool, portal_a: &str, portal_b: &str) -> Pair {
    let network = Arc::new(Network::default());
    let a = world(pool, &network, "svet-a", "approve").await;
    let b = world(pool, &network, "svet-b", "approve").await;
    let vitex = create_session(&a.router, "vitex").await;
    let anna = create_session(&b.router, "anna").await;
    let (code, _) = invitation_code(&a, &vitex, portal_a).await;
    let response = accept(&b, &anna, &code, portal_b).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let pact_b = response_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pacts =
        response_json(player_request(&a.router, "GET", "/portals/pacts", "", &vitex).await).await;
    let pact_a = pacts[0]["id"].as_str().unwrap().to_owned();
    // The operators approve both pacts.
    sqlx::query("UPDATE portal_pacts SET state = 'accepted' WHERE state = 'proposed'")
        .execute(pool)
        .await
        .unwrap();
    Pair {
        network,
        a,
        b,
        vitex,
        anna,
        pact_a,
        pact_b,
    }
}

async fn give(pool: &PgPool, username: &str, items: &[(&str, i64)]) {
    for (item, quantity) in items {
        sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) SELECT id, $2, $3 FROM players WHERE username = $1 ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = EXCLUDED.quantity")
            .bind(username).bind(item).bind(quantity).execute(pool).await.unwrap();
    }
}

pub(super) async fn site(world: &World, token: &str, pact: &str) -> Response {
    player_request(
        &world.router,
        "POST",
        &format!("/portals/pacts/{pact}/site"),
        "",
        token,
    )
    .await
}

async fn deliver(world: &World, token: &str, pact: &str, item: &str, quantity: &str) -> Response {
    player_request(
        &world.router,
        "POST",
        &format!("/portals/pacts/{pact}/contribute"),
        &serde_json::json!({ "item_id": item, "quantity": quantity }).to_string(),
        token,
    )
    .await
}

async fn pact_state(pool: &PgPool, id: &str) -> (String, bool, bool, Option<String>) {
    sqlx::query_as("SELECT state, local_built_at IS NOT NULL, peer_built_at IS NOT NULL, notify_pending FROM portal_pacts WHERE id = $1::uuid")
        .bind(id).fetch_one(pool).await.unwrap()
}

async fn build_end(pool: &PgPool, world: &World, username: &str, token: &str, pact: &str) {
    assert_eq!(site(world, token, pact).await.status(), StatusCode::OK);
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
            deliver(world, token, pact, item, quantity).await.status(),
            StatusCode::OK,
            "{item}"
        );
    }
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn both_ends_built_open_the_portal(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = pact_between_worlds(&pool, "brana-sever", "brana-jih").await;

    // Only an accepted pact can get a site, and only on dry land, once.
    sqlx::query("UPDATE portal_pacts SET state = 'proposed' WHERE id = $1::uuid")
        .bind(&p.pact_a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        site(&p.a, &p.vitex, &p.pact_a).await.status(),
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE portal_pacts SET state = 'accepted' WHERE id = $1::uuid")
        .bind(&p.pact_a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        site(&p.b, &p.vitex, &p.pact_a).await.status(),
        StatusCode::UNAUTHORIZED,
        "a session of world A is not valid in world B"
    );
    assert_eq!(
        site(&p.b, &p.anna, &p.pact_a).await.status(),
        StatusCode::NOT_FOUND,
        "another world's pact"
    );
    assert_eq!(
        site(&p.a, &p.anna, &p.pact_a).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let placed = response_json(site(&p.a, &p.vitex, &p.pact_a).await).await;
    assert_eq!(placed["state"], "building");
    assert!(placed["site"].is_array());
    assert_eq!(placed["requirements"].as_array().unwrap().len(), 3);
    assert_eq!(placed["requirements"][0]["required"], "10");
    assert_eq!(placed["requirements"][0]["contributed"], "0");
    assert_eq!(
        site(&p.a, &p.vitex, &p.pact_a).await.status(),
        StatusCode::CONFLICT,
        "one site per pact"
    );
    let portal: (String, String, String) =
        sqlx::query_as("SELECT name, peer, state FROM portals WHERE world_id = $1")
            .bind(p.a.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        portal,
        (
            "brana-sever".into(),
            "svet-b.example.org".into(),
            "building".into()
        )
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
            deliver(&p.a, &p.vitex, &p.pact_a, item, quantity)
                .await
                .status(),
            status,
            "{item} x {quantity}"
        );
    }
    sqlx::query("UPDATE players SET position_x = position_x + 100 WHERE username = 'vitex'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        deliver(&p.a, &p.vitex, &p.pact_a, "stone_block", "1")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE players SET position_x = position_x - 100 WHERE username = 'vitex'")
        .execute(&pool)
        .await
        .unwrap();
    let partial = response_json(deliver(&p.a, &p.vitex, &p.pact_a, "stone_block", "4").await).await;
    assert_eq!(partial["requirements"][0]["contributed"], "4");
    let left: i64 = sqlx::query_scalar("SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory WHERE item_id = 'stone_block'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(left, 0, "delivered items leave the inventory");

    // Finishing one end does not open the portal; the other world is told.
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
            deliver(&p.a, &p.vitex, &p.pact_a, item, quantity)
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        pact_state(&pool, &p.pact_a).await,
        ("building".into(), true, false, None)
    );
    assert_eq!(
        pact_state(&pool, &p.pact_b).await,
        ("accepted".into(), false, true, None),
        "world B knows that A is done"
    );
    assert_eq!(
        deliver(&p.a, &p.vitex, &p.pact_a, "stone_block", "1")
            .await
            .status(),
        StatusCode::CONFLICT,
        "a finished end takes no more"
    );

    // The second world builds its end, and both pacts and portals open.
    build_end(&pool, &p.b, "anna", &p.anna, &p.pact_b).await;
    assert_eq!(
        pact_state(&pool, &p.pact_b).await,
        ("open".into(), true, true, None)
    );
    assert_eq!(
        pact_state(&pool, &p.pact_a).await,
        ("open".into(), true, true, None)
    );
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM portals ORDER BY world_id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(states, ["open", "open"]);
    let spent: i64 = sqlx::query_scalar("SELECT sum(quantity)::bigint FROM portal_contributions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        spent,
        2 * (10 + 20 + 2),
        "the materials are recorded for each world"
    );

    let details = response_json(
        player_request(
            &p.a.router,
            "GET",
            &format!("/portals/pacts/{}", p.pact_a),
            "",
            &p.vitex,
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            details["state"].as_str(),
            details["local_built"].as_bool(),
            details["peer_built"].as_bool()
        ),
        (Some("open"), Some(true), Some(true))
    );
    let position = details["site"].as_array().unwrap();
    let near = format!(
        "/world/portals?x={}&y={}&z={}",
        position[0], position[1], position[2]
    );
    let listed = response_json(request(&p.a.router, &near).await).await;
    assert_eq!(listed[0]["name"], "brana-sever");
    assert_eq!(listed[0]["state"], "open");
    assert_eq!(listed[0]["peer"], "svet-b.example.org");
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let far = response_json(request(&p.a.router, "/world/portals?x=0&y=0&z=6371000").await).await;
    assert!(far.as_array().unwrap().is_empty());
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn status_messages_are_repeated_until_delivered(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = pact_between_worlds(&pool, "brana-sever", "brana-jih").await;
    p.network.reachable.lock().unwrap().remove(&p.b.url);
    build_end(&pool, &p.a, "vitex", &p.vitex, &p.pact_a).await;
    assert_eq!(
        pact_state(&pool, &p.pact_a).await,
        ("building".into(), true, false, Some("built".into())),
        "the message waits"
    );
    assert!(!pact_state(&pool, &p.pact_b).await.2);
    let state_a = AppState {
        pool: pool.clone(),
        world_id: p.a.id,
    };
    let directory: Arc<dyn crate::federation::PeerDirectory> = p.network.clone();
    crate::portals::retry_pending(&state_a, directory.as_ref()).await;
    assert_eq!(
        pact_state(&pool, &p.pact_a).await.3,
        Some("built".into()),
        "still unreachable"
    );
    p.network.reachable.lock().unwrap().insert(p.b.url.clone());
    crate::portals::retry_pending(&state_a, directory.as_ref()).await;
    assert_eq!(pact_state(&pool, &p.pact_a).await.3, None, "delivered");
    assert!(pact_state(&pool, &p.pact_b).await.2, "world B now knows");
    // Delivery is idempotent: a repeated message changes nothing.
    sqlx::query("UPDATE portal_pacts SET notify_pending = 'built' WHERE id = $1::uuid")
        .bind(&p.pact_a)
        .execute(&pool)
        .await
        .unwrap();
    crate::portals::retry_pending(&state_a, directory.as_ref()).await;
    assert_eq!(
        pact_state(&pool, &p.pact_b).await,
        ("accepted".into(), false, true, None)
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn closing_a_pact_leaves_ruins_on_both_worlds(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = pact_between_worlds(&pool, "brana-sever", "brana-jih").await;
    build_end(&pool, &p.a, "vitex", &p.vitex, &p.pact_a).await;
    build_end(&pool, &p.b, "anna", &p.anna, &p.pact_b).await;
    assert_eq!(pact_state(&pool, &p.pact_a).await.0, "open");
    let path = format!("/portals/pacts/{}", p.pact_a);
    assert_eq!(
        player_request(&p.a.router, "DELETE", &path, "", &p.anna)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        player_request(&p.a.router, "DELETE", &path, "", &p.vitex)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        pact_state(&pool, &p.pact_a).await,
        ("closed".into(), true, true, None)
    );
    assert_eq!(
        pact_state(&pool, &p.pact_b).await.0,
        "closed",
        "the other world is told"
    );
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM portals ORDER BY world_id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        states,
        ["closed", "closed"],
        "the portals stay as inactive ruins"
    );
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_contributions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        kept, 8,
        "the delivered materials stay on record for mining the ruins"
    );
    assert_eq!(
        player_request(&p.a.router, "DELETE", &path, "", &p.vitex)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        deliver(&p.a, &p.vitex, &p.pact_a, "stone_block", "1")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let listed = response_json(request(&p.b.router, "/world/portals?x=0&y=0&z=0").await).await;
    assert!(listed.as_array().unwrap().is_empty() || listed[0]["state"] == "closed");
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn forged_or_misdirected_status_is_refused(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = pact_between_worlds(&pool, "brana-sever", "brana-jih").await;
    let post = |world: &World, message: String| {
        let router = world.router.clone();
        async move {
            player_request(
                &router,
                "POST",
                "/federation/pacts/status",
                &serde_json::json!({ "message": message }).to_string(),
                "",
            )
            .await
            .status()
        }
    };
    for garbage in ["x", "a.b", "a.b.c", ""] {
        assert_eq!(
            post(&p.b, garbage.to_owned()).await,
            StatusCode::BAD_REQUEST,
            "{garbage:?}"
        );
    }
    // A message made with the key of a world that is not the pact's peer is refused.
    let intruder = SigningKeyForTest::message(
        "svet-a.example.org",
        &p.a.url,
        "svet-b.example.org",
        &invitation_of(&pool, &p.pact_b).await,
        "built",
    );
    assert!(matches!(
        post(&p.b, intruder).await,
        StatusCode::UNAUTHORIZED | StatusCode::CONFLICT
    ));
    assert!(
        !pact_state(&pool, &p.pact_b).await.2,
        "nothing was recorded"
    );
}

async fn invitation_of(pool: &PgPool, pact: &str) -> String {
    sqlx::query_scalar("SELECT invitation_id::text FROM portal_pacts WHERE id = $1::uuid")
        .bind(pact)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Builds status messages signed with a throwaway key, for forgery checks.
struct SigningKeyForTest;

impl SigningKeyForTest {
    fn message(from: &str, from_url: &str, to: &str, invitation: &str, status: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
        use ed25519_dalek::{Signer, SigningKey};
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let payload = serde_json::json!({
            "v": 1, "type": "portal-pact-status", "invitation": invitation,
            "from_world": from, "from_api_url": from_url, "to_world": to,
            "status": status, "issued": now, "expires": now + 300,
        })
        .to_string();
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let mut message = b"ishtaria/portal-pact-status/v1\0".to_vec();
        message.extend_from_slice(payload.as_bytes());
        format!(
            "{}.{}",
            B64.encode(payload),
            B64.encode(key.sign(&message).to_bytes())
        )
    }
}
