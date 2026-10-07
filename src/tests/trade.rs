use super::building::give;
use super::harvest::teleport;
use super::*;
use crate::story::world;
use std::{fs, path::Path};

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

/// A datadisk with one merchant (`shop:general`) and one plain talker.
fn install_disk() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ishtaria-trade-it-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let root = dir.join("play");
    write(&root, "datadisk.yaml", "id: play\nversion: 1.0.0\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\n");
    let strings = "place.hut: Hut\nnpc.trader: Trader\nnpc.hermit: Hermit\nhi: Hi\nbye: Bye\n";
    write(&root, "i18n/en.yaml", strings);
    write(&root, "i18n/cs.yaml", strings);
    write(
        &root,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut, spawn: true}\n",
    );
    write(&root, "npcs/n.yaml", "- {id: trader, name_key: npc.trader, place: hut, character: {pack: retro, skin: humanMaleA}, dialogue: talk, tags: [shop, 'shop:general']}\n- {id: hermit, name_key: npc.hermit, place: hut, character: {pack: retro, skin: humanMaleA}, dialogue: talk}\n");
    write(&root, "dialogues/talk.yaml", "id: talk\nstart: hi\nnodes:\n  hi:\n    text_key: hi\n    choices:\n      - {text_key: bye}\n");
    dir
}

async fn merchant_world(pool: &PgPool) -> (Router, AppState, [f64; 3]) {
    let dir = install_disk();
    std::env::set_var("ISHTARIA_DATADISK_DIR", &dir);
    world::reset_cache().await;
    let world_id = initialize_spawn_world(pool).await;
    sqlx::query("INSERT INTO world_datadisks (world_id, disk_id, version, position) VALUES ($1, 'play', '1.0.0', 0)")
        .bind(world_id).execute(pool).await.unwrap();
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let story = world::world(&state).await.unwrap().unwrap();
    let npc = story
        .npcs
        .iter()
        .find(|npc| npc.id.ends_with("trader"))
        .unwrap();
    assert_eq!(npc.shop.as_deref(), Some("general"));
    let at = npc.position;
    (app(state.clone()), state, at)
}

async fn shop(router: &Router, token: &str, path: &str, body: serde_json::Value) -> Response {
    player_request(router, "POST", path, &body.to_string(), token).await
}

async fn stock(pool: &PgPool, username: &str, item: &str) -> i64 {
    sqlx::query_scalar("SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory JOIN players ON players.id = player_id WHERE username = $1 AND item_id = $2")
        .bind(username).bind(item).fetch_one(pool).await.unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn merchants_trade_for_gold_within_reach_and_a_daily_limit(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let (router, _state, at) = merchant_world(&pool).await;
    let token = create_session(&router, "buyer").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let trader = "play:trader";
    let goods = |npc: &str| format!("/shops/{npc}");

    // Far from the merchant nothing can be seen or done.
    teleport(&pool, id, [at[0] + 200.0, at[1], at[2]]).await;
    assert_eq!(
        player_request(&router, "GET", &goods(trader), "", &token)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        shop(
            &router,
            &token,
            &format!("{}/buy", goods(trader)),
            serde_json::json!({"item_id": "apple", "quantity": 1})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    teleport(&pool, id, at).await;
    let listing =
        response_json(player_request(&router, "GET", &goods(trader), "", &token).await).await;
    assert!(listing["sells"]
        .as_array()
        .unwrap()
        .iter()
        .any(|good| good["item_id"] == "apple" && good["price"] == "4"));
    assert_eq!(listing["daily_limit"], "400");
    assert_eq!(
        player_request(&router, "GET", &goods("play:hermit"), "", &token)
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "not a merchant"
    );
    assert_eq!(
        player_request(&router, "GET", &goods("play:nobody"), "", &token)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // Buying costs gold; what cannot be paid for changes nothing.
    let buy = |item: &str, quantity: i64| {
        let (router, token, path) = (
            router.clone(),
            token.clone(),
            format!("{}/buy", goods(trader)),
        );
        let body = serde_json::json!({"item_id": item, "quantity": quantity});
        async move { shop(&router, &token, &path, body).await.status() }
    };
    let apples = stock(&pool, "buyer", "apple").await;
    assert_eq!(buy("apple", 5).await, StatusCode::OK);
    assert_eq!(stock(&pool, "buyer", "gold").await, 80);
    assert_eq!(stock(&pool, "buyer", "apple").await, apples + 5);
    assert_eq!(
        buy("sword", 1).await,
        StatusCode::CONFLICT,
        "90 gold are too much for 80"
    );
    assert_eq!(stock(&pool, "buyer", "gold").await, 80);
    assert_eq!(buy("bone", 1).await, StatusCode::NOT_FOUND, "not for sale");
    for quantity in [0, -3, 1001] {
        assert_eq!(
            buy("apple", quantity).await,
            StatusCode::BAD_REQUEST,
            "{quantity}"
        );
    }
    assert_eq!(
        buy("apple", i64::MAX).await,
        StatusCode::BAD_REQUEST,
        "no overflow"
    );
    assert_eq!(stock(&pool, "buyer", "gold").await, 80);

    // Selling pays less, only what is bought, and only what is owned.
    let sell = |item: &str, quantity: i64| {
        let (router, token, path) = (
            router.clone(),
            token.clone(),
            format!("{}/sell", goods(trader)),
        );
        let body = serde_json::json!({"item_id": item, "quantity": quantity});
        async move { shop(&router, &token, &path, body).await.status() }
    };
    give(&pool, "buyer", &[("hide", 10), ("iron_ingot", 30)]).await;
    assert_eq!(sell("hide", 4).await, StatusCode::OK);
    assert_eq!(
        (
            stock(&pool, "buyer", "hide").await,
            stock(&pool, "buyer", "gold").await
        ),
        (6, 100)
    );
    assert_eq!(
        sell("hide", 7).await,
        StatusCode::CONFLICT,
        "only six are left"
    );
    assert_eq!(
        sell("axe", 1).await,
        StatusCode::NOT_FOUND,
        "not bought here"
    );
    assert_eq!(stock(&pool, "buyer", "hide").await, 6);

    // The merchant pays at most 400 gold a day to one character.
    assert_eq!(
        sell("iron_ingot", 27).await,
        StatusCode::OK,
        "27 x 14 gold brings the day to 398"
    );
    assert_eq!(
        sell("iron_ingot", 1).await,
        StatusCode::CONFLICT,
        "398 + 14 is over the limit"
    );
    assert_eq!(
        stock(&pool, "buyer", "iron_ingot").await,
        3,
        "a refused sale keeps the goods"
    );
    assert_eq!(
        sell("hide", 1).await,
        StatusCode::CONFLICT,
        "398 + 5 is over the limit too"
    );
    give(&pool, "buyer", &[("bone", 1)]).await;
    let before = stock(&pool, "buyer", "gold").await;
    assert_eq!(
        sell("bone", 1).await,
        StatusCode::OK,
        "2 gold bring it to exactly 400"
    );
    assert_eq!(stock(&pool, "buyer", "gold").await, before + 2);
    sqlx::query("UPDATE shop_sales SET window_start = now() - interval '25 hours'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sell("iron_ingot", 3).await,
        StatusCode::OK,
        "a new day, a new limit"
    );

    // The dead do not trade.
    sqlx::query("UPDATE players SET health = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
}

async fn trade(
    router: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> Response {
    player_request(router, method, path, &body.to_string(), token).await
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn players_swap_items_only_when_both_agree(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let anna = create_session(&router, "anna").await;
    let boris = create_session(&router, "boris").await;
    create_session(&router, "cyril").await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM players ORDER BY username")
        .fetch_all(&pool)
        .await
        .unwrap();
    let spot = [6_371_000.0, 0.0, 0.0];
    for id in &ids {
        teleport(&pool, *id, spot).await;
    }
    teleport(&pool, ids[2], [6_371_000.0, 500.0, 0.0]).await;
    give(&pool, "anna", &[("hide", 5), ("gold", 100)]).await;
    give(&pool, "boris", &[("iron_ingot", 3), ("gold", 100)]).await;
    let open = |token: &str, with: &str| {
        let (router, token) = (router.clone(), token.to_owned());
        let body = serde_json::json!({"with": with});
        async move {
            trade(&router, &token, "POST", "/trades", body)
                .await
                .status()
        }
    };
    assert_eq!(
        open(&anna, "cyril").await,
        StatusCode::FORBIDDEN,
        "too far away"
    );
    assert_eq!(open(&anna, "anna").await, StatusCode::BAD_REQUEST);
    assert_eq!(open(&anna, "nobody").await, StatusCode::NOT_FOUND);
    assert_eq!(open(&anna, "boris").await, StatusCode::CREATED);
    assert_eq!(
        open(&anna, "boris").await,
        StatusCode::CONFLICT,
        "one exchange at a time"
    );
    assert_eq!(open(&boris, "anna").await, StatusCode::CONFLICT);
    // The partner is told once, through the events.
    let events =
        response_json(player_request(&router, "GET", "/events?after=0", "", &boris).await).await;
    assert_eq!(events["events"][0]["kind"], "trade");
    assert_eq!(events["events"][0]["subject"], "anna");

    let put = |token: &str, items: serde_json::Value| {
        let (router, token) = (router.clone(), token.to_owned());
        async move {
            trade(
                &router,
                &token,
                "PUT",
                "/trades/current/offer",
                serde_json::json!({"items": items}),
            )
            .await
        }
    };
    assert_eq!(
        put(
            &anna,
            serde_json::json!([{"item_id": "hide", "quantity": 6}])
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "more than owned"
    );
    assert_eq!(
        put(
            &anna,
            serde_json::json!([{"item_id": "hide", "quantity": 0}])
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(put(&anna, serde_json::json!([{"item_id": "hide", "quantity": 1}, {"item_id": "hide", "quantity": 1}])).await.status(), StatusCode::BAD_REQUEST, "twice the same item");
    assert_eq!(
        put(
            &anna,
            serde_json::json!([{"item_id": "unicorn", "quantity": 1}])
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(put(&anna, serde_json::json!([{"item_id": "hide", "quantity": 5}, {"item_id": "gold", "quantity": 30}])).await.status(), StatusCode::OK);
    let theirs = response_json(
        put(
            &boris,
            serde_json::json!([{"item_id": "iron_ingot", "quantity": 3}]),
        )
        .await,
    )
    .await;
    assert_eq!(theirs["with"], "anna");
    assert_eq!(theirs["theirs"].as_array().unwrap().len(), 2);

    // One side accepting changes nothing; changing an offer withdraws it.
    let accept = |token: &str| {
        let (router, token) = (router.clone(), token.to_owned());
        async move {
            response_json(
                trade(
                    &router,
                    &token,
                    "POST",
                    "/trades/current/accept",
                    serde_json::json!({}),
                )
                .await,
            )
            .await
        }
    };
    let waiting = accept(&anna).await;
    assert_eq!(waiting["done"], false);
    assert_eq!(waiting["trade"]["i_accepted"], true);
    assert_eq!(waiting["trade"]["they_accepted"], false);
    assert_eq!(stock(&pool, "anna", "hide").await, 5);
    let changed = response_json(
        put(
            &boris,
            serde_json::json!([{"item_id": "iron_ingot", "quantity": 2}]),
        )
        .await,
    )
    .await;
    assert_eq!(
        changed["they_accepted"], false,
        "the offer changed, acceptance is withdrawn"
    );
    let mine =
        response_json(player_request(&router, "GET", "/trades/current", "", &anna).await).await;
    assert_eq!(mine["i_accepted"], false);

    // Both accept: the swap happens at once, and the exchange is gone.
    accept(&anna).await;
    let done = accept(&boris).await;
    assert_eq!(done["done"], true);
    assert_eq!(
        (
            stock(&pool, "anna", "hide").await,
            stock(&pool, "anna", "iron_ingot").await,
            stock(&pool, "anna", "gold").await
        ),
        (0, 2, 70)
    );
    assert_eq!(
        (
            stock(&pool, "boris", "hide").await,
            stock(&pool, "boris", "iron_ingot").await,
            stock(&pool, "boris", "gold").await
        ),
        (5, 1, 130)
    );
    assert_eq!(
        player_request(&router, "GET", "/trades/current", "", &anna)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM trades) + (SELECT count(*) FROM trade_offers)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0, "nothing of an exchange is kept");

    // Items that changed hands before the swap, a full inventory and a worn piece all stop it.
    assert_eq!(open(&anna, "boris").await, StatusCode::CREATED);
    put(
        &anna,
        serde_json::json!([{"item_id": "iron_ingot", "quantity": 2}]),
    )
    .await;
    put(
        &boris,
        serde_json::json!([{"item_id": "hide", "quantity": 5}]),
    )
    .await;
    accept(&anna).await;
    give(&pool, "anna", &[("iron_ingot", 1)]).await;
    let refused = trade(
        &router,
        &boris,
        "POST",
        "/trades/current/accept",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        refused.status(),
        StatusCode::CONFLICT,
        "anna no longer has two"
    );
    assert_eq!(
        (
            stock(&pool, "anna", "iron_ingot").await,
            stock(&pool, "boris", "hide").await
        ),
        (1, 5),
        "all or nothing"
    );
    let state =
        response_json(player_request(&router, "GET", "/trades/current", "", &anna).await).await;
    assert_eq!(
        (
            state["i_accepted"].as_bool(),
            state["they_accepted"].as_bool()
        ),
        (Some(false), Some(false)),
        "both must accept again"
    );
    sqlx::query("UPDATE player_inventory SET durability = 5 WHERE item_id = 'axe'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        put(
            &anna,
            serde_json::json!([{"item_id": "axe", "quantity": 1}])
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "a worn item cannot be traded"
    );

    // Cancelling ends it for both; an exchange nobody finishes expires.
    assert_eq!(
        trade(
            &router,
            &boris,
            "DELETE",
            "/trades/current",
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        player_request(&router, "GET", "/trades/current", "", &anna)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(open(&anna, "boris").await, StatusCode::CREATED);
    sqlx::query("UPDATE trades SET created_at = now() - interval '6 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        player_request(&router, "GET", "/trades/current", "", &anna)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        open(&boris, "anna").await,
        StatusCode::CREATED,
        "after expiry a new exchange can start"
    );

    // A full inventory refuses the swap and keeps everything where it was.
    sqlx::query("DELETE FROM trades")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(open(&anna, "boris").await, StatusCode::CREATED);
    put(
        &boris,
        serde_json::json!([{"item_id": "hide", "quantity": 5}]),
    )
    .await;
    accept(&boris).await;
    // A million coins fill a hundred slots: anna cannot carry one more stack.
    give(&pool, "anna", &[("gold", 1_000_000)]).await;
    let full = trade(
        &router,
        &anna,
        "POST",
        "/trades/current/accept",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(full.status(), StatusCode::CONFLICT);
    assert_eq!(stock(&pool, "boris", "hide").await, 5);
}
