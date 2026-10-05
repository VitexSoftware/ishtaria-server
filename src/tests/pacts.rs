//! Linking portals of two worlds by a share link: harness and tests.

use super::building::{finish_portal, give, two_worlds, Pair};
use super::*;
use crate::federation::{PeerDirectory, PortalInfo, ServerInfo};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

/// In-process stand-in for the network: routes peer requests to the other world's router.
#[derive(Default)]
pub(super) struct Network {
    pub(super) routes: Mutex<HashMap<String, Router>>,
    /// Addresses to which messages can currently be delivered.
    pub(super) reachable: Mutex<HashSet<String>>,
}

impl Network {
    fn router(&self, api_url: &str) -> Result<Router, players::Error> {
        if !self.reachable.lock().unwrap().contains(api_url) {
            return Err(players::Error::Status(
                StatusCode::BAD_GATEWAY,
                "peer unreachable",
            ));
        }
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

    pub(super) fn set_reachable(&self, url: &str, reachable: bool) {
        let mut set = self.reachable.lock().unwrap();
        if reachable {
            set.insert(url.to_owned());
        } else {
            set.remove(url);
        }
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

    fn portal_info(
        &self,
        api_url: String,
        portal: String,
        _allow_private: bool,
    ) -> PeerFuture<PortalInfo> {
        let router = self.router(&api_url);
        Box::pin(async move {
            let response = request(&router?, &format!("/federation/portals/{portal}")).await;
            match response.status() {
                StatusCode::OK => {
                    serde_json::from_value(response_json(response).await).map_err(|_| {
                        players::Error::Status(StatusCode::BAD_GATEWAY, "invalid peer reply")
                    })
                }
                StatusCode::NOT_FOUND => Err(players::Error::Status(
                    StatusCode::NOT_FOUND,
                    "the portal does not stand there",
                )),
                _ => Err(players::Error::Status(
                    StatusCode::BAD_GATEWAY,
                    "peer unreachable",
                )),
            }
        })
    }

    fn post_link(
        &self,
        api_url: String,
        message: String,
        _allow_private: bool,
    ) -> PeerFuture<String> {
        let router = self.router(&api_url);
        Box::pin(async move {
            let body = serde_json::json!({ "message": message }).to_string();
            let response =
                player_request(&router?, "POST", "/federation/portals/link", &body, "").await;
            match response.status() {
                status if status.is_success() => Ok(response_json(response).await["state"]
                    .as_str()
                    .unwrap()
                    .to_owned()),
                status => Err(players::Error::Status(status, "peer rejected the link")),
            }
        })
    }

    fn post_unlink(
        &self,
        api_url: String,
        message: String,
        _allow_private: bool,
    ) -> PeerFuture<()> {
        let router = self.router(&api_url);
        Box::pin(async move {
            let body = serde_json::json!({ "message": message }).to_string();
            let response =
                player_request(&router?, "POST", "/federation/portals/unlink", &body, "").await;
            match response.status() {
                status if status.is_success() => Ok(()),
                StatusCode::NOT_FOUND => {
                    Err(players::Error::Status(StatusCode::GONE, "unknown portal"))
                }
                status => Err(players::Error::Status(status, "peer rejected the message")),
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

async fn share_link(world: &World, token: &str, portal: &str) -> Response {
    player_request(
        &world.router,
        "GET",
        &format!("/portals/mine/{portal}/link"),
        "",
        token,
    )
    .await
}

async fn paste(world: &World, token: &str, portal: &str, link: &str) -> Response {
    player_request(
        &world.router,
        "POST",
        &format!("/portals/mine/{portal}/connect"),
        &serde_json::json!({ "link": link }).to_string(),
        token,
    )
    .await
}

async fn disconnect(world: &World, token: &str, portal: &str) -> Response {
    player_request(
        &world.router,
        "DELETE",
        &format!("/portals/mine/{portal}/link"),
        "",
        token,
    )
    .await
}

async fn link_of(world: &World, token: &str, portal: &str) -> String {
    let response = share_link(world, token, portal).await;
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await["link"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn state_of(pool: &PgPool, portal: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, peer_host FROM portal_pacts WHERE id = $1::uuid")
        .bind(portal)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Two finished portals, one in each world.
async fn finished_pair(pool: &PgPool, policy_a: &str, policy_b: &str) -> (Pair, String, String) {
    let p = two_worlds(pool, policy_a, policy_b).await;
    let a = finish_portal(pool, &p.a, "vitex", &p.vitex, "brana-sever").await;
    let b = finish_portal(pool, &p.b, "anna", &p.anna, "brana-jih").await;
    (p, a, b)
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn pasting_the_link_of_a_finished_portal_links_both_ends(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let p = two_worlds(&pool, "open", "open").await;
    let a = finish_portal(&pool, &p.a, "vitex", &p.vitex, "brana-sever").await;

    // The world publishes its identity and says whether a portal stands and is complete.
    let info = response_json(request(&p.a.router, "/.well-known/ishtaria/server.json").await).await;
    assert_eq!(info["server_name"], "svet-a.example.org");
    let public =
        response_json(request(&p.a.router, &format!("/federation/portals/{a}")).await).await;
    assert_eq!(
        (public["state"].as_str(), public["name"].as_str()),
        (Some("built"), Some("brana-sever"))
    );
    assert_eq!(
        request(
            &p.a.router,
            "/federation/portals/0192f3a1-5b1e-7c3a-9d4e-1a2b3c4d5e6f"
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    // An unfinished portal has no link to offer and cannot be connected.
    let unfinished = response_json(super::building::build_portal(&p.b, &p.anna, "brana-jih").await)
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        share_link(&p.b, &p.anna, &unfinished).await.status(),
        StatusCode::CONFLICT
    );
    let link = link_of(&p.a, &p.vitex, &a).await;
    assert!(link.starts_with("ishtaria-portal:v1."));
    assert_eq!(
        paste(&p.b, &p.anna, &unfinished, &link).await.status(),
        StatusCode::CONFLICT,
        "only a finished portal connects"
    );
    assert_eq!(
        share_link(&p.a, &p.anna, &a).await.status(),
        StatusCode::UNAUTHORIZED,
        "another world's session"
    );

    // Finish it and paste: the other server is asked, then both ends open.
    give(
        &pool,
        "anna",
        &[
            ("stone_block", 10),
            ("pine_plank", 20),
            ("quartz_crystal", 2),
        ],
    )
    .await;
    for (item, quantity) in [
        ("stone_block", "10"),
        ("pine_plank", "20"),
        ("quartz_crystal", "2"),
    ] {
        assert_eq!(
            super::building::deliver(&p.b, &p.anna, &unfinished, item, quantity)
                .await
                .status(),
            StatusCode::OK
        );
    }
    let linked = paste(&p.b, &p.anna, &unfinished, &link).await;
    assert_eq!(linked.status(), StatusCode::OK);
    let linked = response_json(linked).await;
    assert_eq!(linked["state"], "open");
    assert_eq!(linked["peer_host"], "svet-a.example.org");
    assert_eq!(linked["peer_portal_name"], "brana-sever");
    assert_eq!(
        state_of(&pool, &a).await,
        ("open".into(), Some("svet-b.example.org".into())),
        "the other end opened as well"
    );
    let rendered: (String, String) =
        sqlx::query_as("SELECT state, peer FROM portals WHERE world_id = $1")
            .bind(p.a.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rendered, ("open".into(), "svet-b.example.org".into()));

    // A portal has exactly one counterpart: neither end offers or accepts another link.
    assert_eq!(
        share_link(&p.a, &p.vitex, &a).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        paste(&p.a, &p.vitex, &a, &link).await.status(),
        StatusCode::CONFLICT
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_link_is_verified_before_the_portal_is_activated(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let (p, a, b) = finished_pair(&pool, "open", "open").await;
    let link = link_of(&p.a, &p.vitex, &a).await;

    // Not a link at all, or a link of this very world.
    for bad in [
        "",
        "hello",
        "ishtaria-portal:v1.",
        "ishtaria-portal:v1.a.b.c",
        "ishtaria-invite:v1.x.y",
    ] {
        assert_eq!(
            paste(&p.b, &p.anna, &b, bad).await.status(),
            StatusCode::BAD_REQUEST,
            "{bad:?}"
        );
    }
    assert_eq!(
        paste(&p.a, &p.vitex, &a, &link).await.status(),
        StatusCode::BAD_REQUEST,
        "a world does not link itself"
    );

    // A forged payload fails the signature.
    let token = link.strip_prefix("ishtaria-portal:v1.").unwrap();
    let (payload, signature) = token.split_once('.').unwrap();
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    let mut decoded = String::from_utf8(B64.decode(payload).unwrap()).unwrap();
    decoded = decoded.replace("brana-sever", "brana-nepravda");
    let forged = format!("ishtaria-portal:v1.{}.{signature}", B64.encode(decoded));
    assert_eq!(
        paste(&p.b, &p.anna, &b, &forged).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(state_of(&pool, &b).await.0, "built");

    // The other server must be reachable.
    p.network.set_reachable(&p.a.url, false);
    assert_eq!(
        paste(&p.b, &p.anna, &b, &link).await.status(),
        StatusCode::BAD_GATEWAY
    );
    p.network.set_reachable(&p.a.url, true);
    assert_eq!(
        state_of(&pool, &b).await.0,
        "built",
        "nothing changed while it was unreachable"
    );

    // The portal must stand there and be complete.
    sqlx::query("UPDATE portal_pacts SET state = 'building' WHERE id = $1::uuid")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        paste(&p.b, &p.anna, &b, &link).await.status(),
        StatusCode::CONFLICT,
        "not finished"
    );
    assert_eq!(state_of(&pool, &b).await.0, "built");
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn the_operator_policy_decides_whether_a_link_opens(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    // A world that does not link refuses on both sides.
    let (p, a, b) = finished_pair(&pool, "closed", "open").await;
    assert_eq!(
        paste(&p.a, &p.vitex, &a, &link_of(&p.b, &p.anna, &b).await)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        paste(&p.b, &p.anna, &b, &link_of(&p.a, &p.vitex, &a).await)
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "world A refuses the request"
    );
    assert_eq!(state_of(&pool, &b).await.0, "built");
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn approve_policy_leaves_the_link_pending(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let (p, a, b) = finished_pair(&pool, "approve", "open").await;
    let linked =
        response_json(paste(&p.b, &p.anna, &b, &link_of(&p.a, &p.vitex, &a).await).await).await;
    assert_eq!(
        linked["state"], "pending",
        "the other world still has to approve (operator approve)"
    );
    assert_eq!(state_of(&pool, &a).await.0, "pending");
    // The operator of world A approves: the portal opens.
    sqlx::query("UPDATE portal_pacts SET state = 'open' WHERE id = $1::uuid")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(state_of(&pool, &a).await.0, "open");
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn the_owner_can_break_a_link_and_the_other_end_is_released(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let (p, a, b) = finished_pair(&pool, "open", "open").await;
    assert_eq!(
        disconnect(&p.a, &p.vitex, &a).await.status(),
        StatusCode::CONFLICT,
        "nothing to break yet"
    );
    let linked = paste(&p.b, &p.anna, &b, &link_of(&p.a, &p.vitex, &a).await).await;
    assert_eq!(linked.status(), StatusCode::OK);
    assert_eq!(state_of(&pool, &a).await.0, "open");

    // Only the owner can break it.
    let stranger = create_session(&p.a.router, "stranger").await;
    assert_eq!(
        disconnect(&p.a, &stranger, &a).await.status(),
        StatusCode::NOT_FOUND
    );
    // The peer is unreachable: the owner can still break the link; the message is repeated.
    p.network.set_reachable(&p.b.url, false);
    let broken = disconnect(&p.a, &p.vitex, &a).await;
    assert_eq!(broken.status(), StatusCode::OK);
    let broken = response_json(broken).await;
    assert_eq!(
        (broken["state"].as_str(), broken["peer_host"].is_null()),
        (Some("built"), true)
    );
    assert_eq!(
        state_of(&pool, &b).await.0,
        "open",
        "the other world has not heard yet"
    );
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_unlinks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 1);
    p.network.set_reachable(&p.b.url, true);
    let directory: Arc<dyn PeerDirectory> = p.network.clone();
    crate::portals::retry_pending(
        &AppState {
            pool: pool.clone(),
            world_id: p.a.id,
        },
        directory.as_ref(),
    )
    .await;
    assert_eq!(
        state_of(&pool, &b).await,
        ("built".into(), None),
        "the other end is released"
    );
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_unlinks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0, "delivered, so not repeated");
    let ends: Vec<String> = sqlx::query_scalar("SELECT state FROM portals ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ends, ["building", "building"], "both gates are plain again");

    // Both portals are free to be linked again, with each other or with others.
    assert_eq!(
        paste(&p.b, &p.anna, &b, &link_of(&p.a, &p.vitex, &a).await)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(state_of(&pool, &a).await.0, "open");

    // Closing a linked portal breaks its link as well; a ruin stays behind.
    assert_eq!(
        player_request(
            &p.b.router,
            "DELETE",
            &format!("/portals/mine/{b}"),
            "",
            &p.anna
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(state_of(&pool, &b).await.0, "closed");
    assert_eq!(
        state_of(&pool, &a).await.0,
        "built",
        "the other end is free again"
    );
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn only_the_linked_world_can_release_a_portal(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let (p, a, b) = finished_pair(&pool, "open", "open").await;
    assert_eq!(
        paste(&p.b, &p.anna, &b, &link_of(&p.a, &p.vitex, &a).await)
            .await
            .status(),
        StatusCode::OK
    );
    // A message that is not signed by the linked world's key does nothing.
    let garbage = serde_json::json!({ "message": "e30.AAAA" }).to_string();
    assert_ne!(
        player_request(
            &p.a.router,
            "POST",
            "/federation/portals/unlink",
            &garbage,
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_ne!(
        player_request(
            &p.a.router,
            "POST",
            "/federation/portals/link",
            &garbage,
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(state_of(&pool, &a).await.0, "open");
}
