use super::harvest::{cooldown_reset, position_of, teleport};
use super::*;

async fn equip(router: &Router, token: &str, item: &str) {
    let reply = player_request(
        router,
        "POST",
        "/players/me/equip",
        &serde_json::json!({ "item_id": item }).to_string(),
        token,
    )
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
}

async fn creature_request(router: &Router, path: &str, token: &str, id: &str) -> Response {
    player_request(
        router,
        "POST",
        path,
        &serde_json::json!({ "object_id": id }).to_string(),
        token,
    )
    .await
}

/// A land animal of the generated world and where it stands right now.
pub(super) fn animal_near(
    terrain: &movement::WalkingTerrain,
    wanted: Option<&str>,
) -> movement::WorldObject {
    for step in 0..400 {
        let angle = f64::from(step) * 0.0003;
        let place = movement::unit([angle.cos(), angle.sin(), 0.1]);
        let origin = place.map(|value| value * (movement::RADIUS + terrain.ground_height(place)));
        let now = movement::unix_now_ms();
        for (animal, route) in terrain.moving_animals(origin, now) {
            if route.is_some() && wanted.is_none_or(|model| animal.model == model) {
                return animal;
            }
        }
    }
    panic!("no animal found");
}

async fn stock(pool: &PgPool, id: i64, item: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory WHERE player_id = $1 AND item_id = $2",
    )
    .bind(id)
    .bind(item)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn animals_are_butchered_for_meat_and_return_later(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "hunter").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let terrain = movement::terrain(&state).await.unwrap();
    let animal = animal_near(&terrain, None);
    let path = "/players/me/butcher";

    // The weapon in hand decides; with nothing in hand there is nothing to hit with.
    teleport(&pool, player_id, animal.position).await;
    sqlx::query("DELETE FROM player_equipment")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        creature_request(&router, path, &token, &animal.id)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    equip(&router, &token, "sword").await;
    for bad in ["", "nonsense", &"1:".repeat(80), "42:fauna:0:1:1:9"] {
        let status = creature_request(&router, path, &token, bad).await.status();
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{bad:?}: {status}"
        );
    }
    assert_eq!(
        player_request(
            &router,
            "POST",
            path,
            r#"{"object_id":"x","weapon":"sword"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the client cannot choose the weapon"
    );

    // Far away the animal cannot be reached.
    let spawn = position_of(&pool, player_id).await;
    teleport(&pool, player_id, [spawn[0] + 60.0, spawn[1], spawn[2]]).await;
    assert_eq!(
        creature_request(&router, path, &token, &animal.id)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    // Several swings are needed; the cooldown is enforced between them.
    let animal = terrain
        .animal_by_id(&animal.id, movement::unix_now_ms())
        .unwrap();
    teleport(&pool, player_id, animal.position).await;
    let first = response_json(creature_request(&router, path, &token, &animal.id).await).await;
    assert_eq!(first["weapon"], "sword");
    let required = first["hits_required"].as_i64().unwrap();
    assert!(required > 1, "an animal does not fall to one swing");
    assert_eq!(first["state"], "hit");
    assert_eq!(
        creature_request(&router, path, &token, &animal.id)
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let mut last = first;
    for _ in 1..required {
        cooldown_reset(&pool).await;
        let animal = terrain
            .animal_by_id(&animal.id, movement::unix_now_ms())
            .unwrap();
        teleport(&pool, player_id, animal.position).await;
        last = response_json(creature_request(&router, path, &token, &animal.id).await).await;
    }
    assert_eq!(last["state"], "depleted");
    let meat = stock(&pool, player_id, "raw_meat").await;
    assert!(meat >= 1, "meat in the inventory: {meat}");
    assert!(last["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["item_id"] == "raw_meat"));
    assert!(terrain
        .animal_by_id(&animal.id, movement::unix_now_ms())
        .is_none());
    cooldown_reset(&pool).await;
    assert_eq!(
        creature_request(&router, path, &token, &animal.id)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // The animal returns once its time is up, and the meat can be eaten.
    sqlx::query("UPDATE world_object_state SET depleted_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM world_object_state WHERE object_id = $1 AND depleted_until <= now()",
    )
    .bind(&animal.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored, 1,
        "the butchered animal is stored as a change of the world"
    );
    sqlx::query("UPDATE players SET food = 10")
        .execute(&pool)
        .await
        .ok();
    let eaten = player_request(
        &router,
        "POST",
        "/players/me/eat",
        r#"{"item_id":"raw_meat"}"#,
        &token,
    )
    .await;
    assert_eq!(eaten.status(), StatusCode::OK);
    assert_eq!(stock(&pool, player_id, "raw_meat").await, meat - 1);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_cow_gives_a_drink_now_and_then(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "milkmaid").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let terrain = movement::terrain(&state).await.unwrap();
    let path = "/players/me/milk";

    // A pasture beside a village; look for one with a cow and a bull.
    let direction = movement::unit([1.0, 0.0, 0.1]);
    let centre = direction.map(|value| value * movement::RADIUS);
    let mut cow = None;
    for number in 0..40 {
        movement::set_farms(
            format!("42:{}", terrain.heightmap_sha256()),
            vec![movement::FarmSite {
                id: format!("world:town_{number:02}"),
                direction,
                radius_m: 85.0,
            }],
        );
        let animals = terrain.farm_animals(centre);
        if animals.iter().any(|animal| animal.model == "animal.cow")
            && animals.iter().any(|animal| animal.model != "animal.cow")
        {
            cow = animals.iter().find(|a| a.model == "animal.cow").cloned();
            break;
        }
    }
    let cow = cow.expect("a pasture with a cow");
    let other = terrain
        .farm_animals(centre)
        .into_iter()
        .find(|animal| animal.model != "animal.cow")
        .unwrap();
    let standing = |id: &str| terrain.animal_by_id(id, movement::unix_now_ms()).unwrap();

    sqlx::query("UPDATE players SET water = 40, water_fraction = 0")
        .execute(&pool)
        .await
        .unwrap();
    teleport(&pool, player_id, standing(&other.id).position).await;
    assert_eq!(
        creature_request(&router, path, &token, &other.id)
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "only cows give milk"
    );
    teleport(&pool, player_id, [centre[0] + 200.0, centre[1], centre[2]]).await;
    assert_eq!(
        creature_request(&router, path, &token, &cow.id)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    teleport(&pool, player_id, standing(&cow.id).position).await;
    let drunk = response_json(creature_request(&router, path, &token, &cow.id).await).await;
    assert_eq!(drunk["water"], 30.0);
    let water: f64 = sqlx::query_scalar("SELECT (water + water_fraction)::float8 FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!((69.0..=70.5).contains(&water), "water: {water}");

    // The same cow needs time before it gives milk again; the reserve never exceeds 100.
    assert_eq!(
        creature_request(&router, path, &token, &cow.id)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE creature_milked SET available_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET water = 90, water_fraction = 0")
        .execute(&pool)
        .await
        .unwrap();
    let again = creature_request(&router, path, &token, &cow.id).await;
    assert_eq!(again.status(), StatusCode::OK);
    let water: f64 = sqlx::query_scalar("SELECT (water + water_fraction)::float8 FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(water <= 100.0, "water: {water}");
    movement::set_farms(String::new(), Vec::new());
}
