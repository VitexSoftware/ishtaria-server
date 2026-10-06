use super::*;

async fn post_json(router: &Router, path: &str, body: &str, token: &str) -> Response {
    player_request(router, "POST", path, body, token).await
}

async fn befriend(router: &Router, first: &str, second: &str, second_name: &str) {
    assert_eq!(
        post_json(
            router,
            "/friends/requests",
            &serde_json::json!({ "username": second_name }).to_string(),
            first
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    let requests =
        response_json(player_request(router, "GET", "/friends/requests", "", second).await).await;
    let id = requests["incoming"][0]["id"].as_i64().unwrap();
    assert_eq!(
        post_json(
            router,
            &format!("/friends/requests/{id}/accept"),
            "",
            second
        )
        .await
        .status(),
        StatusCode::OK
    );
}

async fn poll(router: &Router, token: &str, after: i64) -> serde_json::Value {
    response_json(player_request(router, "GET", &format!("/events?after={after}"), "", token).await)
        .await
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn friendships_need_a_request_and_an_answer(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let anna = create_session(&router, "anna").await;
    let boris = create_session(&router, "boris").await;
    let cyril = create_session(&router, "cyril").await;

    let request = |name: &str| serde_json::json!({ "username": name }).to_string();
    assert_eq!(
        post_json(&router, "/friends/requests", &request("anna"), &anna)
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "not oneself"
    );
    assert_eq!(
        post_json(&router, "/friends/requests", &request("nobody"), &anna)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_json(&router, "/friends/requests", &request(""), &anna)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_json(&router, "/friends/requests", &request("boris"), "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_json(&router, "/friends/requests", &request("BORIS"), &anna)
            .await
            .status(),
        StatusCode::CREATED,
        "names are not case sensitive"
    );
    assert_eq!(
        post_json(&router, "/friends/requests", &request("boris"), &anna)
            .await
            .status(),
        StatusCode::CREATED,
        "asking twice changes nothing"
    );
    let waiting =
        response_json(player_request(&router, "GET", "/friends/requests", "", &boris).await).await;
    assert_eq!(waiting["incoming"].as_array().unwrap().len(), 1);
    assert_eq!(waiting["incoming"][0]["name"], "anna");
    let sent =
        response_json(player_request(&router, "GET", "/friends/requests", "", &anna).await).await;
    assert_eq!(sent["outgoing"][0]["name"], "boris");
    let none: Vec<serde_json::Value> =
        response_json(player_request(&router, "GET", "/friends", "", &anna).await)
            .await
            .as_array()
            .unwrap()
            .clone();
    assert!(none.is_empty(), "a request is not a friendship yet");

    // Only the addressee can accept, and a stranger's request id is not found.
    let id = waiting["incoming"][0]["id"].as_i64().unwrap();
    assert_eq!(
        post_json(
            &router,
            &format!("/friends/requests/{id}/accept"),
            "",
            &anna
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_json(
            &router,
            &format!("/friends/requests/{id}/accept"),
            "",
            &cyril
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_json(
            &router,
            &format!("/friends/requests/{id}/accept"),
            "",
            &boris
        )
        .await
        .status(),
        StatusCode::OK
    );

    // Friends see each other's name, level, world and whether they are online.
    let friends = response_json(player_request(&router, "GET", "/friends", "", &anna).await).await;
    assert_eq!(friends[0]["name"], "boris");
    assert_eq!(friends[0]["world"], "test.ishtaria.example.org");
    assert_eq!(friends[0]["level"], 1);
    assert_eq!(friends[0]["online"], false, "boris has not polled yet");
    poll(&router, &boris, 0).await;
    let friends = response_json(player_request(&router, "GET", "/friends", "", &anna).await).await;
    assert_eq!(
        friends[0]["online"], true,
        "polling shows a player as online"
    );
    sqlx::query("UPDATE players SET last_seen_at = now() - interval '5 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    let friends = response_json(player_request(&router, "GET", "/friends", "", &anna).await).await;
    assert_eq!(
        friends[0]["online"], false,
        "not polling for a while means offline"
    );

    assert_eq!(
        post_json(&router, "/friends/requests", &request("boris"), &anna)
            .await
            .status(),
        StatusCode::CONFLICT,
        "already friends"
    );
    // A request that was answered by a request the other way becomes a friendship at once.
    post_json(&router, "/friends/requests", &request("cyril"), &anna).await;
    let mutual = post_json(&router, "/friends/requests", &request("anna"), &cyril).await;
    assert_eq!(mutual.status(), StatusCode::OK);
    assert_eq!(response_json(mutual).await["status"], "friends");
    let friends = response_json(player_request(&router, "GET", "/friends", "", &anna).await).await;
    assert_eq!(friends.as_array().unwrap().len(), 2);

    // Declining, cancelling and removing.
    sqlx::query("DELETE FROM player_friendships")
        .execute(&pool)
        .await
        .unwrap();
    post_json(&router, "/friends/requests", &request("boris"), &anna).await;
    let id = response_json(player_request(&router, "GET", "/friends/requests", "", &boris).await)
        .await["incoming"][0]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        player_request(
            &router,
            "DELETE",
            &format!("/friends/requests/{id}"),
            "",
            &cyril
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        player_request(
            &router,
            "DELETE",
            &format!("/friends/requests/{id}"),
            "",
            &boris
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        player_request(
            &router,
            "DELETE",
            &format!("/friends/requests/{id}"),
            "",
            &boris
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    befriend(&router, &anna, &boris, "boris").await;
    assert_eq!(
        player_request(&router, "DELETE", "/friends/cyril", "", &anna)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        player_request(&router, "DELETE", "/friends/Boris", "", &anna)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let friends = response_json(player_request(&router, "GET", "/friends", "", &boris).await).await;
    assert!(
        friends.as_array().unwrap().is_empty(),
        "removing ends it for both"
    );

    // The number of pending requests and of friends is limited.
    for index in 0..21 {
        sqlx::query("INSERT INTO players (world_id, username, password_hash) VALUES ($1, $2, 'x')")
            .bind(world_id)
            .bind(format!("extra-{index:02}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut statuses = Vec::new();
    for index in 0..21 {
        statuses.push(
            post_json(
                &router,
                "/friends/requests",
                &request(&format!("extra-{index:02}")),
                &cyril,
            )
            .await
            .status(),
        );
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        20
    );
    assert_eq!(statuses[20], StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_level_up_restores_the_player_and_tells_the_friends_once(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let hero = create_session(&router, "hero").await;
    let friend = create_session(&router, "friend").await;
    let stranger = create_session(&router, "stranger").await;
    befriend(&router, &hero, &friend, "friend").await;
    // Nothing has happened yet.
    assert_eq!(
        poll(&router, &friend, 0).await["events"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &hero).await).await;
    assert_eq!(profile["stats"]["score"], "100", "level 1 on the first day");
    sqlx::query("UPDATE players SET experience = 49, health = 10, stamina = 5, water = 20 WHERE username = 'hero'")
        .execute(&pool).await.unwrap();
    super::building::give(&pool, "hero", &[("oak_log", 1)]).await;
    let crafted = response_json(
        post_json(
            &router,
            "/players/me/craft",
            r#"{"recipe":"chop_oak_log","count":"1"}"#,
            &hero,
        )
        .await,
    )
    .await;
    assert_eq!(crafted["stats"]["level"], 2);
    for meter in ["health", "stamina", "water"] {
        assert_eq!(crafted["stats"][meter], 100, "{meter} is restored");
    }
    assert_eq!(crafted["stats"]["score"], "200");
    assert_eq!(
        crafted["inventory"]["capacity"], 110,
        "the level improves the inventory"
    );
    // Experience gained without a new level restores nothing.
    sqlx::query("UPDATE players SET health = 10 WHERE username = 'hero'")
        .execute(&pool)
        .await
        .unwrap();
    super::building::give(&pool, "hero", &[("oak_log", 1)]).await;
    let again = response_json(
        post_json(
            &router,
            "/players/me/craft",
            r#"{"recipe":"chop_oak_log","count":"1"}"#,
            &hero,
        )
        .await,
    )
    .await;
    assert_eq!(again["stats"]["level"], 2);
    assert!(again["stats"]["health"].as_i64().unwrap() < 100);

    // The friend is told once; the stranger and the hero are not.
    let told = poll(&router, &friend, 0).await;
    let events = told["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0]["kind"].as_str(),
            events[0]["subject"].as_str(),
            events[0]["level"].as_i64()
        ),
        (Some("level_up"), Some("hero"), Some(2))
    );
    let last = told["last"].as_i64().unwrap();
    assert_eq!(
        poll(&router, &friend, last).await["events"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "an event is delivered once"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM player_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "the server forgets what has been delivered");
    assert!(poll(&router, &stranger, 0).await["events"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(poll(&router, &hero, 0).await["events"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        player_request(&router, "GET", "/events?after=-1", "", &friend)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        player_request(&router, "GET", "/events", "", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Events nobody fetches are dropped after a while.
    sqlx::query("INSERT INTO player_events (recipient_id, kind, subject, level, created_at) SELECT id, 'level_up', 'hero', 9, now() - interval '10 minutes' FROM players WHERE username = 'friend'")
        .execute(&pool).await.unwrap();
    assert!(poll(&router, &friend, 0).await["events"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_chosen_flag_is_shown_to_friends_and_is_only_a_picture(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let anna = create_session(&router, "anna").await;
    let boris = create_session(&router, "boris").await;
    befriend(&router, &anna, &boris, "boris").await;

    // Nobody shows a flag until they choose to.
    let me = response_json(player_request(&router, "GET", "/players/me", "", &anna).await).await;
    assert!(me["flag"].is_null());
    let put = |token: String, body: serde_json::Value| {
        let router = router.clone();
        async move {
            player_request(
                &router,
                "PUT",
                "/players/me/flag",
                &body.to_string(),
                &token,
            )
            .await
        }
    };
    for bad in ["cz", "CZE", "C1", "", "<script>"] {
        assert_eq!(
            put(anna.clone(), serde_json::json!({ "flag": bad }))
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{bad:?} is no flag"
        );
    }
    assert_eq!(
        put(anna.clone(), serde_json::json!({ "language": "cs" }))
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the request carries no language"
    );
    assert_eq!(
        put(String::new(), serde_json::json!({ "flag": "CZ" }))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let shown = response_json(put(anna.clone(), serde_json::json!({ "flag": "CZ" })).await).await;
    assert_eq!(shown["flag"], "CZ");
    let friends = response_json(player_request(&router, "GET", "/friends", "", &boris).await).await;
    assert_eq!(friends[0]["name"], "anna");
    assert_eq!(friends[0]["flag"], "CZ");

    // Hiding it again removes it for everyone.
    let hidden = response_json(put(anna.clone(), serde_json::json!({ "flag": null })).await).await;
    assert!(hidden["flag"].is_null());
    let friends = response_json(player_request(&router, "GET", "/friends", "", &boris).await).await;
    assert!(friends[0]["flag"].is_null());
}
