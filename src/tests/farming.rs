use super::building::give;
use super::harvest::{position_of, teleport};
use super::*;
use crate::movement::WaterNear;

async fn post(router: &Router, token: &str, path: &str, body: serde_json::Value) -> Response {
    player_request(router, "POST", path, &body.to_string(), token).await
}

async fn stock(pool: &PgPool, username: &str, item: &str) -> i64 {
    sqlx::query_scalar("SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory JOIN players ON players.id = player_id WHERE username = $1 AND item_id = $2")
        .bind(username).bind(item).fetch_one(pool).await.unwrap()
}

/// A point `metres` away from `from` along the surface.
fn beside(from: [f64; 3], metres: f64) -> [f64; 3] {
    let up = from.map(|value| value / (from.iter().map(|v| v * v).sum::<f64>()).sqrt());
    let helper = if up[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let east = [
        up[1] * helper[2] - up[2] * helper[1],
        up[2] * helper[0] - up[0] * helper[2],
        up[0] * helper[1] - up[1] * helper[0],
    ];
    let length = east.iter().map(|v| v * v).sum::<f64>().sqrt();
    std::array::from_fn(|axis| from[axis] + east[axis] / length * metres)
}

fn target(item: &str, at: [f64; 3]) -> serde_json::Value {
    serde_json::json!({"item_id": item, "x": at[0], "y": at[1], "z": at[2]})
}

async fn listed(router: &Router, around: [f64; 3]) -> Vec<serde_json::Value> {
    let path = format!(
        "/world/placed?x={}&y={}&z={}",
        around[0], around[1], around[2]
    );
    response_json(player_request(router, "GET", &path, "", "").await)
        .await
        .as_array()
        .unwrap()
        .clone()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn things_are_placed_used_as_stations_and_picked_up(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let owner = create_session(&router, "builder").await;
    let guest = create_session(&router, "guest").await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM players ORDER BY username")
        .fetch_all(&pool)
        .await
        .unwrap();
    let home = position_of(&pool, ids[0]).await;
    teleport(&pool, ids[0], home).await;
    teleport(&pool, ids[1], beside(home, 1.0)).await;
    give(
        &pool,
        "builder",
        &[("campfire", 3), ("sword", 1), ("raw_meat", 4), ("hide", 2)],
    )
    .await;

    let spot = beside(home, 2.0);
    assert_eq!(
        post(&router, &owner, "/players/me/place", target("sword", spot))
            .await
            .status(),
        StatusCode::CONFLICT,
        "not a thing for the world"
    );
    assert_eq!(
        post(
            &router,
            &owner,
            "/players/me/place",
            target("unicorn", spot)
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        post(
            &router,
            &owner,
            "/players/me/place",
            target("campfire", beside(home, 60.0))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "out of reach"
    );
    let mut bad = target("campfire", spot);
    bad["yaw"] = serde_json::json!(-1.0);
    assert_eq!(
        post(&router, &owner, "/players/me/place", bad)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(&router, "", "/players/me/place", target("campfire", spot))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Without a campfire nothing can be cooked.
    let cook = serde_json::json!({"recipe": "cook_meat", "count": "2"});
    assert_eq!(
        post(&router, &owner, "/players/me/craft", cook.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let placed = response_json(
        post(
            &router,
            &owner,
            "/players/me/place",
            target("campfire", spot),
        )
        .await,
    )
    .await;
    assert_eq!(placed["object"]["kind"], "campfire");
    assert_eq!(placed["object"]["station"], "campfire");
    assert_eq!(placed["object"]["owner"], "builder");
    assert_eq!(stock(&pool, "builder", "campfire").await, 2);
    let id = placed["object"]["id"].as_i64().unwrap();
    let around = listed(&router, home).await;
    assert_eq!(around.len(), 1);
    assert_eq!(around[0]["model"], "survival.campfire-pit");
    assert!(
        listed(&router, beside(home, 500.0)).await.is_empty(),
        "far away it is not listed"
    );
    assert_eq!(
        post(
            &router,
            &owner,
            "/players/me/place",
            target("campfire", beside(spot, 0.3))
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "too close"
    );
    assert_eq!(
        stock(&pool, "builder", "campfire").await,
        2,
        "a refused placing keeps the item"
    );

    // Anyone near the campfire can cook; far from it nobody can.
    assert_eq!(
        post(&router, &owner, "/players/me/craft", cook.clone())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        (
            stock(&pool, "builder", "raw_meat").await,
            stock(&pool, "builder", "cooked_meat").await
        ),
        (2, 2)
    );
    give(&pool, "guest", &[("raw_meat", 1)]).await;
    assert_eq!(
        post(
            &router,
            &guest,
            "/players/me/craft",
            serde_json::json!({"recipe": "cook_meat", "count": "1"})
        )
        .await
        .status(),
        StatusCode::OK,
        "a station is public"
    );
    teleport(&pool, ids[0], beside(home, 30.0)).await;
    assert_eq!(
        post(
            &router,
            &owner,
            "/players/me/craft",
            serde_json::json!({"recipe": "cook_meat", "count": "1"})
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "too far from the fire"
    );
    assert_eq!(
        post(
            &router,
            &owner,
            &format!("/placed/{id}/pickup"),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "too far to pick up"
    );
    teleport(&pool, ids[0], home).await;

    // Only the owner picks a thing up.
    assert_eq!(
        post(
            &router,
            &guest,
            &format!("/placed/{id}/pickup"),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &router,
            &owner,
            "/placed/9999/pickup",
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            &router,
            &owner,
            &format!("/placed/{id}/pickup"),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(stock(&pool, "builder", "campfire").await, 3);
    assert!(listed(&router, home).await.is_empty());

    // The recipes of the workshop need their stations.
    give(
        &pool,
        "builder",
        &[
            ("iron_ingot", 6),
            ("stone_block", 4),
            ("iron_ore", 2),
            ("pine_wood", 5),
        ],
    )
    .await;
    assert_eq!(
        post(
            &router,
            &owner,
            "/players/me/craft",
            serde_json::json!({"recipe": "make_anvil", "count": "1"})
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "the anvil is made at a fire"
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn crops_grow_with_the_clock_and_only_the_owner_harvests(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let farmer = create_session(&router, "farmer").await;
    let thief = create_session(&router, "thief").await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM players ORDER BY username")
        .fetch_all(&pool)
        .await
        .unwrap();
    let home = position_of(&pool, ids[0]).await;
    teleport(&pool, ids[1], beside(home, 1.0)).await;
    give(&pool, "farmer", &[("seed_carrot", 2)]).await;

    let field = beside(home, 2.0);
    assert_eq!(
        post(
            &router,
            &farmer,
            "/players/me/plant",
            target("carrot", field)
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "a carrot is not a seed"
    );
    let planted = response_json(
        post(
            &router,
            &farmer,
            "/players/me/plant",
            target("seed_carrot", field),
        )
        .await,
    )
    .await;
    let id = planted["object"]["id"].as_i64().unwrap();
    assert_eq!(planted["object"]["kind"], "crop_carrot");
    assert_eq!(planted["object"]["crop"]["ripe"], false);
    assert_eq!(stock(&pool, "farmer", "seed_carrot").await, 1);
    assert_eq!(
        post(
            &router,
            &farmer,
            "/players/me/plant",
            target("seed_carrot", beside(field, 0.2))
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "plants need room"
    );

    let harvest = format!("/placed/{id}/harvest");
    assert_eq!(
        post(&router, &farmer, &harvest, serde_json::json!({}))
            .await
            .status(),
        StatusCode::CONFLICT,
        "not ripe yet"
    );
    // Half the time passes: it has grown but is not ripe.
    sqlx::query("UPDATE placed_objects SET planted_at = now() - interval '10 minutes', ripe_at = now() + interval '10 minutes'").execute(&pool).await.unwrap();
    let half = &listed(&router, home).await[0];
    let growth = half["crop"]["growth"].as_f64().unwrap();
    assert!((0.45..0.55).contains(&growth), "{growth}");
    assert!(half["scale"].as_f64().unwrap() > 0.5 && half["scale"].as_f64().unwrap() < 1.0);

    // Ripe by the clock, as if the farmer had been offline.
    sqlx::query("UPDATE placed_objects SET planted_at = now() - interval '2 hours', ripe_at = now() - interval '1 hour'").execute(&pool).await.unwrap();
    assert_eq!(listed(&router, home).await[0]["crop"]["ripe"], true);
    assert_eq!(
        post(&router, &thief, &harvest, serde_json::json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "someone else's field"
    );
    let before = stock(&pool, "farmer", "carrot").await;
    let reply = response_json(post(&router, &farmer, &harvest, serde_json::json!({})).await).await;
    assert!(reply["items"].as_array().unwrap().len() == 2);
    let gained = stock(&pool, "farmer", "carrot").await - before;
    assert!((2..=4).contains(&gained), "{gained}");
    assert_eq!(
        stock(&pool, "farmer", "seed_carrot").await,
        2,
        "the seed comes back"
    );
    assert!(listed(&router, home).await.is_empty());
    assert_eq!(
        post(&router, &farmer, &harvest, serde_json::json!({}))
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "harvested once"
    );

    // An unripe crop can be dug up for its seed.
    let again = response_json(
        post(
            &router,
            &farmer,
            "/players/me/plant",
            target("seed_carrot", field),
        )
        .await,
    )
    .await;
    let id = again["object"]["id"].as_i64().unwrap();
    assert_eq!(
        post(
            &router,
            &farmer,
            &format!("/placed/{id}/pickup"),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(stock(&pool, "farmer", "seed_carrot").await, 2);

    // The number of crops of one character is bounded.
    give(&pool, "farmer", &[("seed_corn", 50)]).await;
    sqlx::query("INSERT INTO placed_objects (world_id, owner_id, kind, position_x, position_y, position_z, ripe_at) SELECT $1, $2, 'crop_corn', 1, 1, g, now() FROM generate_series(1, 60) AS g")
        .bind(world_id).bind(ids[0]).execute(&pool).await.unwrap();
    assert_eq!(
        post(
            &router,
            &farmer,
            "/players/me/plant",
            target("seed_corn", beside(home, 3.0))
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "too many crops"
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn fishing_needs_a_rod_and_water(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    // One of the six faces of the planet is sea, the rest is land.
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    for _row in 0..16 {
        for column in 0..96 {
            pgm.push(if column / 16 == 5 { 60 } else { 144 });
        }
    }
    crate::story::world::reset_cache().await;
    let world_id = initialize(&pool, &config(), Some((&pgm, 42)))
        .await
        .unwrap();
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let angler = create_session(&router, "angler").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let land = position_of(&pool, id).await;
    let terrain = crate::movement::terrain(&state).await.unwrap();
    let shore = (0..4000)
        .map(|index| {
            let z = 1.0 - 2.0 * (f64::from(index) + 0.5) / 4000.0;
            let ring = (1.0 - z * z).sqrt();
            let angle = f64::from(index) * 2.399_963;
            let direction = [ring * angle.cos(), ring * angle.sin(), z];
            direction.map(|value| value * crate::movement::RADIUS)
        })
        .find(|point| terrain.water_near(*point, 6.0) == WaterNear::Salt)
        .expect("the sea face has water");

    let fish = |token: String, router: Router| async move {
        post(&router, &token, "/players/me/fish", serde_json::json!({})).await
    };
    teleport(&pool, id, shore).await;
    assert_eq!(
        fish(angler.clone(), router.clone()).await.status(),
        StatusCode::CONFLICT,
        "no rod in hand"
    );
    give(&pool, "angler", &[("fishing_rod", 1)]).await;
    assert_eq!(
        post(
            &router,
            &angler,
            "/players/me/equip",
            serde_json::json!({"item_id": "fishing_rod"})
        )
        .await
        .status(),
        StatusCode::OK
    );
    teleport(&pool, id, land).await;
    assert_eq!(
        terrain.water_near(land, 6.0),
        WaterNear::None,
        "the spawn is dry"
    );
    assert_eq!(
        fish(angler.clone(), router.clone()).await.status(),
        StatusCode::CONFLICT,
        "no water within reach"
    );
    teleport(&pool, id, shore).await;
    let mut caught = 0;
    for _ in 0..16 {
        sqlx::query("UPDATE players SET last_gathered_at = NULL, stamina = 100")
            .execute(&pool)
            .await
            .unwrap();
        let reply = response_json(fish(angler.clone(), router.clone()).await).await;
        if reply["caught"] == true {
            caught += 1;
            assert_eq!(reply["items"][0]["item_id"], "fish");
        } else {
            assert!(reply["items"].as_array().unwrap().is_empty());
        }
        assert!(reply["wear"]["durability"].as_i64().unwrap() < 120);
    }
    assert_eq!(stock(&pool, "angler", "fish").await, caught);
    assert!(
        (1..=16).contains(&caught),
        "about half of 16 casts catch: {caught}"
    );
    assert_eq!(
        fish(angler.clone(), router.clone()).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "one cast at a time"
    );
}
