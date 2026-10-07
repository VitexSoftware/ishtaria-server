use super::*;

async fn place(pool: &PgPool, name: &str, y: f64) {
    sqlx::query("UPDATE players SET position_x = 6371000, position_y = $2, position_z = 0, last_active_at = now(), last_seen_at = now(), last_chat_at = NULL WHERE username = $1")
        .bind(name).bind(y).execute(pool).await.unwrap();
}

async fn post_json(router: &Router, path: &str, body: serde_json::Value, token: &str) -> Response {
    player_request(router, "POST", path, &body.to_string(), token).await
}

async fn events(router: &Router, token: &str) -> Vec<serde_json::Value> {
    response_json(player_request(router, "GET", "/events?after=0", "", token).await).await["events"]
        .as_array()
        .unwrap()
        .clone()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn nearby_players_are_listed_and_hear_spoken_lines(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let anna = create_session(&router, "anna").await;
    let boris = create_session(&router, "boris").await;
    let cyril = create_session(&router, "cyril").await;
    place(&pool, "anna", 0.0).await;
    place(&pool, "boris", 20.0).await;
    place(&pool, "cyril", 1000.0).await;

    let seen =
        response_json(player_request(&router, "GET", "/players/nearby", "", &anna).await).await;
    let names: Vec<_> = seen
        .as_array()
        .unwrap()
        .iter()
        .map(|player| player["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["boris"], "not oneself, not the distant");
    assert!(seen[0]["character"].is_string());
    assert_eq!(
        player_request(&router, "GET", "/players/nearby", "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // A player who has gone quiet is not shown.
    sqlx::query("UPDATE players SET last_active_at = now() - interval '5 minutes', last_seen_at = now() - interval '5 minutes' WHERE username = 'boris'")
        .execute(&pool).await.unwrap();
    let seen =
        response_json(player_request(&router, "GET", "/players/nearby", "", &anna).await).await;
    assert!(seen.as_array().unwrap().is_empty());
    place(&pool, "boris", 20.0).await;

    let said = response_json(
        post_json(
            &router,
            "/chat/say",
            serde_json::json!({"text": "  Hello  "}),
            &anna,
        )
        .await,
    )
    .await;
    assert_eq!(said["delivered"], 1);
    let heard = events(&router, &boris).await;
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0]["kind"], "say");
    assert_eq!(heard[0]["subject"], "anna");
    assert_eq!(heard[0]["body"], "Hello");
    assert!(events(&router, &cyril).await.is_empty(), "too far to hear");
    assert!(
        events(&router, &anna).await.is_empty(),
        "no echo to the speaker"
    );

    // One message per second.
    assert_eq!(
        post_json(
            &router,
            "/chat/say",
            serde_json::json!({"text": "again"}),
            &anna
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    place(&pool, "anna", 0.0).await;
    for text in ["", "   ", "tab\there", &"x".repeat(501)] {
        assert_eq!(
            post_json(
                &router,
                "/chat/say",
                serde_json::json!({"text": text}),
                &anna
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST,
            "{text:?}"
        );
    }
    assert_eq!(
        post_json(
            &router,
            "/chat/say",
            serde_json::json!({"text": "x".repeat(500)}),
            &anna
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn whispers_go_to_friends_and_wait_in_the_mailbox(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let anna = create_session(&router, "anna").await;
    let boris = create_session(&router, "boris").await;
    create_session(&router, "cyril").await;
    let whisper = |to: &str| serde_json::json!({"to": to, "text": "psst"});

    assert_eq!(
        post_json(&router, "/chat/whisper", whisper("boris"), &anna)
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "not friends yet"
    );
    assert_eq!(
        post_json(
            &router,
            "/friends/requests",
            serde_json::json!({"username": "boris"}),
            &anna
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    let id = response_json(player_request(&router, "GET", "/friends/requests", "", &boris).await)
        .await["incoming"][0]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        post_json(
            &router,
            &format!("/friends/requests/{id}/accept"),
            serde_json::json!({}),
            &boris
        )
        .await
        .status(),
        StatusCode::OK
    );

    // Boris is far away and offline: the whisper still waits for him, for seven days.
    sqlx::query(
        "UPDATE players SET last_seen_at = now() - interval '2 days' WHERE username = 'boris'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        post_json(&router, "/chat/whisper", whisper("BORIS"), &anna)
            .await
            .status(),
        StatusCode::CREATED
    );
    let expires: bool = sqlx::query_scalar("SELECT expires_at BETWEEN now() + interval '6 days' AND now() + interval '7 days 1 minute' FROM player_events WHERE kind = 'whisper'")
        .fetch_one(&pool).await.unwrap();
    assert!(expires);
    let waiting = events(&router, &boris).await;
    assert_eq!(waiting[0]["kind"], "whisper");
    assert_eq!(waiting[0]["subject"], "anna");
    assert_eq!(waiting[0]["body"], "psst");

    // An old, undelivered level-up is dropped, an unexpired whisper is not.
    sqlx::query("UPDATE player_events SET created_at = now() - interval '3 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(events(&router, &boris).await.len(), 1);
    // Delivered once, then forgotten.
    let last = waiting[0]["id"].as_i64().unwrap();
    let after = response_json(
        player_request(&router, "GET", &format!("/events?after={last}"), "", &boris).await,
    )
    .await;
    assert!(after["events"].as_array().unwrap().is_empty());
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM player_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);

    // The mailbox is bounded.
    sqlx::query("INSERT INTO player_events (recipient_id, kind, subject, body, expires_at) SELECT (SELECT id FROM players WHERE username = 'boris'), 'whisper', 'anna', 'x', now() + interval '1 day' FROM generate_series(1, 100)")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE players SET last_chat_at = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        post_json(&router, "/chat/whisper", whisper("boris"), &anna)
            .await
            .status(),
        StatusCode::CONFLICT
    );
}
