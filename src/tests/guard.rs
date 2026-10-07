use super::*;
use crate::guard::{Guard, LimitsConfig};
use axum::extract::ConnectInfo;
use std::{net::SocketAddr, sync::Arc};

fn guarded(pool: &PgPool, world_id: i64, limits: LimitsConfig) -> Router {
    app(AppState {
        pool: pool.clone(),
        world_id,
    })
    .layer(axum::middleware::from_fn(crate::guard::middleware))
    .layer(Extension(Arc::new(Guard::new(limits))))
}

async fn from(
    router: &Router,
    peer: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: String,
) -> Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::from(body)).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    router.clone().oneshot(request).await.unwrap()
}

fn registration(name: &str) -> String {
    format!(r#"{{"username":"{name}","password":"test-password"}}"#)
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn registration_and_login_are_limited_per_address(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = guarded(
        &pool,
        world_id,
        LimitsConfig {
            auth_per_minute: 3,
            ..LimitsConfig::default()
        },
    );

    for name in ["anna", "boris", "cyril"] {
        let response = from(
            &router,
            "203.0.113.5:4000",
            "POST",
            "/players",
            &[],
            registration(name),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "{name}");
    }
    let refused = from(
        &router,
        "203.0.113.5:4001",
        "POST",
        "/players",
        &[],
        registration("dana"),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        refused
            .headers()
            .get(header::RETRY_AFTER)
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    let login = from(
        &router,
        "203.0.113.5:4002",
        "POST",
        "/players/login",
        &[],
        registration("anna"),
    )
    .await;
    assert_eq!(
        login.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "login shares the allowance of registration"
    );
    let nobody: i64 = sqlx::query_scalar("SELECT count(*) FROM players WHERE username = 'dana'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(nobody, 0, "a refused request did nothing");

    // Another address is not affected, and other routes keep their own, larger allowance.
    let other = from(
        &router,
        "198.51.100.7:4000",
        "POST",
        "/players",
        &[],
        registration("dana"),
    )
    .await;
    assert_eq!(other.status(), StatusCode::CREATED);
    for _ in 0..20 {
        assert_eq!(
            from(
                &router,
                "203.0.113.5:4000",
                "GET",
                "/health",
                &[],
                String::new()
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    let world = from(
        &router,
        "203.0.113.5:4000",
        "GET",
        "/world",
        &[],
        String::new(),
    )
    .await;
    assert_eq!(world.status(), StatusCode::OK);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn every_request_is_limited_in_total_and_the_proxy_address_counts_only_when_trusted(
    pool: PgPool,
) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let small = LimitsConfig {
        requests_per_second: 1,
        burst: 5,
        ..LimitsConfig::default()
    };
    let router = guarded(&pool, world_id, small.clone());
    let mut statuses = Vec::new();
    for _ in 0..9 {
        statuses.push(
            from(
                &router,
                "203.0.113.9:1",
                "GET",
                "/world",
                &[],
                String::new(),
            )
            .await
            .status(),
        );
    }
    let allowed = statuses
        .iter()
        .filter(|status| **status == StatusCode::OK)
        .count();
    assert!(
        (5..=6).contains(&allowed),
        "a burst of 5, and perhaps one refilled token: {statuses:?}"
    );
    assert_eq!(*statuses.last().unwrap(), StatusCode::TOO_MANY_REQUESTS);

    // Without trust the header is ignored: the same peer is still limited whatever it claims.
    let claimed = from(
        &router,
        "203.0.113.9:1",
        "GET",
        "/world",
        &[("x-forwarded-for", "192.0.2.1")],
        String::new(),
    )
    .await;
    assert_eq!(claimed.status(), StatusCode::TOO_MANY_REQUESTS);
    // Behind a proxy of the operator the last entry is the client, so two clients are told apart.
    let proxied = guarded(
        &pool,
        world_id,
        LimitsConfig {
            trust_proxy: true,
            ..small
        },
    );
    for _ in 0..5 {
        from(
            &proxied,
            "10.0.0.1:1",
            "GET",
            "/world",
            &[("x-forwarded-for", "198.51.100.1, 192.0.2.10")],
            String::new(),
        )
        .await;
    }
    let same = from(
        &proxied,
        "10.0.0.1:1",
        "GET",
        "/world",
        &[("x-forwarded-for", "1.2.3.4, 192.0.2.10")],
        String::new(),
    )
    .await;
    assert_eq!(
        same.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a client cannot rename itself with an earlier entry"
    );
    let another = from(
        &proxied,
        "10.0.0.1:1",
        "GET",
        "/world",
        &[("x-forwarded-for", "192.0.2.77")],
        String::new(),
    )
    .await;
    assert_eq!(
        another.status(),
        StatusCode::OK,
        "another client behind the same proxy"
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn metrics_are_for_the_operator_only(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let plain = guarded(
        &pool,
        world_id,
        LimitsConfig {
            auth_per_minute: 1,
            ..LimitsConfig::default()
        },
    );
    let remote = "203.0.113.5:9";

    from(
        &plain,
        remote,
        "POST",
        "/players",
        &[],
        registration("anna"),
    )
    .await;
    from(
        &plain,
        remote,
        "POST",
        "/players",
        &[],
        registration("boris"),
    )
    .await;
    assert_eq!(
        from(&plain, remote, "GET", "/metrics", &[], String::new())
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "a stranger does not see that it exists"
    );
    let text =
        response_text(from(&plain, "127.0.0.1:9", "GET", "/metrics", &[], String::new()).await)
            .await;
    assert!(
        text.contains("ishtaria_http_rate_limited_total 1"),
        "{text}"
    );
    assert!(
        text.contains("ishtaria_http_requests_total{class=\"2xx\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("ishtaria_http_requests_by_group_total{group=\"players\"} 2"),
        "{text}"
    );
    assert!(text.contains("ishtaria_players_alive 1"), "{text}");
    assert!(text.contains("ishtaria_db_pool_connections{state=\"open\"}"));
    assert!(text.contains(&format!(
        "ishtaria_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(
        !text.contains("anna") && !text.contains("test-password"),
        "no player data in the metrics"
    );

    // A remote scraper needs the token; the wrong one and a header without the word Bearer do not work.
    let secured = guarded(
        &pool,
        world_id,
        LimitsConfig {
            metrics_token: Some("scrape-me".into()),
            ..LimitsConfig::default()
        },
    );
    assert_eq!(
        from(
            &secured,
            remote,
            "GET",
            "/metrics",
            &[("authorization", "Bearer scrape-me")],
            String::new()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        from(
            &secured,
            remote,
            "GET",
            "/metrics",
            &[("authorization", "Bearer scrape-yo")],
            String::new()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        from(
            &secured,
            remote,
            "GET",
            "/metrics",
            &[("authorization", "scrape-me")],
            String::new()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    // Behind a proxy every request seems to come from loopback, so loopback alone proves nothing.
    let proxied = guarded(
        &pool,
        world_id,
        LimitsConfig {
            trust_proxy: true,
            ..LimitsConfig::default()
        },
    );
    assert_eq!(
        from(
            &proxied,
            "127.0.0.1:9",
            "GET",
            "/metrics",
            &[],
            String::new()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    // Without a guard (as in every other test) there is no such route.
    let bare = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    assert_eq!(
        request(&bare, "/metrics").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn oversized_bodies_are_refused(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    let huge = format!(
        r#"{{"username":"anna","password":"{}"}}"#,
        "x".repeat(70_000)
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/players")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(huge))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn response_text(response: Response) -> String {
    assert!(response.status().is_success(), "{}", response.status());
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_burst_of_registrations_waits_for_the_password_hashing(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let router = app(AppState {
        pool: pool.clone(),
        world_id,
    });
    // Only two passwords are hashed at a time; the other requests wait their turn instead of failing.
    let tasks: Vec<_> = (0..8)
        .map(|index| {
            let router = router.clone();
            tokio::spawn(async move {
                from(
                    &router,
                    "203.0.113.5:4000",
                    "POST",
                    "/players",
                    &[],
                    registration(&format!("burst{index}")),
                )
                .await
                .status()
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::CREATED);
    }
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 8);
}
