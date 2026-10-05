use super::building::{pact_between_worlds, site};
use super::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const SECRET: &str = "test-secret-0123456789";

fn signed(body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn webhook(router: &Router, body: &str, signature: &str) -> Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/payments/webhook")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-ishtaria-signature", signature)
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn pay(router: &Router, order: &str, status: &str, reference: &str) -> Response {
    let body = serde_json::json!({ "order_id": order, "status": status, "reference": reference })
        .to_string();
    webhook(router, &body, &signed(&body)).await
}

async fn rent(
    router: &Router,
    token: &str,
    face: i64,
    from: (i64, i64),
    to: (i64, i64),
) -> Response {
    player_request(
        router,
        "POST",
        "/land/leases",
        &serde_json::json!({ "face": face, "x0": from.0, "y0": from.1, "x1": to.0, "y1": to.1 })
            .to_string(),
        token,
    )
    .await
}

async fn lease_world(pool: &PgPool) -> (Router, i64) {
    std::env::set_var("ISHTARIA_PAYMENT_SECRET", SECRET);
    let mut cfg = config();
    cfg.monetization = Some(
        toml::from_str("enabled = true\ncurrency = \"CZK\"\ntile_price_minor = 1000\ngrace_days = 7\nmax_tiles = 100").unwrap(),
    );
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    pgm.extend(vec![144; 16 * 16 * 6]);
    let id = initialize(pool, &cfg, Some((&pgm, 42))).await.unwrap();
    (
        app(AppState {
            pool: pool.clone(),
            world_id: id,
        }),
        id,
    )
}

async fn tile_of_player(pool: &PgPool, username: &str) -> (i64, i64, i64) {
    let (x, y, z): (f64, f64, f64) = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE username = $1",
    )
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap();
    let (face, column, row) = movement::tile_of([x, y, z]);
    (i64::from(face), i64::from(column), i64::from(row))
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn leases_are_ordered_paid_renewed_and_lapse(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let (router, world_id) = lease_world(&pool).await;
    let prices = response_json(request(&router, "/land/prices").await).await;
    assert_eq!(
        (
            prices["enabled"].clone(),
            prices["tile_price_minor"].clone(),
            prices["grace_days"].clone()
        ),
        (true.into(), "1000".into(), 7.into())
    );
    let tenant = create_session(&router, "tenant").await;
    let rival = create_session(&router, "rival").await;
    // Both players stand at the same place, next to the land they want.
    sqlx::query("UPDATE players SET position_x = t.position_x, position_y = t.position_y, position_z = t.position_z FROM players t WHERE t.username = 'tenant' AND players.username = 'rival'")
        .execute(&pool)
        .await
        .unwrap();
    let (face, column, row) = tile_of_player(&pool, "tenant").await;
    let around = ((column - 1, row - 1), (column + 1, row + 1));

    assert_eq!(
        player_request(
            &router,
            "POST",
            "/land/leases",
            r#"{"face":0,"x0":1,"y0":1,"x1":2,"y1":2}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    for (from, to, status) in [
        ((column + 1, row), (column, row), StatusCode::BAD_REQUEST),
        ((-1, 0), (3, 3), StatusCode::BAD_REQUEST),
        ((column, row), (column + 41, row), StatusCode::BAD_REQUEST),
        (
            (column - 9, row - 9),
            (column + 9, row + 9),
            StatusCode::BAD_REQUEST,
        ),
        (
            (column + 500, row),
            (column + 501, row),
            StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(
            rent(&router, &tenant, face, from, to).await.status(),
            status,
            "{from:?} {to:?}"
        );
    }
    assert_eq!(
        rent(&router, &tenant, (face + 1) % 6, around.0, around.1)
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "another face"
    );

    let ordered = response_json(rent(&router, &tenant, face, around.0, around.1).await).await;
    assert_eq!(ordered["amount_minor"], "9000", "nine tiles at 10.00");
    assert_eq!(ordered["currency"], "CZK");
    assert_eq!(ordered["checkout"]["provider"], "manual");
    let order = ordered["order_id"].as_str().unwrap().to_owned();
    let lease = ordered["lease_id"].as_str().unwrap().to_owned();
    assert_eq!(ordered["checkout"]["reference"], order.as_str());
    let leases =
        response_json(player_request(&router, "GET", "/land/leases", "", &tenant).await).await;
    assert_eq!(
        (
            leases[0]["state"].as_str(),
            leases[0]["tiles"].as_i64(),
            leases[0]["paid_until"].is_null()
        ),
        (Some("pending"), Some(9), true)
    );

    // A pending lease holds its land, and an overlapping order is refused.
    assert_eq!(
        rent(&router, &rival, face, (column, row), (column + 2, row + 2))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let beside = rent(
        &router,
        &rival,
        face,
        (column + 2, row - 1),
        (column + 3, row + 1),
    )
    .await;
    assert_eq!(
        beside.status(),
        StatusCode::CREATED,
        "the next parcel is free"
    );

    // Only the signed callback of the provider confirms a payment.
    let body = serde_json::json!({ "order_id": order, "status": "paid", "reference": "pay-1" })
        .to_string();
    assert_eq!(
        webhook(&router, &body, &"0".repeat(64)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        webhook(&router, &body, "short").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        pay(
            &router,
            "00000000-0000-0000-0000-000000000000",
            "paid",
            "pay-x"
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        pay(&router, &order, "unknown", "pay-1").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        pay(&router, &order, "paid", "").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        player_request(&router, "GET", "/land/leases", "", &tenant)
            .await
            .status(),
        StatusCode::OK
    );
    let confirmed = response_json(pay(&router, &order, "paid", "pay-1").await).await;
    assert_eq!(confirmed["order"], "paid");
    let again = response_json(pay(&router, &order, "paid", "pay-1").await).await;
    assert_eq!(
        again["order"], "paid",
        "a repeated callback changes nothing"
    );
    let leases =
        response_json(player_request(&router, "GET", "/land/leases", "", &tenant).await).await;
    assert_eq!(leases[0]["state"], "active");
    let months: f64 = sqlx::query_scalar("SELECT extract(epoch FROM paid_until - now())::float8 / 86400 FROM land_leases WHERE id = $1::uuid")
        .bind(&lease).fetch_one(&pool).await.unwrap();
    assert!(
        (27.0..=32.0).contains(&months),
        "one month paid, {months} days"
    );
    let rival_order =
        response_json(player_request(&router, "GET", "/land/leases", "", &rival).await).await;
    assert_eq!(rival_order[0]["state"], "pending");
    let other_order: String = sqlx::query_scalar("SELECT id::text FROM payment_orders WHERE player_id = (SELECT id FROM players WHERE username = 'rival')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        pay(&router, &other_order, "paid", "pay-1").await.status(),
        StatusCode::CONFLICT,
        "one payment reference pays one order"
    );
    assert_eq!(
        pay(&router, &order, "failed", "pay-1").await.status(),
        StatusCode::CONFLICT,
        "a paid order cannot fail"
    );

    // Only the tenant may build on the land, through the grace period.
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM players ORDER BY username")
        .fetch_all(&pool)
        .await
        .unwrap();
    let (rival_id, tenant_id) = (ids[0], ids[1]);
    let position: [f64; 3] = {
        let (x, y, z): (f64, f64, f64) = sqlx::query_as(
            "SELECT position_x, position_y, position_z FROM players WHERE username = 'tenant'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        [x, y, z]
    };
    let check = |player: i64| {
        let pool = pool.clone();
        async move {
            let mut transaction = pool.begin().await.unwrap();
            let result = land::require_tenant(&mut transaction, world_id, player, position).await;
            transaction.rollback().await.unwrap();
            result
        }
    };
    assert!(check(tenant_id).await.is_ok());
    assert!(matches!(
        check(rival_id).await,
        Err(players::Error::Status(StatusCode::FORBIDDEN, _))
    ));
    sqlx::query(
        "UPDATE land_leases SET paid_until = now() - interval '3 days' WHERE id = $1::uuid",
    )
    .bind(&lease)
    .execute(&pool)
    .await
    .unwrap();
    let leases =
        response_json(player_request(&router, "GET", "/land/leases", "", &tenant).await).await;
    assert_eq!(leases[0]["state"], "grace");
    assert!(
        check(rival_id).await.is_err(),
        "the right lasts through the grace period"
    );
    let renewal = response_json(
        player_request(
            &router,
            "POST",
            &format!("/land/leases/{lease}/renew"),
            "",
            &tenant,
        )
        .await,
    )
    .await;
    assert_eq!(renewal["amount_minor"], "9000");
    let renew_order = renewal["order_id"].as_str().unwrap();
    assert_eq!(
        player_request(
            &router,
            "POST",
            &format!("/land/leases/{lease}/renew"),
            "",
            &rival
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        pay(&router, renew_order, "paid", "pay-2").await.status(),
        StatusCode::OK
    );
    let months: f64 = sqlx::query_scalar("SELECT extract(epoch FROM paid_until - now())::float8 / 86400 FROM land_leases WHERE id = $1::uuid")
        .bind(&lease).fetch_one(&pool).await.unwrap();
    assert!(
        (26.0..=29.5).contains(&months),
        "renewing continues from the old end: {months} days"
    );

    sqlx::query(
        "UPDATE land_leases SET paid_until = now() - interval '8 days' WHERE id = $1::uuid",
    )
    .bind(&lease)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        check(rival_id).await.is_ok(),
        "a lapsed lease ends the exclusive right"
    );
    assert_eq!(
        player_request(
            &router,
            "POST",
            &format!("/land/leases/{lease}/renew"),
            "",
            &tenant
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let free = rent(&router, &rival, face, around.0, around.1).await;
    assert_eq!(
        free.status(),
        StatusCode::CREATED,
        "lapsed land can be leased again"
    );

    // A refund ends a lease at once.
    sqlx::query(
        "UPDATE land_leases SET paid_until = now() + interval '10 days' WHERE id = $1::uuid",
    )
    .bind(&lease)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        pay(&router, &order, "refunded", "pay-1").await.status(),
        StatusCode::OK
    );
    assert!(check(rival_id).await.is_ok());

    // Disabled worlds neither sell land nor accept payments.
    sqlx::query("UPDATE monetization_settings SET enabled = false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        rent(
            &router,
            &tenant,
            face,
            (column + 20, row),
            (column + 21, row)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        pay(&router, &order, "paid", "pay-3").await.status(),
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn building_a_portal_on_rented_land_needs_the_lease(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = pact_between_worlds(&pool, "brana-sever", "brana-jih").await;
    let landlord = create_session(&p.a.router, "landlord").await;
    let _ = landlord;
    let (face, column, row) = tile_of_player(&pool, "vitex").await;
    sqlx::query("UPDATE monetization_settings SET enabled = true WHERE world_id = $1")
        .bind(p.a.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO land_leases (world_id, player_id, face, x0, y0, x1, y1, tile_price_minor, currency, paid_until) SELECT $1, id, $2, $3, $4, $5, $6, 1000, 'CZK', now() + interval '10 days' FROM players WHERE username = 'landlord'")
        .bind(p.a.id).bind(face as i32).bind(column as i32 - 2).bind(row as i32 - 2).bind(column as i32 + 2).bind(row as i32 + 2)
        .execute(&pool).await.unwrap();
    assert_eq!(
        site(&p.a, &p.vitex, &p.pact_a).await.status(),
        StatusCode::FORBIDDEN,
        "someone else's land"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM portal_pacts WHERE id = $1::uuid")
        .bind(&p.pact_a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "accepted", "nothing was built");
    sqlx::query("UPDATE land_leases SET paid_until = now() - interval '9 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        site(&p.a, &p.vitex, &p.pact_a).await.status(),
        StatusCode::OK,
        "the lease has lapsed"
    );
}
