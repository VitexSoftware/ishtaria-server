use super::*;
use crate::federation::{PeerDirectory, ServerInfo};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// In-process stand-in for the network: routes peer requests to the other world's router.
#[derive(Default)]
pub(super) struct Network {
    pub(super) routes: Mutex<HashMap<String, Router>>,
    /// Addresses to which status messages can currently be delivered.
    pub(super) reachable: Mutex<std::collections::HashSet<String>>,
}

impl Network {
    fn router(&self, api_url: &str) -> Result<Router, players::Error> {
        self.routes
            .lock()
            .unwrap()
            .get(api_url)
            .cloned()
            .ok_or(players::Error::Status(
                StatusCode::BAD_GATEWAY,
                "peer unreachable",
            ))
    }
}

type PeerFuture<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, players::Error>> + Send>>;

impl PeerDirectory for Network {
    fn server_info(&self, api_url: String, _allow_private: bool) -> PeerFuture<ServerInfo> {
        let router = self.router(&api_url);
        Box::pin(async move {
            let response = request(&router?, "/.well-known/ishtaria/server.json").await;
            if response.status() != StatusCode::OK {
                return Err(players::Error::Status(
                    StatusCode::BAD_GATEWAY,
                    "peer unreachable",
                ));
            }
            serde_json::from_value(response_json(response).await)
                .map_err(|_| players::Error::Status(StatusCode::BAD_GATEWAY, "invalid peer info"))
        })
    }

    fn post_accept(
        &self,
        api_url: String,
        message: String,
        _allow_private: bool,
    ) -> PeerFuture<()> {
        let router = self.router(&api_url);
        Box::pin(async move {
            let body = serde_json::json!({ "message": message }).to_string();
            let response = player_request(&router?, "POST", "/federation/pacts", &body, "").await;
            match response.status() {
                status if status.is_success() => Ok(()),
                status => Err(players::Error::Status(status, "peer rejected the pact")),
            }
        })
    }

    fn post_status(
        &self,
        api_url: String,
        message: String,
        _allow_private: bool,
    ) -> PeerFuture<()> {
        let router = self.router(&api_url);
        let reachable = self.reachable.lock().unwrap().contains(&api_url);
        Box::pin(async move {
            if !reachable {
                return Err(players::Error::Status(
                    StatusCode::BAD_GATEWAY,
                    "peer unreachable",
                ));
            }
            let body = serde_json::json!({ "message": message }).to_string();
            let response =
                player_request(&router?, "POST", "/federation/pacts/status", &body, "").await;
            match response.status() {
                status if status.is_success() => Ok(()),
                status => Err(players::Error::Status(status, "peer rejected the status")),
            }
        })
    }
}

pub(super) struct World {
    pub(super) router: Router,
    pub(super) id: i64,
    pub(super) url: String,
}

fn world_config(name: &str, policy: &str) -> Config {
    let mut cfg = config();
    cfg.server_name = format!("{name}.example.org");
    cfg.public_url = Some(format!("https://{name}.example.org:7400"));
    cfg.federation = Some(FederationConfig {
        policy: Some(policy.into()),
        allow_private_peers: None,
    });
    cfg
}

pub(super) async fn world(
    pool: &PgPool,
    network: &Arc<Network>,
    name: &str,
    policy: &str,
) -> World {
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    pgm.extend(vec![144; 16 * 16 * 6]);
    let cfg = world_config(name, policy);
    let id = initialize(pool, &cfg, Some((&pgm, 42))).await.unwrap();
    let directory: Arc<dyn PeerDirectory> = network.clone();
    let router = app(AppState {
        pool: pool.clone(),
        world_id: id,
    })
    .layer(Extension(directory));
    let url = cfg.public_url.unwrap();
    network.reachable.lock().unwrap().insert(url.clone());
    network
        .routes
        .lock()
        .unwrap()
        .insert(url.clone(), router.clone());
    World { router, id, url }
}

async fn invite(world: &World, token: &str, portal: &str) -> Response {
    player_request(
        &world.router,
        "POST",
        "/portals/invitations",
        &format!(r#"{{"portal_name":"{portal}"}}"#),
        token,
    )
    .await
}

pub(super) async fn accept(world: &World, token: &str, code: &str, portal: &str) -> Response {
    player_request(
        &world.router,
        "POST",
        "/portals/pacts",
        &serde_json::json!({ "code": code, "portal_name": portal }).to_string(),
        token,
    )
    .await
}

pub(super) async fn invitation_code(world: &World, token: &str, portal: &str) -> (String, String) {
    let response = invite(world, token, portal).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = response_json(response).await;
    (
        body["code"].as_str().unwrap().to_owned(),
        body["id"].as_str().unwrap().to_owned(),
    )
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn invitation_creates_a_pact_on_both_worlds(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let network = Arc::new(Network::default());
    let a = world(&pool, &network, "svet-a", "approve").await;
    let b = world(&pool, &network, "svet-b", "open").await;
    let vitex = create_session(&a.router, "vitex").await;
    let anna = create_session(&b.router, "anna").await;

    let info = response_json(request(&a.router, "/.well-known/ishtaria/server.json").await).await;
    assert_eq!(info["server_name"], "svet-a.example.org");
    assert_eq!(info["federation_policy"], "approve");
    assert!(info["public_key"].as_str().unwrap().starts_with("ed25519:"));

    let (code, invitation_id) = invitation_code(&a, &vitex, "brana-sever").await;
    assert!(code.starts_with("ishtaria-invite:v1."));
    assert_eq!(
        invite(&a, &vitex, "brana-sever").await.status(),
        StatusCode::CONFLICT,
        "an open invitation reserves its portal name"
    );
    assert_eq!(
        player_request(&a.router, "GET", "/portals/pacts", "", &vitex)
            .await
            .status(),
        StatusCode::OK
    );

    let response = accept(&b, &anna, &code, "brana-jih").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let invitee = response_json(response).await;
    assert_eq!(invitee["role"], "invitee");
    assert_eq!(
        invitee["state"], "accepted",
        "open policy accepts immediately"
    );
    assert_eq!(invitee["peer_host"], "svet-a.example.org");
    assert_eq!(invitee["peer_player"], "vitex");
    assert_eq!(invitee["portal_name"], "brana-jih");
    assert_eq!(invitee["peer_portal_name"], "brana-sever");

    let inviter =
        response_json(player_request(&a.router, "GET", "/portals/pacts", "", &vitex).await).await;
    assert_eq!(inviter.as_array().unwrap().len(), 1);
    assert_eq!(inviter[0]["role"], "inviter");
    assert_eq!(
        inviter[0]["state"], "proposed",
        "approve policy waits for the operator"
    );
    assert_eq!(inviter[0]["peer_host"], "svet-b.example.org");
    assert_eq!(inviter[0]["peer_player"], "anna");
    assert_eq!(inviter[0]["portal_name"], "brana-sever");
    assert_eq!(inviter[0]["peer_portal_name"], "brana-jih");

    let again = accept(&b, &anna, &code, "brana-jih").await;
    assert_eq!(
        again.status(),
        StatusCode::OK,
        "repeating the acceptance is idempotent"
    );
    assert_eq!(response_json(again).await["id"], invitee["id"]);
    let pacts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM portal_pacts WHERE invitation_id = $1::uuid")
            .bind(&invitation_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pacts, 2, "one pact per world");

    let bob = create_session(&b.router, "bob").await;
    assert_eq!(
        accept(&b, &bob, &code, "brana-bob").await.status(),
        StatusCode::CONFLICT,
        "an invitation is single use"
    );
    let own = accept(&a, &vitex, &code, "brana-own").await;
    assert_eq!(
        own.status(),
        StatusCode::BAD_REQUEST,
        "no pact within one world"
    );

    for (world_id, host) in [(a.id, "svet-b.example.org"), (b.id, "svet-a.example.org")] {
        let pinned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM federation_peers WHERE world_id = $1 AND host = $2",
        )
        .bind(world_id)
        .bind(host)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pinned, 1, "{host} key is pinned on first contact");
    }
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn forged_expired_revoked_and_closed_invitations_are_refused(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let network = Arc::new(Network::default());
    let a = world(&pool, &network, "svet-a", "approve").await;
    let b = world(&pool, &network, "svet-b", "approve").await;
    let vitex = create_session(&a.router, "vitex").await;
    let anna = create_session(&b.router, "anna").await;

    let (code, _) = invitation_code(&a, &vitex, "brana-sever").await;
    let (payload, signature) = code
        .strip_prefix("ishtaria-invite:v1.")
        .unwrap()
        .split_once('.')
        .unwrap();
    let changed = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        String::from_utf8(
            base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
                .unwrap(),
        )
        .unwrap()
        .replace("brana-sever", "brana-zlo"),
    );
    for forged in [
        format!("ishtaria-invite:v1.{changed}.{signature}"),
        format!("ishtaria-invite:v1.{payload}.{}", "A".repeat(86)),
        "ishtaria-invite:v1.not-a-code".to_owned(),
        "garbage".to_owned(),
    ] {
        let status = accept(&b, &anna, &forged, "brana-jih").await.status();
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::BAD_REQUEST,
            "forged code must be refused, got {status}"
        );
    }
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_pacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(none, 0);

    sqlx::query("UPDATE portal_invitations SET created_at = now() - interval '2 days', expires_at = now() - interval '1 day'")
        .execute(&pool)
        .await
        .unwrap();
    // The signed payload still says valid: the issuing world must refuse on its own clock.
    assert_eq!(
        accept(&b, &anna, &code, "brana-jih").await.status(),
        StatusCode::GONE
    );

    let (revoked_code, revoked_id) = invitation_code(&a, &vitex, "brana-revoked").await;
    assert_eq!(
        player_request(
            &a.router,
            "DELETE",
            &format!("/portals/invitations/{revoked_id}"),
            "",
            &vitex
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        accept(&b, &anna, &revoked_code, "brana-jih").await.status(),
        StatusCode::GONE
    );

    let (closed_code, _) = invitation_code(&a, &vitex, "brana-closed").await;
    sqlx::query("UPDATE federation_settings SET policy = 'closed' WHERE world_id = $1")
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        invite(&a, &vitex, "brana-new").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        accept(&b, &anna, &closed_code, "brana-jih").await.status(),
        StatusCode::FORBIDDEN,
        "a world that closed federation refuses acceptances"
    );
    sqlx::query("UPDATE federation_settings SET policy = 'approve' WHERE world_id = $1")
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();

    // A changed key of an already pinned peer is never trusted again.
    assert_eq!(
        accept(&b, &anna, &closed_code, "brana-jih").await.status(),
        StatusCode::CREATED
    );
    sqlx::query("UPDATE federation_peers SET public_key = $1 WHERE world_id = $2")
        .bind(vec![9u8; 32])
        .bind(b.id)
        .execute(&pool)
        .await
        .unwrap();
    let (rotated, _) = invitation_code(&a, &vitex, "brana-rotated").await;
    let status = accept(&b, &anna, &rotated, "brana-other").await.status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::UNPROCESSABLE_ENTITY,
        "pinned key mismatch must fail, got {status}"
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn invitation_limits_and_inputs_are_bounded(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let network = Arc::new(Network::default());
    let a = world(&pool, &network, "svet-a", "approve").await;
    let vitex = create_session(&a.router, "vitex").await;
    assert_eq!(
        player_request(
            &a.router,
            "POST",
            "/portals/invitations",
            r#"{"portal_name":"x"}"#,
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    for invalid in ["", "UPPER", "with space", &"a".repeat(65), "../x"] {
        assert_eq!(
            invite(&a, &vitex, invalid).await.status(),
            StatusCode::BAD_REQUEST,
            "{invalid:?}"
        );
    }
    for index in 0..3 {
        invitation_code(&a, &vitex, &format!("brana-{index}")).await;
    }
    assert_eq!(
        invite(&a, &vitex, "brana-4").await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let oversized = format!("ishtaria-invite:v1.{}", "A".repeat(3000));
    assert_eq!(
        accept(&a, &vitex, &oversized, "brana-x").await.status(),
        StatusCode::BAD_REQUEST
    );
    let unsigned = player_request(
        &a.router,
        "POST",
        "/federation/pacts",
        r#"{"message":"x.y"}"#,
        "",
    )
    .await;
    assert!(
        unsigned.status() == StatusCode::BAD_REQUEST
            || unsigned.status() == StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        player_request(
            &a.router,
            "POST",
            "/federation/pacts",
            &format!(r#"{{"message":"{}"}}"#, "A".repeat(5000)),
            ""
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // Without a public URL the world cannot sign invitations others could answer.
    let mut unconfigured = config();
    unconfigured.server_name = "svet-c.example.org".into();
    let c = initialize(&pool, &unconfigured, None).await.unwrap();
    let federation: Option<String> =
        sqlx::query_scalar("SELECT public_url FROM federation_settings WHERE world_id = $1")
            .bind(c)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(federation.is_none());
    let mut invalid_url = world_config("svet-d", "approve");
    invalid_url.public_url = Some("ftp://svet-d.example.org".into());
    assert!(initialize(&pool, &invalid_url, None).await.is_err());
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_server_cannot_claim_another_worlds_name(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let network = Arc::new(Network::default());
    // The impostor reports the name svet-a but is served from evil.example.org.
    let mut pgm = b"P5\n96 16\n255\n".to_vec();
    pgm.extend(vec![144; 16 * 16 * 6]);
    let mut cfg = world_config("svet-a", "approve");
    cfg.public_url = Some("https://evil.example.org:7400".into());
    let id = initialize(&pool, &cfg, Some((&pgm, 42))).await.unwrap();
    let directory: Arc<dyn PeerDirectory> = network.clone();
    let router = app(AppState {
        pool: pool.clone(),
        world_id: id,
    })
    .layer(Extension(directory));
    network
        .routes
        .lock()
        .unwrap()
        .insert("https://evil.example.org:7400".into(), router.clone());
    let impostor = World {
        router,
        id,
        url: "https://evil.example.org:7400".into(),
    };
    let b = world(&pool, &network, "svet-b", "approve").await;
    let mallory = create_session(&impostor.router, "mallory").await;
    let anna = create_session(&b.router, "anna").await;
    let (code, _) = invitation_code(&impostor, &mallory, "brana-sever").await;
    assert_eq!(
        accept(&b, &anna, &code, "brana-jih").await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let pinned: i64 = sqlx::query_scalar("SELECT count(*) FROM federation_peers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pinned, 0, "nothing is pinned for an unverified claim");
}
