use super::*;

pub(super) async fn position_of(pool: &PgPool, player_id: i64) -> [f64; 3] {
    let (x, y, z): (f64, f64, f64) =
        sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(pool)
            .await
            .unwrap();
    [x, y, z]
}

pub(super) async fn teleport(pool: &PgPool, player_id: i64, point: [f64; 3]) {
    sqlx::query("UPDATE players SET position_x = $2, position_y = $3, position_z = $4, last_gathered_at = NULL, stamina = 100 WHERE id = $1")
        .bind(player_id).bind(point[0]).bind(point[1]).bind(point[2])
        .execute(pool).await.unwrap();
}

pub(super) async fn harvest_request(router: &Router, token: &str, object_id: &str) -> Response {
    player_request(
        router,
        "POST",
        "/players/me/harvest",
        &serde_json::json!({ "object_id": object_id }).to_string(),
        token,
    )
    .await
}

pub(super) async fn cooldown_reset(pool: &PgPool) {
    sqlx::query("UPDATE players SET last_gathered_at = NULL")
        .execute(pool)
        .await
        .unwrap();
}

pub(super) fn find_object(
    terrain: &movement::WalkingTerrain,
    around: [f64; 3],
    kind: &str,
) -> movement::WorldObject {
    terrain
        .objects(around, 16)
        .into_iter()
        .find(|object| {
            crate::gathering::resource_kind(&object.model) == Some(kind)
                && object.collision_radius_m > 0.0
        })
        .unwrap_or_else(|| panic!("no {kind} near the spawn point"))
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn trees_and_rocks_are_harvested_by_the_server(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "lumberjack").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let spawn = position_of(&pool, player_id).await;
    let terrain = movement::terrain(&state).await.unwrap();
    let tree = find_object(&terrain, spawn, "tree");
    let rock = find_object(&terrain, spawn, "rock");
    let resource = crate::gathering::resource_info(&tree.model).unwrap();

    // Out of reach, unknown and malformed objects are refused without side effects.
    let far = [spawn[0] + 40.0, spawn[1], spawn[2]];
    teleport(&pool, player_id, far).await;
    if (0..3)
        .map(|axis| (far[axis] - tree.position[axis]).powi(2))
        .sum::<f64>()
        .sqrt()
        > 20.0
    {
        assert_eq!(
            harvest_request(&router, &token, &tree.id).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    teleport(&pool, player_id, tree.position).await;
    for bad in ["", "nonsense", &"1:".repeat(80), "42:0:1:1"] {
        let status = harvest_request(&router, &token, bad).await.status();
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{bad:?}: {status}"
        );
    }
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/me/harvest",
            r#"{"object_id":"x","tool":"axe"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the client cannot choose the tool"
    );

    // Several hits fell the tree; the cooldown is enforced between hits.
    let first = response_json(harvest_request(&router, &token, &tree.id).await).await;
    assert_eq!(first["state"], "hit");
    assert_eq!(first["tool"], "axe");
    assert_eq!(first["hits"], 1);
    assert_eq!(first["hits_required"], resource.hits);
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let mut felled = serde_json::Value::Null;
    for _ in 1..resource.hits {
        cooldown_reset(&pool).await;
        felled = response_json(harvest_request(&router, &token, &tree.id).await).await;
    }
    assert_eq!(felled["state"], "depleted");
    let items = felled["items"].as_array().unwrap();
    assert!(items.iter().any(|item| item["item_id"] == resource.log));
    let logs: i64 = sqlx::query_scalar("SELECT quantity FROM player_inventory WHERE item_id = $1")
        .bind(&resource.log)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        (1..=3).contains(&logs),
        "a felled tree gives one to three logs"
    );
    let stamina: i32 = sqlx::query_scalar("SELECT stamina FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stamina, 100 - 2 * resource.hits, "every hit costs stamina");

    // The tree is gone from the world and cannot be harvested again until it grows back.
    assert!(terrain.object_by_id(&tree.id).is_none());
    let remains = terrain
        .objects(spawn, 16)
        .into_iter()
        .find(|object| object.id == tree.id)
        .expect("a felled tree leaves a stump");
    assert_eq!(remains.model, "nature.stump_round");
    assert!(
        remains.collision_radius_m == 0.0,
        "the stump can be walked over"
    );
    assert!(remains.scale_m < tree.scale_m);
    cooldown_reset(&pool).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::NOT_FOUND
    );
    let (hits, depleted): (i32, bool) = sqlx::query_as(
        "SELECT hits, depleted_until > now() FROM world_object_state WHERE object_id = $1",
    )
    .bind(&tree.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((hits, depleted), (0, true), "the change is persisted");
    terrain.mark_removed(&tree.id, 0, None);
    assert!(terrain.object_by_id(&tree.id).is_some(), "it grows back");
    sqlx::query("UPDATE world_object_state SET depleted_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    cooldown_reset(&pool).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::OK,
        "a grown-back tree can be harvested again"
    );

    // Rocks need the pickaxe in hand; a character starts with the axe.
    teleport(&pool, player_id, rock.position).await;
    assert_eq!(
        harvest_request(&router, &token, &rock.id).await.status(),
        StatusCode::CONFLICT,
        "the axe does not work on stone"
    );
    cooldown_reset(&pool).await;
    let profile = response_json(equip_request(&router, &token, "pickaxe").await).await;
    assert_eq!(profile["equipment"]["hand"], "pickaxe");
    // Rocks yield stone and sometimes rare minerals.
    teleport(&pool, player_id, rock.position).await;
    let rock_resource = crate::gathering::resource_info(&rock.model).unwrap();
    let mut mined = serde_json::Value::Null;
    for _ in 0..rock_resource.hits {
        cooldown_reset(&pool).await;
        mined = response_json(harvest_request(&router, &token, &rock.id).await).await;
    }
    assert_eq!(mined["kind"], "rock");
    assert_eq!(mined["tool"], "pickaxe");
    assert!(mined["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["item_id"] == "stone"));

    // A full inventory refuses the final hit and leaves the object standing.
    let other = find_object(&terrain, spawn, "tree");
    let other = terrain
        .objects(spawn, 16)
        .into_iter()
        .find(|object| {
            crate::gathering::resource_kind(&object.model) == Some("tree")
                && object.id != other.id
                && object.id != tree.id
        })
        .unwrap_or(other);
    teleport(&pool, player_id, other.position).await;
    let other_resource = crate::gathering::resource_info(&other.model).unwrap();
    sqlx::query("INSERT INTO item_types (id, name, stack_limit) SELECT 'filler-' || value, 'Filler', 1 FROM generate_series(1,200) AS value")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO player_inventory SELECT $1, id, 1 FROM item_types WHERE id LIKE 'filler-%'",
    )
    .bind(player_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO world_object_state (world_id, object_id, hits) VALUES ($1, $2, $3)")
        .bind(world_id)
        .bind(&other.id)
        .bind(other_resource.hits * 2 - 1)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        harvest_request(&router, &token, &other.id).await.status(),
        StatusCode::CONFLICT
    );
    assert!(terrain.object_by_id(&other.id).is_some());
    let (hits, depleted): (i32, bool) = sqlx::query_as("SELECT hits, coalesce(depleted_until > now(), false) FROM world_object_state WHERE object_id = $1")
        .bind(&other.id).fetch_one(&pool).await.unwrap();
    assert_eq!((hits, depleted), (other_resource.hits * 2 - 1, false));

    // An exhausted character cannot swing.
    sqlx::query("DELETE FROM player_inventory WHERE item_id LIKE 'filler-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET stamina = 1, last_gathered_at = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        harvest_request(&router, &token, &other.id).await.status(),
        StatusCode::CONFLICT
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn crafting_consumes_ingredients_atomically(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "carpenter").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
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
    assert_eq!(
        craft("chop_oak_log", "1").await.status(),
        StatusCode::CONFLICT,
        "no logs yet"
    );
    sqlx::query(
        "INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, 'oak_log', 3)",
    )
    .bind(player_id)
    .execute(&pool)
    .await
    .unwrap();
    // Each log fills ten slots until it is chopped.
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert_eq!(
        profile["inventory"]["used"],
        8 + 30,
        "four foods, the coins and three tools, plus three logs of ten slots"
    );
    for (recipe, count, status) in [
        ("chop_oak_log", "0", StatusCode::BAD_REQUEST),
        ("chop_oak_log", "-1", StatusCode::BAD_REQUEST),
        ("chop_oak_log", "101", StatusCode::BAD_REQUEST),
        ("chop_oak_log", "x", StatusCode::BAD_REQUEST),
        ("missing", "1", StatusCode::NOT_FOUND),
        ("chop_oak_log", "4", StatusCode::CONFLICT),
        ("oak_plank", "1", StatusCode::CONFLICT),
    ] {
        assert_eq!(
            craft(recipe, count).await.status(),
            status,
            "{recipe} x {count}"
        );
    }
    let logs: i64 =
        sqlx::query_scalar("SELECT quantity FROM player_inventory WHERE item_id = 'oak_log'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(logs, 3, "failed crafting consumes nothing");
    let profile = response_json(craft("chop_oak_log", "2").await).await;
    assert_eq!(
        profile["stats"]["gold"], "100",
        "crafting leaves the coins alone"
    );
    assert_eq!(
        profile["inventory"]["used"],
        8 + 10 + 1,
        "one log left and the wood shares one slot"
    );
    let (logs, wood): (i64, i64) = sqlx::query_as("SELECT (SELECT quantity FROM player_inventory WHERE item_id = 'oak_log'), (SELECT quantity FROM player_inventory WHERE item_id = 'oak_wood')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!((logs, wood), (1, 40));
    let profile = response_json(craft("oak_plank", "4").await).await;
    let (wood, planks): (i64, i64) = sqlx::query_as("SELECT (SELECT quantity FROM player_inventory WHERE item_id = 'oak_wood'), (SELECT quantity FROM player_inventory WHERE item_id = 'oak_plank')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!((wood, planks), (20, 8));
    assert_eq!(profile["inventory"]["used"], 8 + 10 + 2);
    // The last ingredients are used up completely.
    assert_eq!(craft("oak_plank", "4").await.status(), StatusCode::OK);
    let wood: Option<i64> =
        sqlx::query_scalar("SELECT quantity FROM player_inventory WHERE item_id = 'oak_wood'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(wood.is_none());
    let recipes = response_json(request(&router, "/recipes").await).await;
    assert!(recipes
        .as_array()
        .unwrap()
        .iter()
        .any(|recipe| recipe["id"] == "stone_block" && recipe["inputs"][0]["name"] == "Stone"));
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn configured_items_exist(pool: PgPool) {
    initialize(&pool, &config(), None).await.unwrap();
    let known: Vec<String> = sqlx::query_scalar("SELECT id FROM item_types")
        .fetch_all(&pool)
        .await
        .unwrap();
    for item in crate::gathering::configured_items() {
        assert!(
            known.contains(&item),
            "item {item} is configured but not defined"
        );
    }
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn item_catalog_has_categories_and_rpg_items(pool: PgPool) {
    initialize(&pool, &config(), None).await.unwrap();
    let uncategorised: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM item_types WHERE category = 'misc' AND id <> 'snowflake'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        uncategorised, 0,
        "every item except the snowflake has a specific category"
    );
    let categories: Vec<(String, i64)> = sqlx::query_as(
        "SELECT category, count(*) FROM item_types WHERE id IN ('sword', 'sword_2', 'claymore', 'armor_leather', 'shield_round', 'potion_bottle', 'key_4', 'open_book_4', 'skull_2', 'gold_ingots', 'backpack', 'gold', 'axe', 'apple', 'stone', 'bag') GROUP BY category ORDER BY category",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let categories: std::collections::HashMap<_, _> = categories.into_iter().collect();
    assert_eq!(categories["weapon"], 3);
    assert_eq!(categories["container"], 2, "bag and backpack");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM item_types")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        total >= 70,
        "the RPG items are part of the catalog, found {total}"
    );
    // Items that are not stacked fill one slot each, so every sword is carried separately.
    let (limit, multi): (i64, bool) =
        sqlx::query_as("SELECT stack_limit, multi_slot FROM item_types WHERE id = 'claymore'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((limit, multi), (1, true));
}

async fn equip_request(router: &Router, token: &str, item: &str) -> Response {
    player_request(
        router,
        "POST",
        "/players/me/equip",
        &serde_json::json!({ "item_id": item }).to_string(),
        token,
    )
    .await
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn the_tool_in_hand_decides_what_can_be_harvested(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let session = response_json(
        player_request(
            &router,
            "POST",
            "/players",
            r#"{"username":"woodsman","password":"test-password"}"#,
            "",
        )
        .await,
    )
    .await;
    let token = session["token"].as_str().unwrap().to_owned();
    assert_eq!(
        session["player"]["equipment"]["hand"], "axe",
        "a new character holds the axe"
    );
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let spawn = position_of(&pool, player_id).await;
    let terrain = movement::terrain(&state).await.unwrap();
    let tree = find_object(&terrain, spawn, "tree");
    let resource = crate::gathering::resource_info(&tree.model).unwrap();
    teleport(&pool, player_id, tree.position).await;

    // Only tools and weapons that the character owns can be taken in hand.
    for (item, status) in [
        ("apple", StatusCode::CONFLICT),
        ("claymore", StatusCode::CONFLICT),
        ("gold", StatusCode::CONFLICT),
        ("sword", StatusCode::OK),
    ] {
        assert_eq!(
            equip_request(&router, &token, item).await.status(),
            status,
            "{item}"
        );
    }
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/me/equip",
            r#"{"item_id":"sword","x":1}"#,
            &token
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        equip_request(&router, "", "axe").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::CONFLICT,
        "a sword does not fell trees"
    );

    // Nothing in hand: no work either.
    let bare =
        response_json(player_request(&router, "DELETE", "/players/me/equip", "", &token).await)
            .await;
    assert!(bare["equipment"]["hand"].is_null());
    cooldown_reset(&pool).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::CONFLICT
    );

    // The pickaxe fells a tree, but needs twice as many swings as the axe.
    equip_request(&router, &token, "pickaxe").await;
    let mut swings = 0;
    let last = loop {
        cooldown_reset(&pool).await;
        let reply = response_json(harvest_request(&router, &token, &tree.id).await).await;
        swings += 1;
        assert_eq!(reply["tool"], "pickaxe");
        assert_eq!(reply["hits_required"], 2 * resource.hits);
        if reply["state"] == "depleted" {
            break reply;
        }
        assert!(swings < 2 * resource.hits, "the tree should have fallen");
    };
    assert_eq!(
        swings,
        2 * resource.hits,
        "twice the swings of the axe ({} with the axe)",
        resource.hits
    );
    assert!(last["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["item_id"] == resource.log));

    // The axe is the fastest tool for the same job.
    let other = terrain
        .objects(spawn, 16)
        .into_iter()
        .find(|object| {
            crate::gathering::resource_kind(&object.model) == Some("tree") && object.id != tree.id
        })
        .expect("another tree");
    let other_resource = crate::gathering::resource_info(&other.model).unwrap();
    teleport(&pool, player_id, other.position).await;
    equip_request(&router, &token, "axe").await;
    let mut axe_swings = 0;
    loop {
        cooldown_reset(&pool).await;
        let reply = response_json(harvest_request(&router, &token, &other.id).await).await;
        axe_swings += 1;
        if reply["state"] == "depleted" {
            break;
        }
        assert!(axe_swings < other_resource.hits + 1);
    }
    assert_eq!(axe_swings, other_resource.hits);

    // Mixed tools add up: a swing with a weaker tool is worth half.
    sqlx::query("DELETE FROM world_object_state")
        .execute(&pool)
        .await
        .unwrap();
    terrain.mark_removed(&tree.id, 0, None);
    teleport(&pool, player_id, tree.position).await;
    let first = response_json(harvest_request(&router, &token, &tree.id).await).await;
    assert_eq!(first["hits_required"], resource.hits);
    let stored: i32 =
        sqlx::query_scalar("SELECT hits FROM world_object_state WHERE object_id = $1")
            .bind(&tree.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, 2, "an axe swing is worth two work points");

    // Giving the tool away takes it out of hand; dying clears the equipment.
    sqlx::query("DELETE FROM player_inventory WHERE item_id = 'axe'")
        .execute(&pool)
        .await
        .unwrap();
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert!(
        profile["equipment"]["hand"].is_null(),
        "a tool that is no longer owned is not in hand"
    );
    equip_request(&router, &token, "pickaxe").await;
    survival::apply_damage(&state, player_id, 100, survival::DamageCause::Disease)
        .await
        .unwrap();
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM player_equipment")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(held, 0, "a dead character holds nothing");
}
