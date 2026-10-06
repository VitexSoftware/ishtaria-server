use super::*;

mod building;
mod creatures;
mod drinking;
mod durability;
mod experience;
mod halls;
mod harvest;
mod leases;
mod pacts;
mod social;
mod story;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

static AUTH_TESTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn inventory_is_granted_once_and_uuid_is_unique(pool: PgPool) {
    let world_id = initialize(&pool, &config(), None).await.unwrap();
    let (player_id, uuid): (i64, String) = sqlx::query_as(
        "INSERT INTO players (world_id, username, password_hash) VALUES ($1, 'inventory-player', 'test-only') RETURNING id, uuid::text",
    ).bind(world_id).fetch_one(&pool).await.unwrap();
    assert_eq!(uuid.len(), 36);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM player_inventory WHERE player_id = $1")
            .bind(player_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 8,
        "four food stacks, the starting coins and three tools"
    );
    sqlx::query("DELETE FROM player_inventory WHERE player_id = $1")
        .bind(player_id)
        .execute(&pool)
        .await
        .unwrap();
    initialize(&pool, &config(), None).await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM player_inventory WHERE player_id = $1")
            .bind(player_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert!(sqlx::query("INSERT INTO players (world_id, username, password_hash, uuid) VALUES ($1, 'duplicate-player', 'test-only', $2::uuid)")
        .bind(world_id).bind(uuid).execute(&pool).await.is_err());
    set_gold(&pool, Some(player_id), 50).await;
    set_gold(&pool, Some(player_id), 80).await;
    let lifetime: i64 = sqlx::query_scalar("SELECT lifetime_gold FROM players WHERE id = $1")
        .bind(player_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    // 100 starting coins, all removed above, then 50 and 30 more gained.
    assert_eq!(lifetime, 180);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn new_player_gold_is_persistent_and_nonnegative(pool: PgPool) {
    let cfg = config();
    let world_id = initialize(&pool, &cfg, None).await.unwrap();
    let player_id: i64 = sqlx::query_scalar(
        "INSERT INTO players (world_id, username, password_hash) VALUES ($1, 'new-player', 'test-only') RETURNING id",
    ).bind(world_id).fetch_one(&pool).await.unwrap();
    assert_eq!(gold_of(&pool, player_id).await, 100);
    sqlx::query("UPDATE players SET created_at = now() - interval '12 days 6 hours' WHERE id = $1")
        .bind(player_id)
        .execute(&pool)
        .await
        .unwrap();
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let profile = serde_json::to_value(players::profile(&state, player_id).await.unwrap()).unwrap();
    assert_eq!(profile["life"]["age_days"], "12");
    assert!(profile["life"]["born_at"].as_str().unwrap().ends_with('Z'));
    set_gold(&pool, Some(player_id), 73).await;
    assert_eq!(initialize(&pool, &cfg, None).await.unwrap(), world_id);
    assert_eq!(gold_of(&pool, player_id).await, 73);
    for invalid in [-1, 0] {
        assert!(sqlx::query(
            "UPDATE player_inventory SET quantity = $2 WHERE player_id = $1 AND item_id = 'gold'"
        )
        .bind(player_id)
        .bind(invalid)
        .execute(&pool)
        .await
        .is_err());
    }
    let another: i64 = sqlx::query_scalar(
        "INSERT INTO players (world_id, username, password_hash) VALUES ($1, 'another-player', 'test-only') RETURNING id",
    ).bind(world_id).fetch_one(&pool).await.unwrap();
    assert_eq!(gold_of(&pool, another).await, 100);
    let lifetime: i64 = sqlx::query_scalar("SELECT lifetime_gold FROM players WHERE id = $1")
        .bind(another)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lifetime, 100, "the starting coins are counted once");
}

const SMALL_MAP: &[u8] = b"P5\n6 1\n255\n\x0a\x20\x00\x80\xfe\xff";

async fn initialize_spawn_world(pool: &PgPool) -> i64 {
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    pgm.extend(vec![144; 16 * 16 * 6]);
    initialize(pool, &config(), Some((&pgm, 42))).await.unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn unsafe_spawn_rolls_back_registration(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize(&pool, &config(), None).await.unwrap();
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let credentials = r#"{"username":"unsafe-player","password":""}"#;
    for has_ocean in [false, true] {
        if has_ocean {
            let mut pgm = b"P5\n96 16\n255\n".to_vec();
            pgm.extend(vec![80; 16 * 16 * 6]);
            initialize(&pool, &config(), Some((&pgm, 42)))
                .await
                .unwrap();
        }
        assert_eq!(
            player_request(&router, "POST", "/players", credentials, "")
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let players: i64 = sqlx::query_scalar("SELECT count(*) FROM players")
            .fetch_one(&pool)
            .await
            .unwrap();
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM player_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((players, sessions), (0, 0));
    }
}

async fn response_json(response: Response) -> serde_json::Value {
    assert!(response.status().is_success(), "{}", response.status());
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL and ISHTARIA_TEST_HEIGHTMAP"]
async fn safe_spawn_on_imported_world(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    pgm.extend(vec![144; 16 * 16 * 6]);
    if let Ok(path) = std::env::var("ISHTARIA_TEST_HEIGHTMAP") {
        pgm = std::fs::read(path).unwrap();
    }
    let world_id = initialize(&pool, &config(), Some((&pgm, 20261005)))
        .await
        .unwrap();
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let credentials = r#"{"username":"safe-world-player","password":""}"#;
    let session =
        response_json(player_request(&router, "POST", "/players", credentials, "").await).await;
    let position = &session["player"]["position"];
    let radius = ["x", "y", "z"]
        .iter()
        .map(|axis| position[axis].as_f64().unwrap().powi(2))
        .sum::<f64>()
        .sqrt();
    assert!((6_371_150.0..=6_372_600.0).contains(&radius));
    assert_eq!(session["player"]["stats"]["gold"], "100");
    let login =
        response_json(player_request(&router, "POST", "/players/login", credentials, "").await)
            .await;
    assert_eq!(position, &login["player"]["position"]);
}

async fn create_session(router: &Router, username: &str) -> String {
    let body = format!(r#"{{"username":"{username}","password":"test-password"}}"#);
    response_json(player_request(router, "POST", "/players", &body, "").await).await["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn jumping_is_persistent_and_replay_safe(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let credentials = r#"{"username":"jumping-player","password":""}"#;
    let session =
        response_json(player_request(&router, "POST", "/players", credentials, "").await).await;
    let token = session["token"].as_str().unwrap();
    sqlx::query("UPDATE players SET last_moved_at = clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let intent = r#"{"direction":[0,0,0],"sequence":"1","jump":true}"#;
    let response = player_request(&router, "POST", "/players/me/move", intent, token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let launched = response_json(response).await;
    assert_eq!(launched["position"]["airborne"], true);
    let speed: f64 = sqlx::query_scalar("SELECT jump_vertical_speed FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(speed > 0.0 && speed < 6.5);
    let replay =
        response_json(player_request(&router, "POST", "/players/me/move", intent, token).await)
            .await;
    assert_eq!(replay["position"], launched["position"]);
    assert_eq!(
        speed,
        sqlx::query_scalar::<_, f64>("SELECT jump_vertical_speed FROM players")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    let login =
        response_json(player_request(&router, "POST", "/players/login", credentials, "").await)
            .await;
    assert_eq!(login["player"]["position"], launched["position"]);
    assert_eq!(login["player"]["stats"]["gold"], "100");
    let mut landed = launched;
    for sequence in 2..=9 {
        sqlx::query("UPDATE players SET last_moved_at = clock_timestamp() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let intent =
            serde_json::json!({"direction":[0,0,0],"sequence":sequence.to_string(),"jump":false})
                .to_string();
        landed = response_json(
            player_request(&router, "POST", "/players/me/move", &intent, token).await,
        )
        .await;
    }
    assert_eq!(landed["position"]["airborne"], false);
    assert_eq!(
        0.0,
        sqlx::query_scalar::<_, f64>("SELECT jump_vertical_speed FROM players")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    initialize(&pool, &config(), None).await.unwrap();
    let restarted = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let profile =
        response_json(player_request(&restarted, "GET", "/players/me", "", token).await).await;
    assert_eq!(profile["position"], landed["position"]);
    assert_eq!(profile["stats"]["gold"], "100");
    let malformed = r#"{"direction":[0,0,0],"sequence":"10","jump":1}"#;
    assert_eq!(
        player_request(&router, "POST", "/players/me/move", malformed, token)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn running_is_persistent_bounded_and_replay_safe(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "running-player").await;
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    let origin: [f64; 3] =
        std::array::from_fn(|axis| profile["position"][["x", "y", "z"][axis]].as_f64().unwrap());
    let radius = origin[0].hypot(origin[2]);
    let direction = [-origin[2] / radius, 0.0, origin[0] / radius];
    sqlx::query("UPDATE players SET last_moved_at = clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let intent =
        serde_json::json!({"direction": direction, "sequence": "1", "run": true}).to_string();
    let response = player_request(&router, "POST", "/players/me/move", &intent, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let running = response_json(response).await;
    let forward: f64 = ["x", "y", "z"]
        .iter()
        .enumerate()
        .map(|(axis, name)| {
            (running["position"][name].as_f64().unwrap() - origin[axis]) * direction[axis]
        })
        .sum();
    assert!((forward - 1.5).abs() < 0.00001);
    let replay =
        response_json(player_request(&router, "POST", "/players/me/move", &intent, &token).await)
            .await;
    assert_eq!(replay["position"], running["position"]);
    assert_eq!(replay["moving"], false);
    sqlx::query("UPDATE players SET last_moved_at = clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let intent =
        serde_json::json!({"direction": direction, "sequence": "2", "run": true, "jump": true})
            .to_string();
    let response = player_request(&router, "POST", "/players/me/move", &intent, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let launched = response_json(response).await;
    assert_eq!(launched["position"]["airborne"], true);
    let speed: f64 = sqlx::query_scalar(
        "SELECT sqrt(jump_x * jump_x + jump_y * jump_y + jump_z * jump_z) FROM players",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((speed - 6.0).abs() < 0.00001);
    let vertical_speed: f64 = sqlx::query_scalar("SELECT jump_vertical_speed FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(vertical_speed > 6.0 && vertical_speed <= 8.5);
    initialize(&pool, &config(), None).await.unwrap();
    let restored_speed: f64 = sqlx::query_scalar("SELECT jump_vertical_speed FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(restored_speed, vertical_speed);
    let restarted = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let profile =
        response_json(player_request(&restarted, "GET", "/players/me", "", &token).await).await;
    assert_eq!(profile["position"], launched["position"]);
    assert_eq!(profile["stats"]["gold"], "100");
    let response = player_request(
        &router,
        "POST",
        "/players/me/move",
        r#"{"direction":[0,0,1],"sequence":"3","run":6}"#,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn movement_is_persistent_bounded_and_replay_safe(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let credentials = r#"{"username":"walking-player","password":""}"#;
    let session =
        response_json(player_request(&router, "POST", "/players", credentials, "").await).await;
    let token = session["token"].as_str().unwrap();
    let previous = &session["player"]["position"];
    let horizontal = previous["x"].as_f64().unwrap();
    let depth = previous["z"].as_f64().unwrap();
    let radius = horizontal.hypot(depth);
    let intent = serde_json::json!({"direction": [-depth / radius, 0.0, horizontal / radius], "sequence": "1"}).to_string();
    assert_eq!(
        player_request(&router, "POST", "/players/me/move", &intent, "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE players SET last_moved_at = clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        player_request(&router, "POST", "/players/me/move", &intent, token),
        player_request(&router, "POST", "/players/me/move", &intent, token)
    );
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first = response_json(first).await;
    let second = response_json(second).await;
    assert_eq!(first["position"], second["position"]);
    assert!(first["moving"].as_bool().unwrap() || second["moving"].as_bool().unwrap());
    let distance = ["x", "y", "z"]
        .iter()
        .map(|axis| {
            (first["position"][axis].as_f64().unwrap() - previous[axis].as_f64().unwrap()).powi(2)
        })
        .sum::<f64>()
        .sqrt();
    assert!(distance > 0.99 && distance <= (1.0_f64 + 0.7 * 0.7).sqrt());
    let replay =
        response_json(player_request(&router, "POST", "/players/me/move", &intent, token).await)
            .await;
    assert_eq!(replay["position"], first["position"]);
    assert_eq!(replay["moving"], false);
    for body in [
        serde_json::json!({"direction": [0, 0, 2], "sequence": "2"}),
        serde_json::json!({"direction": [0, 0, 1], "sequence": "-1"}),
        serde_json::json!({"direction": [0, 0, 1], "sequence": "9223372036854775808"}),
    ] {
        assert_eq!(
            player_request(
                &router,
                "POST",
                "/players/me/move",
                &body.to_string(),
                token
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let login =
        response_json(player_request(&router, "POST", "/players/login", credentials, "").await)
            .await;
    assert_eq!(login["player"]["position"], first["position"]);
    assert_eq!(login["player"]["stats"]["gold"], "100");
    assert_eq!(login["player"]["character"], session["player"]["character"]);
    let objects = response_json(
        request(
            &router,
            &format!(
                "/world/objects?x={}&y={}&z={}",
                first["position"]["x"], first["position"]["y"], first["position"]["z"]
            ),
        )
        .await,
    )
    .await;
    let obstacle = objects["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["collision_radius_m"].as_f64().unwrap() > 0.0)
        .unwrap();
    let center: [f64; 3] = std::array::from_fn(|axis| obstacle["position"][axis].as_f64().unwrap());
    let horizontal = center[0].hypot(center[2]);
    let direction = [-center[2] / horizontal, 0.0, center[0] / horizontal];
    let distance = obstacle["collision_radius_m"].as_f64().unwrap() + 0.35 + 0.1;
    let start: [f64; 3] = std::array::from_fn(|axis| center[axis] - direction[axis] * distance);
    let radius = center.iter().map(|value| value * value).sum::<f64>().sqrt();
    let start_radius = start.iter().map(|value| value * value).sum::<f64>().sqrt();
    let start = start.map(|value| value * radius / start_radius);
    sqlx::query("UPDATE players SET position_x = $1, position_y = $2, position_z = $3, last_moved_at = clock_timestamp() - interval '1 second'")
        .bind(start[0]).bind(start[1]).bind(start[2]).execute(&pool).await.unwrap();
    let collision_intent = serde_json::json!({"direction": direction, "sequence": "2"}).to_string();
    let blocked = player_request(
        &router,
        "POST",
        "/players/me/move",
        &collision_intent,
        login["token"].as_str().unwrap(),
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::OK);
    let blocked = response_json(blocked).await;
    assert_eq!(blocked["moving"], false);
    assert_eq!(
        blocked["position"],
        serde_json::json!({"x": start[0], "y": start[1], "z": start[2], "sequence": "2", "airborne": false, "on_object": false})
    );
    let profile = response_json(
        player_request(
            &router,
            "GET",
            "/players/me",
            "",
            login["token"].as_str().unwrap(),
        )
        .await,
    )
    .await;
    assert_eq!(profile["position"], blocked["position"]);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn survival_empty_password(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let credentials = r#"{"username":"passwordless-player","password":""}"#;
    let session =
        response_json(player_request(&router, "POST", "/players", credentials, "").await).await;
    assert_eq!(session["player"]["stats"]["gold"], "100");
    let position = &session["player"]["position"];
    let distance = ["x", "y", "z"]
        .iter()
        .map(|axis| position[axis].as_f64().unwrap().powi(2))
        .sum::<f64>()
        .sqrt();
    let base = 144.0 * 16000.0 / 255.0 - 8000.0;
    let point = ["x", "y", "z"].map(|axis| position[axis].as_f64().unwrap());
    assert!(
        (distance - (6_371_000.0 + base + crate::environment::relief_offset(point, base, "42")))
            .abs()
            < 0.000001
    );
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(stored.starts_with("$argon2id$"));
    let login =
        response_json(player_request(&router, "POST", "/players/login", credentials, "").await)
            .await;
    assert_eq!(
        session["player"]["life"]["uuid"],
        login["player"]["life"]["uuid"]
    );
    assert_ne!(session["token"], login["token"]);
    assert_eq!(session["player"]["position"], login["player"]["position"]);
    sqlx::query("UPDATE players SET position_x = 0, position_y = 6371000, position_z = 0")
        .execute(&pool)
        .await
        .unwrap();
    let moved =
        response_json(player_request(&router, "POST", "/players/login", credentials, "").await)
            .await;
    assert_eq!(
        moved["player"]["position"],
        serde_json::json!({"x": 0.0, "y": 6371000.0, "z": 0.0, "sequence": "0", "airborne": false, "on_object": false})
    );
    sqlx::query("UPDATE players SET position_x = NULL, position_y = NULL, position_z = NULL")
        .execute(&pool)
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        player_request(&router, "POST", "/players/login", credentials, ""),
        player_request(&router, "POST", "/players/login", credentials, "")
    );
    let first = response_json(first).await;
    let second = response_json(second).await;
    assert!(first["player"]["position"]["x"].is_number());
    assert_eq!(first["player"]["position"], second["player"]["position"]);
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/login",
            r#"{"username":"passwordless-player","password":"wrong-password"}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    for password in ["a".to_owned(), "1234567".to_owned(), "a".repeat(129)] {
        let body =
            serde_json::json!({"username": "invalid-player", "password": password}).to_string();
        assert_eq!(
            player_request(&router, "POST", "/players", &body, "")
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/login",
            r#"{"username":"passwordless-player"}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn survival_login_settles_starvation(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "offline-player").await;
    let credentials = r#"{"username":"offline-player","password":"test-password"}"#;
    let second =
        response_json(player_request(&router, "POST", "/players/login", credentials, "").await)
            .await;
    set_gold(&pool, None, i64::MAX).await;
    set_gold(&pool, None, 10).await;
    sqlx::query("UPDATE players SET created_at = now() - interval '1234 days', last_ate_at = now() - interval '7 days'").execute(&pool).await.unwrap();
    let response = player_request(&router, "POST", "/players/login", credentials, "").await;
    assert_eq!(response.status(), StatusCode::GONE);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let notice: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let born_at: String = sqlx::query_scalar("SELECT to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM players").fetch_one(&pool).await.unwrap();
    assert_eq!(
        notice,
        serde_json::json!({"obituary": {"name": "offline-player", "born_at": born_at, "lived_days": "1234", "lifetime_gold": "9223372036854775807", "friends_count": "0"}})
    );
    for old_token in [token.as_str(), second["token"].as_str().unwrap()] {
        assert_eq!(
            player_request(&router, "GET", "/players/me", "", old_token)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let response = player_request(&router, "POST", "/players/login", credentials, "").await;
    assert_eq!(response.status(), StatusCode::GONE);
    let repeated: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(notice, repeated);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM player_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    let grave_gold: i64 =
        sqlx::query_scalar("SELECT quantity FROM grave_inventory WHERE item_id = 'gold'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(grave_gold, 10);
    let new_credentials = r#"{"username":"Offline-Player","password":"next-password"}"#;
    let (first, second) = tokio::join!(
        player_request(&router, "POST", "/players", new_credentials, ""),
        player_request(&router, "POST", "/players", new_credentials, "")
    );
    let (created, conflict) = if first.status() == StatusCode::CREATED {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let new_session = response_json(created).await;
    assert_eq!(new_session["player"]["stats"]["gold"], "100");
    assert_eq!(new_session["player"]["inventory"]["capacity"], 100);
    assert_eq!(new_session["player"]["life"]["age_days"], "0");
    let later_birth: bool = sqlx::query_scalar("SELECT players.created_at > graves.born_at AND players.uuid <> graves.player_uuid FROM players JOIN graves ON graves.world_id = players.world_id WHERE players.died_at IS NULL").fetch_one(&pool).await.unwrap();
    assert!(later_birth);
    assert_eq!(
        player_request(&router, "POST", "/players/login", credentials, "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let login =
        response_json(player_request(&router, "POST", "/players/login", new_credentials, "").await)
            .await;
    assert_eq!(
        login["player"]["life"]["uuid"],
        new_session["player"]["life"]["uuid"]
    );
    assert!(sqlx::query("UPDATE graves SET born_at = now()")
        .execute(&pool)
        .await
        .is_err());
    let saved_birth: String = sqlx::query_scalar("SELECT to_char(born_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM graves").fetch_one(&pool).await.unwrap();
    assert_eq!(saved_birth, born_at);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn survival_activity_recovery_and_dehydration(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let walking_token = create_session(&router, "walking-survivor").await;
    create_session(&router, "running-survivor").await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM players ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET stamina = 50, water = 50, activity_seconds = 0")
        .execute(&pool)
        .await
        .unwrap();
    let mut warmup = pool.begin().await.unwrap();
    assert!(survival::lock_alive(&mut warmup, world_id, ids[0])
        .await
        .unwrap());
    for _ in 0..4 {
        assert!(survival::activity(&mut warmup, ids[0], 0.25, false)
            .await
            .unwrap());
    }
    warmup.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT stamina FROM players WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap(),
        50
    );
    sqlx::query("UPDATE players SET activity_seconds = 10")
        .execute(&pool)
        .await
        .unwrap();
    for (index, id) in ids.iter().enumerate() {
        let mut transaction = pool.begin().await.unwrap();
        assert!(survival::lock_alive(&mut transaction, world_id, *id)
            .await
            .unwrap());
        for _ in 0..4 {
            assert!(survival::activity(&mut transaction, *id, 0.25, index == 1)
                .await
                .unwrap());
        }
        transaction.commit().await.unwrap();
    }
    let values: Vec<(f64, f64)> = sqlx::query_as(
        "SELECT stamina + stamina_fraction, water + water_fraction FROM players ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!((values[0].0 - 49.9).abs() < 1e-8);
    assert!((values[1].0 - 49.5).abs() < 1e-8);
    assert!(values[1].1 < values[0].1);
    sqlx::query("UPDATE players SET stamina = 20, stamina_fraction = 0, last_active_at = clock_timestamp() - interval '3 seconds', survival_updated_at = clock_timestamp() - interval '3 seconds' WHERE id = $1")
        .bind(ids[0]).execute(&pool).await.unwrap();
    survival::settle(&state, ids[0]).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT stamina FROM players WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap(),
        20
    );
    sqlx::query("UPDATE players SET stamina = 0, stamina_fraction = 0, last_active_at = clock_timestamp() - interval '30 seconds', survival_updated_at = clock_timestamp() - interval '20 seconds' WHERE id = $1")
        .bind(ids[0]).execute(&pool).await.unwrap();
    survival::settle(&state, ids[0]).await.unwrap();
    let recovered: (i32, f64, i32) =
        sqlx::query_as("SELECT stamina, activity_seconds, health FROM players WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!((10..=11).contains(&recovered.0));
    assert_eq!(recovered.1, 0.0);
    assert_eq!(recovered.2, 100);
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &walking_token).await)
            .await;
    let origin: [f64; 3] =
        std::array::from_fn(|axis| profile["position"][["x", "y", "z"][axis]].as_f64().unwrap());
    let radius = origin[0].hypot(origin[2]);
    let direction = [-origin[2] / radius, 0.0, origin[0] / radius];
    sqlx::query("UPDATE players SET stamina = 0, stamina_fraction = 0, activity_seconds = 0, last_active_at = clock_timestamp(), survival_updated_at = clock_timestamp(), last_moved_at = clock_timestamp() - interval '1 second' WHERE id = $1")
        .bind(ids[0]).execute(&pool).await.unwrap();
    let intent =
        serde_json::json!({"direction": direction, "sequence": "1", "run": true}).to_string();
    let moved = response_json(
        player_request(&router, "POST", "/players/me/move", &intent, &walking_token).await,
    )
    .await;
    let distance: f64 = ["x", "y", "z"]
        .iter()
        .enumerate()
        .map(|(axis, name)| {
            (moved["position"][name].as_f64().unwrap() - origin[axis]) * direction[axis]
        })
        .sum();
    assert!((distance - 1.0).abs() < 1e-5);
    assert_eq!(moved["stats"]["stamina"], 0);
    assert_eq!(moved["stats"]["health"], 99);
    let health: f64 =
        sqlx::query_scalar("SELECT health + health_fraction FROM players WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    player_request(&router, "POST", "/players/me/move", &intent, &walking_token).await;
    let replay_health: f64 =
        sqlx::query_scalar("SELECT health + health_fraction FROM players WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(health, replay_health);
    sqlx::query("UPDATE players SET health = 1, health_fraction = 0, last_active_at = clock_timestamp(), last_moved_at = clock_timestamp() - interval '1 second' WHERE id = $1")
        .bind(ids[0]).execute(&pool).await.unwrap();
    let fatal_intent =
        serde_json::json!({"direction": direction, "sequence": "2", "run": true}).to_string();
    let response = player_request(
        &router,
        "POST",
        "/players/me/move",
        &fatal_intent,
        &walking_token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::GONE);
    let notice: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(notice["obituary"]["name"], "walking-survivor");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT cause FROM graves WHERE player_id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap(),
        "exhaustion"
    );
    let grave_origin: (f64, f64, f64) =
        sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
            .bind(ids[1])
            .fetch_one(&pool)
            .await
            .unwrap();
    set_gold(&pool, Some(ids[1]), 1000001).await;
    sqlx::query("UPDATE players SET water = 0, water_fraction = 0, health = 1, survival_updated_at = clock_timestamp() - interval '6 seconds' WHERE id = $1")
        .bind(ids[1]).execute(&pool).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    let grave: (String, String, f64, f64, f64) = sqlx::query_as(
        "SELECT cause, kind, position_x, position_y, position_z FROM graves WHERE player_id = $1",
    )
    .bind(ids[1])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grave.0, "dehydration");
    assert_eq!(grave.1, "mausoleum");
    assert_eq!((grave.2, grave.3, grave.4), grave_origin);
    let region_path = format!("/world/objects?x={}&y={}&z={}", grave.2, grave.3, grave.4);
    let region = response_json(player_request(&router, "GET", &region_path, "", "").await).await;
    let memorial = region["memorials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["player_name"] == "running-survivor")
        .unwrap();
    assert_eq!(memorial["kind"], "mausoleum");
    for (axis, expected) in [grave.2, grave.3, grave.4].iter().enumerate() {
        assert!((memorial["position"][axis].as_f64().unwrap() - expected).abs() < 1e-8);
    }
    let memorial_path = region_path.replace("/world/objects", "/world/memorials");
    let memorials =
        response_json(player_request(&router, "GET", &memorial_path, "", "").await).await;
    assert_eq!(memorials, region["memorials"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM graves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM player_sessions WHERE player_id = $1")
            .bind(ids[1])
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn survival_eating_starvation_and_graves(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "hungry-player").await;
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert_eq!(profile["inventory"]["capacity"], 100);
    assert_eq!(profile["inventory"]["used"], 8);
    assert_eq!(profile["life"]["alive"], true);
    assert_eq!(profile["inventory"]["items"][0]["quantity"], "3");
    sqlx::query("UPDATE players SET health = 80, last_ate_at = now() - interval '6 days 23 hours'")
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
    assert_eq!(profile["life"]["calories_consumed"], "95");
    assert_eq!(profile["inventory"]["items"][0]["quantity"], "2");
    assert!(profile["stats"]["food"].as_i64().unwrap() >= 99);
    assert_eq!(profile["stats"]["health"], 80);
    let healed = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"cheese"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert_eq!(healed["stats"]["food"], 100);
    assert_eq!(healed["stats"]["health"], 85);
    sqlx::query("UPDATE players SET health = 99")
        .execute(&pool)
        .await
        .unwrap();
    let capped = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"bread"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert_eq!(capped["stats"]["health"], 100);
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"gold"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"apple","calories":9000}"#,
            &token
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    sqlx::query("UPDATE players SET level = 3")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO player_inventory SELECT id, 'bag', 1 FROM players")
        .execute(&pool)
        .await
        .unwrap();
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert_eq!(profile["inventory"]["capacity"], 140);
    set_gold(&pool, None, 1000000).await;
    sqlx::query("UPDATE players SET last_ate_at = now() - interval '7 days', created_at = now() - interval '12 days 6 hours'").execute(&pool).await.unwrap();
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"apple"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::GONE
    );
    survival::settle_world(&state).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    assert_eq!(
        player_request(&router, "GET", "/players/me", "", &token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (alive, cause, balance): (bool, String, i64) =
        sqlx::query_as("SELECT died_at IS NULL, death_cause, (SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory WHERE player_id = players.id AND item_id = 'gold') FROM players")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!alive);
    assert_eq!(cause, "starvation");
    assert_eq!(balance, 0);
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM player_inventory")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(items, 0);
    assert_eq!(
        player_request(&router, "GET", "/graves", "", &token)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM graves")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let (grave_id, kind, name, uuid, gold, position): (
        i64,
        String,
        String,
        String,
        i64,
        Option<f64>,
    ) = sqlx::query_as(
        "SELECT id, kind, player_name, player_uuid::text, (SELECT quantity FROM grave_inventory WHERE grave_id = graves.id AND item_id = 'gold'), position_x FROM graves",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind, "monument");
    assert_eq!(name, "hungry-player");
    assert_eq!(uuid, profile["life"]["uuid"].as_str().unwrap());
    assert_eq!(gold, 1000000);
    assert!((position.unwrap() - profile["position"]["x"].as_f64().unwrap()).abs() < 0.000001);
    let quantity: i64 = sqlx::query_scalar(
        "SELECT quantity FROM grave_inventory WHERE grave_id = $1 AND item_id = 'apple'",
    )
    .bind(grave_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(quantity, 2);
    let path = format!("/graves/{grave_id}");
    assert_eq!(
        player_request(&router, "GET", &path, "", &token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = player_request(
        &router,
        "POST",
        "/players/login",
        r#"{"username":"hungry-player","password":"test-password"}"#,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::GONE);
    let obituary: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let born_at: String = sqlx::query_scalar("SELECT to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM players WHERE username = 'hungry-player'").fetch_one(&pool).await.unwrap();
    assert_eq!(
        obituary,
        serde_json::json!({"obituary": {"name": "hungry-player", "born_at": born_at, "lived_days": "12", "lifetime_gold": "1000000", "friends_count": "0"}})
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/login",
            r#"{"username":"hungry-player","password":"wrong-password"}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM player_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    let new_token = create_session(&router, "new-character").await;
    let new_profile =
        response_json(player_request(&router, "GET", "/players/me", "", &new_token).await).await;
    assert_eq!(new_profile["life"]["alive"], true);
    assert_ne!(new_profile["life"]["uuid"], profile["life"]["uuid"]);
    assert_eq!(
        player_request(&router, "GET", &path, "", &new_token)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn survival_damage_and_concurrent_loot(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let dead_token = create_session(&router, "wealthy-player").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    set_gold(&pool, Some(id), 1000001).await;
    sqlx::query("UPDATE players SET created_at = now() - interval '5 days 12 hours', position_x = 10, position_y = 20, position_z = 30 WHERE id = $1").bind(id).execute(&pool).await.unwrap();
    for name in ["friend-one", "friend-two"] {
        let friend: i64 = sqlx::query_scalar("INSERT INTO players (world_id, username, password_hash, character) SELECT world_id, $2, password_hash, character FROM players WHERE id = $1 RETURNING id").bind(id).bind(name).fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO player_friendships (player_id, friend_id) VALUES (least($1, $2), greatest($1, $2))").bind(id).bind(friend).execute(&pool).await.unwrap();
    }
    survival::apply_damage(&state, id, 50, survival::DamageCause::Fall)
        .await
        .unwrap();
    assert_eq!(
        response_json(player_request(&router, "GET", "/players/me", "", &dead_token).await).await
            ["stats"]["health"],
        50
    );
    survival::apply_damage(&state, id, 50, survival::DamageCause::FallingTree)
        .await
        .unwrap();
    survival::apply_damage(&state, id, 100, survival::DamageCause::HostilePlayer)
        .await
        .unwrap();
    let grave_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM graves")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(grave_ids.len(), 1);
    let inspect_path = format!("/graves/{}", grave_ids[0]);
    let path = format!("{inspect_path}/loot");
    assert_eq!(
        player_request(&router, "GET", &inspect_path, "", &dead_token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"gold","quantity":"1"}"#,
            &dead_token
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let token = create_session(&router, "living-player").await;
    assert_eq!(
        player_request(&router, "GET", &inspect_path, "", &token)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"gold","quantity":"1"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE players SET position_x = 10, position_y = 20, position_z = 33 WHERE username = 'living-player'").execute(&pool).await.unwrap();
    let grave =
        response_json(player_request(&router, "GET", &inspect_path, "", &token).await).await;
    assert_eq!(grave["kind"], "mausoleum");
    assert_eq!(grave["cause"], "falling_tree");
    assert_eq!(grave["position_x"], 10.0);
    // 1000101 coins fill 101 slots of 10000, so the looter needs the capacity of a high level.
    sqlx::query("UPDATE players SET level = 100 WHERE username = 'living-player'")
        .execute(&pool)
        .await
        .unwrap();
    let body = r#"{"item_id":"gold","quantity":"1000001"}"#;
    let (first, second) = tokio::join!(
        player_request(&router, "POST", &path, body, &token),
        player_request(&router, "POST", &path, body, &token)
    );
    assert!(
        (first.status() == StatusCode::OK && second.status() == StatusCode::CONFLICT)
            || (second.status() == StatusCode::OK && first.status() == StatusCode::CONFLICT)
    );
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert_eq!(profile["stats"]["gold"], "1000101");
    assert_eq!(
        profile["inventory"]["used"], 108,
        "4 food stacks, 3 tools and 101 coin slots"
    );
    sqlx::query("DELETE FROM player_friendships")
        .execute(&pool)
        .await
        .unwrap();
    let response = player_request(
        &router,
        "POST",
        "/players/login",
        r#"{"username":"wealthy-player","password":"test-password"}"#,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::GONE);
    let obituary: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let born_at: String = sqlx::query_scalar("SELECT to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM players WHERE username = 'wealthy-player'").fetch_one(&pool).await.unwrap();
    assert_eq!(
        obituary,
        serde_json::json!({"obituary": {"name": "wealthy-player", "born_at": born_at, "lived_days": "5", "lifetime_gold": "1000001", "friends_count": "2"}})
    );
    let emptied =
        response_json(player_request(&router, "GET", &inspect_path, "", &token).await).await;
    assert_eq!(emptied["gold"], "0");
    assert_eq!(emptied["obituary"], obituary["obituary"]);
    assert!(sqlx::query("DELETE FROM graves WHERE id = $1")
        .bind(grave_ids[0])
        .execute(&pool)
        .await
        .is_err());
    assert!(
        sqlx::query("UPDATE graves SET player_name = 'forgotten' WHERE id = $1")
            .bind(grave_ids[0])
            .execute(&pool)
            .await
            .is_err()
    );
    let profile = response_json(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"apple","quantity":"3"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert_eq!(profile["inventory"]["items"][0]["quantity"], "6");
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"apple","quantity":"1"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let remaining =
        response_json(player_request(&router, "GET", &inspect_path, "", &token).await).await;
    for item in remaining["items"].as_array().unwrap() {
        let body = serde_json::json!({"item_id": item["item_id"], "quantity": item["quantity"]})
            .to_string();
        assert_eq!(
            player_request(&router, "POST", &path, &body, &token)
                .await
                .status(),
            StatusCode::OK
        );
    }
    initialize(&pool, &config(), None).await.unwrap();
    let permanent =
        response_json(player_request(&router, "GET", &inspect_path, "", &token).await).await;
    assert_eq!(permanent["items"], serde_json::json!([]));
    assert_eq!(permanent["obituary"], obituary["obituary"]);
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"gold","quantity":"0"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let mut other = config();
    other.server_name = "other.ishtaria.example.org".into();
    let other_id = initialize(&pool, &other, None).await.unwrap();
    let other_router = app(AppState {
        pool: pool.clone(),
        world_id: other_id,
    });
    assert_eq!(
        player_request(&other_router, "POST", &path, body, &token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn survival_capacity_and_stacking(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    create_session(&router, "dead-player").await;
    let dead_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO item_types (id, name, stack_limit) VALUES ('pebble', 'Pebble', 100)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, 'pebble', 1), ($1, 'bag', 1), ($1, 'suitcase', 1)")
        .bind(dead_id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE players SET position_x = 0, position_y = 0, position_z = 0 WHERE id = $1")
        .bind(dead_id)
        .execute(&pool)
        .await
        .unwrap();
    survival::apply_damage(&state, dead_id, 100, survival::DamageCause::Disease)
        .await
        .unwrap();
    let (grave_id, kind, cause): (i64, String, String) =
        sqlx::query_as("SELECT id, kind, cause FROM graves")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, "headstone");
    assert_eq!(cause, "disease");
    let token = create_session(&router, "full-player").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players WHERE username = 'full-player'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE players SET position_x = 0, position_y = 0, position_z = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO item_types (id, name, stack_limit) SELECT 'filler-' || value, 'Filler', 1 FROM generate_series(1,92) AS value").execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO player_inventory SELECT $1, id, 1 FROM item_types WHERE id LIKE 'filler-%'",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let path = format!("/graves/{grave_id}/loot");
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"pebble","quantity":"1"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let profile = response_json(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"apple","quantity":"3"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert_eq!(profile["inventory"]["used"], 100);
    assert_eq!(profile["inventory"]["items"][0]["quantity"], "6");
    let profile = response_json(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"bag","quantity":"1"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert_eq!(profile["inventory"]["used"], 101);
    assert_eq!(profile["inventory"]["capacity"], 120);
    let profile = response_json(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"suitcase","quantity":"1"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert_eq!(profile["inventory"]["capacity"], 170);
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"pebble","quantity":"1"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE player_inventory SET quantity = 99 WHERE player_id = $1 AND item_id = 'cheese'",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        player_request(
            &router,
            "POST",
            &path,
            r#"{"item_id":"cheese","quantity":"2"}"#,
            &token
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    sqlx::query(
        "UPDATE player_inventory SET quantity = 1 WHERE player_id = $1 AND item_id = 'apple'",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let eat_body = r#"{"item_id":"apple"}"#;
    let (first, second) = tokio::join!(
        player_request(&router, "POST", "/players/me/eat", eat_body, &token),
        player_request(&router, "POST", "/players/me/eat", eat_body, &token)
    );
    assert!(
        (first.status() == StatusCode::OK && second.status() == StatusCode::BAD_REQUEST)
            || (second.status() == StatusCode::OK && first.status() == StatusCode::BAD_REQUEST)
    );
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    assert_eq!(profile["life"]["calories_consumed"], "95");
    if std::env::var_os("ISHTARIA_TEST_CLIENT").is_some() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new("godot4")
                .args([
                    "--headless",
                    "--path",
                    "../ishtaria-client",
                    "--max-fps",
                    "60",
                    "--script",
                    "res://tests/character_creation.gd",
                ])
                .env("ISHTARIA_PLAYER_TEST_URL", format!("http://{address}"))
                .env("ISHTARIA_TEST_OBITUARY", "1")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        server.abort();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("{stdout}");
        assert!(
            output.status.success() && !stderr.contains("ERROR"),
            "{stderr}"
        );
    }
}

async fn player_request(
    router: &Router,
    method: &str,
    path: &str,
    body: &str,
    token: &str,
) -> Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn player_api_returns_authoritative_gold(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let credentials = r#"{"username":"new-player","password":"test-password","character":"survivors/survivorFemaleA"}"#;
    let response = player_request(&router, "POST", "/players", credentials, "").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let session: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(session["player"]["stats"]["gold"], "100");
    assert_eq!(session["player"]["character"], "survivors/survivorFemaleA");
    assert!(session["player"].get("password_hash").is_none());
    let token = session["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    set_gold(&pool, None, 73).await;
    let response = player_request(&router, "GET", "/players/me", "", token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let profile: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(profile["stats"]["gold"], "73");
    assert_eq!(profile["character"], "survivors/survivorFemaleA");
    assert_eq!(initialize(&pool, &config(), None).await.unwrap(), world_id);
    let response = player_request(&router, "POST", "/players/login", credentials, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let session: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(session["player"]["stats"]["gold"], "73");
    assert_eq!(session["player"]["character"], "survivors/survivorFemaleA");
    assert_eq!(player_request(&router, "POST", "/players", r#"{"username":"bad-character","password":"test-password","character":"../../unexpected"}"#, "").await.status(), StatusCode::BAD_REQUEST);
    assert!(sqlx::query("UPDATE players SET character = 'unknown'")
        .execute(&pool)
        .await
        .is_err());
    assert_eq!(
        player_request(&router, "POST", "/players", credentials, "")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players",
            r#"{"username":"hacker","password":"test-password","gold":1000}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/login",
            r#"{"username":"new-player","password":"wrong-password"}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        player_request(&router, "PATCH", "/players/me", r#"{"gold":1000}"#, token)
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        player_request(&router, "GET", "/players/me", "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        player_request(&router, "DELETE", "/players/session", "", token)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        player_request(&router, "GET", "/players/me", "", token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE player_sessions SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        player_request(
            &router,
            "GET",
            "/players/me",
            "",
            session["token"].as_str().unwrap()
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

fn config() -> Config {
    Config {
        server_name: "test.ishtaria.example.org".into(),
        ruleset: "core-rules@1.0".into(),
        listen: "127.0.0.1:0".into(),
        database_url: None,
        atmosphere: None,
        public_url: None,
        federation: None,
        monetization: None,
    }
}

async fn request(router: &Router, path: &str) -> Response {
    router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn persistence_and_conflicting_imports(pool: PgPool) {
    let cfg = config();
    let world_id = initialize(&pool, &cfg, Some((SMALL_MAP, 42)))
        .await
        .unwrap();
    assert_eq!(
        initialize(&pool, &cfg, Some((SMALL_MAP, 42)))
            .await
            .unwrap(),
        world_id
    );
    assert_eq!(initialize(&pool, &cfg, None).await.unwrap(), world_id);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM heightmaps")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(initialize(&pool, &cfg, Some((SMALL_MAP, 43)))
        .await
        .is_err());
    let changed_map = b"P5\n6 1\n255\n\x00\x00\x00\x00\x00\x00";
    assert!(initialize(&pool, &cfg, Some((changed_map, 42)))
        .await
        .is_err());
    let mut changed_cfg = config();
    changed_cfg.ruleset = "core-rules@2.0".into();
    assert!(initialize(&pool, &changed_cfg, None).await.is_err());
    let pgm: Vec<u8> = sqlx::query_scalar("SELECT pgm FROM heightmaps WHERE world_id = $1")
        .bind(world_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pgm, SMALL_MAP);
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        versions,
        [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32, 33, 34
        ]
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL; optionally set ISHTARIA_TEST_HEIGHTMAP to the worldgen PGM"]
async fn imported_map_survives_restart_and_http_roundtrip(pool: PgPool) {
    let pgm = match env::var("ISHTARIA_TEST_HEIGHTMAP") {
        Ok(path) => tokio::fs::read(path).await.unwrap(),
        Err(_) => SMALL_MAP.to_vec(),
    };
    let cfg = config();
    let world_id = initialize(&pool, &cfg, Some((&pgm, 42))).await.unwrap();
    let persisted_pixels: Vec<u8> =
        sqlx::query_scalar("SELECT pixels FROM heightmaps WHERE world_id = $1")
            .bind(world_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let decoded = Heightmap::parse(&pgm).unwrap();
    assert_eq!(persisted_pixels, decoded.pixels);
    let restarted_id = initialize(&pool, &cfg, None).await.unwrap();
    assert_eq!(restarted_id, world_id);
    let router = app(AppState {
        pool,
        world_id: restarted_id,
    });
    assert_eq!(request(&router, "/health").await.status(), StatusCode::OK);
    let before_solar = atmosphere::Solar::now().unix_seconds;
    let metadata = request(&router, "/world").await;
    assert_eq!(metadata.status(), StatusCode::OK);
    let metadata = metadata.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&metadata).unwrap();
    assert!(text.contains(&decoded.sha256));
    assert!(text.contains("\"seed\":\"42\""));
    let document: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
    assert_eq!(document["server_version"], env!("CARGO_PKG_VERSION"));
    let solar = &document["solar"];
    assert_eq!(solar["version"], 1);
    let seconds = solar["unix_seconds"].as_f64().unwrap();
    assert!(seconds >= before_solar && seconds <= atmosphere::Solar::now().unix_seconds);
    assert_eq!(solar["sidereal_day_seconds"], 86164.0905);
    let expected = atmosphere::Solar::at(seconds);
    for (component, expected_component) in solar["direction"]
        .as_array()
        .unwrap()
        .iter()
        .zip(expected.direction)
    {
        assert!((component.as_f64().unwrap() - expected_component).abs() < 1e-12);
    }
    assert_eq!(solar["direction"].as_array().unwrap().len(), 3);
    assert!(
        (solar["angular_radius_degrees"].as_f64().unwrap() - expected.angular_radius_degrees).abs()
            < 1e-12
    );
    let shifted_router = router.clone().layer(axum::Extension(43200_i64));
    let before_shifted = atmosphere::Solar::now().unix_seconds + 43200.0;
    let shifted = response_json(request(&shifted_router, "/world").await).await;
    let shifted_seconds = shifted["solar"]["unix_seconds"].as_f64().unwrap();
    assert!(shifted_seconds >= before_shifted);
    assert!(shifted_seconds <= atmosphere::Solar::now().unix_seconds + 43200.0);
    assert_eq!(shifted["sha256"], document["sha256"]);
    let expected_shifted = atmosphere::Solar::at(shifted_seconds);
    for (component, expected_component) in shifted["solar"]["direction"]
        .as_array()
        .unwrap()
        .iter()
        .zip(expected_shifted.direction)
    {
        assert!((component.as_f64().unwrap() - expected_component).abs() < 1e-12);
    }
    let later = response_json(request(&shifted_router, "/world").await).await;
    assert!(later["solar"]["unix_seconds"].as_f64().unwrap() > shifted_seconds);
    let response = request(&router, "/world/heightmap").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "image/x-portable-graymap"
    );
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        pgm
    );
    let size = decoded.face_size;
    let response = request(&router, "/world/heightmap.png").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    let png = response.into_body().collect().await.unwrap().to_bytes();
    let png = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
    assert_eq!(png.width(), size as u32 * 6);
    assert_eq!(png.height(), size as u32);
    assert_eq!(png.into_luma8().into_raw(), persisted_pixels);
    let environment = response_json(request(&router, "/world/environment").await).await;
    assert_eq!(environment["version"], 2);
    assert_eq!(environment["heightmap_sha256"], decoded.sha256);
    assert_eq!(environment["seed"], "42");
    assert_eq!(
        environment["biomes"].as_array().unwrap().len(),
        (size as usize).min(environment::MAX_FACE_SIZE).pow(2) * 6
    );
    assert_eq!(
        environment,
        response_json(request(&router, "/world/environment").await).await
    );
    let object_path = "/world/objects?x=6371000&y=0&z=0";
    let objects = request(&router, object_path).await;
    assert_eq!(objects.status(), StatusCode::OK);
    let objects = response_json(objects).await;
    assert_eq!(objects["version"], 1);
    assert_eq!(objects["heightmap_sha256"], decoded.sha256);
    assert_eq!(objects["seed"], "42");
    assert!(objects["objects"].as_array().unwrap().len() <= 512);
    // Walking animals and the clock change from moment to moment; everything else is stable.
    let stable = |mut region: serde_json::Value| {
        region.as_object_mut().unwrap().remove("server_time_ms");
        region["objects"]
            .as_array_mut()
            .unwrap()
            .retain(|object| object.get("wander").is_none());
        region
    };
    assert!(objects["server_time_ms"].as_i64().unwrap() > 0);
    assert_eq!(
        stable(objects.clone()),
        stable(response_json(request(&router, object_path).await).await)
    );
    for query in [
        "x=0&y=0&z=0",
        "x=NaN&y=0&z=0",
        "x=9000000&y=0&z=0",
        "x=6371000&y=0&z=0&span=1000000",
    ] {
        assert_eq!(
            request(&router, &format!("/world/objects?{query}"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    for face in 0..6 {
        for (x, y) in [(0, 0), (size - 1, size - 1), (size / 2, size / 2)] {
            let response = request(&router, &format!("/terrain/{face}/{x}/{y}")).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let index = (y * 6 * size + face * size + x) as usize;
            let expected = format!(
                "{{\"face\":{face},\"x\":{x},\"y\":{y},\"value\":{}}}",
                decoded.pixels[index]
            );
            assert_eq!(body.as_ref(), expected.as_bytes());
        }
    }
    for (path, status) in [
        ("/terrain/6/0/0".to_string(), StatusCode::BAD_REQUEST),
        ("/terrain/0/-1/0".to_string(), StatusCode::BAD_REQUEST),
        (format!("/terrain/0/{size}/0"), StatusCode::NOT_FOUND),
        (format!("/terrain/0/0/{size}"), StatusCode::NOT_FOUND),
    ] {
        assert_eq!(request(&router, &path).await.status(), status);
    }
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/world/heightmap")
                .body(Body::from("unauthorized overwrite"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn invalid_import_rolls_back_and_database_outage_fails_health(pool: PgPool) {
    let cfg = config();
    assert!(initialize(&pool, &cfg, Some((b"invalid", 42)))
        .await
        .is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM worlds")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let world_id = initialize(&pool, &cfg, None).await.unwrap();
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    assert_eq!(request(&router, "/health").await.status(), StatusCode::OK);
    assert_eq!(request(&router, "/world").await.status(), StatusCode::OK);
    assert_eq!(
        request(&router, "/world/heightmap").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&router, "/world/heightmap.png").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&router, "/world/environment").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&router, "/terrain/0/0/0").await.status(),
        StatusCode::NOT_FOUND
    );
    pool.close().await;
    assert_eq!(
        request(&router, "/health").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL and ISHTARIA_TEST_HEIGHTMAP"]
async fn banned_player_cannot_log_in_or_use_session(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    pgm.extend(vec![144; 16 * 16 * 6]);
    if let Ok(path) = std::env::var("ISHTARIA_TEST_HEIGHTMAP") {
        pgm = std::fs::read(path).unwrap();
    }
    let world_id = initialize(&pool, &config(), Some((&pgm, 20261005)))
        .await
        .unwrap();
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let token = create_session(&router, "banned-player").await;
    let credentials = r#"{"username":"banned-player","password":"test-password"}"#;
    assert_eq!(
        player_request(&router, "GET", "/players/me", "", &token)
            .await
            .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE players SET banned_at = now(), ban_reason = 'test' WHERE username = 'banned-player'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        player_request(&router, "GET", "/players/me", "", &token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        player_request(&router, "POST", "/players/login", credentials, "")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE players SET banned_at = NULL, ban_reason = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        player_request(&router, "POST", "/players/login", credentials, "")
            .await
            .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn scheduled_shutdown_is_announced_in_world(pool: PgPool) {
    let world_id = initialize(&pool, &config(), None).await.unwrap();
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    })
    .layer(Extension(0_i64));
    let world = response_json(request(&router, "/world").await).await;
    assert_eq!(world["messages"], serde_json::json!([]));
    sqlx::query("INSERT INTO server_shutdown (world_id, shutdown_at, message) VALUES ($1, now() + interval '90 seconds', 'Maintenance')")
        .bind(world_id)
        .execute(&pool)
        .await
        .unwrap();
    let world = response_json(request(&router, "/world").await).await;
    let message = &world["messages"][0];
    assert_eq!(message["kind"], "system");
    assert_eq!(message["code"], "shutdown");
    assert_eq!(message["text"], "Maintenance");
    let left = message["seconds_left"].as_i64().unwrap();
    assert!((85..=90).contains(&left), "{left}");
    sqlx::query("DELETE FROM server_shutdown")
        .execute(&pool)
        .await
        .unwrap();
    let world = response_json(request(&router, "/world").await).await;
    assert_eq!(world["messages"], serde_json::json!([]));
}

/// Sets the coin stack of one player (or of all players) in the inventory.
async fn set_gold(pool: &PgPool, player_id: Option<i64>, amount: i64) {
    if amount > 0 {
        sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) SELECT id, 'gold', $1 FROM players WHERE $2::bigint IS NULL OR id = $2 ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = EXCLUDED.quantity")
            .bind(amount).bind(player_id).execute(pool).await.unwrap();
    } else {
        sqlx::query("DELETE FROM player_inventory WHERE item_id = 'gold' AND ($1::bigint IS NULL OR player_id = $1)")
            .bind(player_id).execute(pool).await.unwrap();
    }
}

async fn gold_of(pool: &PgPool, player_id: i64) -> i64 {
    sqlx::query_scalar("SELECT coalesce(sum(quantity), 0)::bigint FROM player_inventory WHERE player_id = $1 AND item_id = 'gold'")
        .bind(player_id).fetch_one(pool).await.unwrap()
}
