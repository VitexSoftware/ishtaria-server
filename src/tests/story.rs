use super::harvest::teleport;
use super::*;
use crate::story::world;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

/// A one-NPC disk named `play` installed in a scratch directory.
fn install_disk() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ishtaria-story-it-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let root = dir.join("play");
    write(&root, "datadisk.yaml", "id: play\nversion: 1.0.0\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\n");
    let strings = "place.hut: Hut\nnpc.hermit: Hermit\nhi: Hi\nbye: Bye\nq: Quest\nq.s1: One\nq.s2: Two\nmusic.t: Tune\nr: Walk\nr.a: Go\nr.b: Arrived\n";
    write(&root, "i18n/en.yaml", strings);
    write(&root, "i18n/cs.yaml", strings);
    write(
        &root,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut, spawn: true}\n",
    );
    write(&root, "npcs/n.yaml", "- {id: hermit, name_key: npc.hermit, place: hut, character: {pack: retro, skin: humanMaleA}, dialogue: hermit, portrait: media/portraits/hermit.jpg}\n");
    write(
        &root,
        "music/m.yaml",
        "- {id: t, file: media/music/t.ogg, title_key: music.t, loop: true}\n",
    );
    write(&root, "quests/q.yaml", "id: q\ntitle_key: q\nstart_stage: s1\nstages:\n  s1: {text_key: q.s1}\n  s2: {text_key: q.s2, final: true}\n");
    write(&root, "quests/r.yaml", "id: r\ntitle_key: r\nstart_stage: a\nstages:\n  a: {text_key: r.a, reach: {place: hut, radius_m: 30, goto: b}}\n  b: {text_key: r.b, final: true}\n");
    write(&root, "dialogues/hermit.yaml", "id: hermit\nmusic: t\nstart: hi\nnodes:\n  hi:\n    text_key: hi\n    choices:\n      - {text_key: bye, if: {gold_at_least: 5}, effects: [{gold: -5}, {once: {key: reward, effects: [{gold: 20}, {set_stage: {quest: q, stage: s2}}]}}, {set_flag: met}, {set_stage: {quest: r, stage: a}}]}\n");
    fs::create_dir_all(root.join("media/portraits")).unwrap();
    fs::create_dir_all(root.join("media/music")).unwrap();
    fs::write(
        root.join("media/portraits/hermit.jpg"),
        b"\xff\xd8not really a jpeg",
    )
    .unwrap();
    fs::write(root.join("media/music/t.ogg"), b"OggS not really audio").unwrap();
    dir
}

async fn apply_disk(pool: &PgPool, world_id: i64) {
    // The admin tool records only id and version; the server pins the content hash.
    sqlx::query("INSERT INTO world_datadisks (world_id, disk_id, version, position) VALUES ($1, 'play', '1.0.0', 0)")
        .bind(world_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn post(router: &Router, token: &str, path: &str, body: serde_json::Value) -> Response {
    player_request(router, "POST", path, &body.to_string(), token).await
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn dialogue_is_server_authoritative_replay_safe_and_range_checked(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let dir = install_disk();
    std::env::set_var("ISHTARIA_DATADISK_DIR", &dir);
    world::reset_cache().await;
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "visitor").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Without an applied disk the world has no story.
    let response = post(
        &router,
        &token,
        "/story/dialogue/start",
        serde_json::json!({"npc_id": "play:hermit"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    apply_disk(&pool, world_id).await;
    world::reset_cache().await;

    let story = world::world(&state).await.unwrap().expect("disk applied");
    let pinned: Option<String> = sqlx::query_scalar("SELECT sha256 FROM world_datadisks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pinned.as_deref(), Some(story.story.disks[0].2.as_str()));
    let npc = story.npcs[0].clone();
    assert_eq!(npc.id, "play:hermit");
    assert_eq!(
        npc.portrait.as_deref(),
        Some("/story/media/play/media/portraits/hermit.jpg")
    );
    let anchors: i64 = sqlx::query_scalar("SELECT count(*) FROM story_anchors")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(anchors, 1, "the place is persisted");

    // The disk's spawn place is where new characters appear; characters that already stood
    // somewhere stay where they are.
    let before: [f64; 3] = {
        let (x, y, z): (f64, f64, f64) =
            sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
                .bind(player_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        [x, y, z]
    };
    let newcomer = create_session(&router, "newcomer").await;
    let _ = newcomer;
    let (x, y, z): (f64, f64, f64) = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE username = 'newcomer'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let apart =
        |a: [f64; 3], b: [f64; 3]| (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt();
    assert!(
        apart([x, y, z], npc.position) < 25.0,
        "the newcomer woke up by the place of the disk"
    );
    assert!(
        apart([x, y, z], before) > 1000.0,
        "not at the ordinary spawn"
    );
    let still: (f64, f64, f64) =
        sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        [still.0, still.1, still.2],
        before,
        "an existing character does not move"
    );

    // Placement is persisted: a rebuilt world finds the same NPC position.
    world::reset_cache().await;
    assert_eq!(world::world(&state).await.unwrap().unwrap().npcs[0], npc);

    // Too far to talk.
    let far = [npc.position[0] + 50.0, npc.position[1], npc.position[2]];
    teleport(&pool, player_id, far).await;
    let response = post(
        &router,
        &token,
        "/story/dialogue/start",
        serde_json::json!({"npc_id": "play:hermit"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    teleport(&pool, player_id, npc.position).await;
    let response = post(
        &router,
        &token,
        "/story/dialogue/start",
        serde_json::json!({"npc_id": "play:hermit"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let started = response_json(response).await;
    assert_eq!(started["node"]["text_key"], "play:hi");
    assert_eq!(
        started["music"]["url"],
        "/story/media/play/media/music/t.ogg"
    );
    assert_eq!(started["music"]["loop"], true);
    let seq = started["seq"].as_i64().unwrap();
    let gold_before = gold_of(&pool, player_id).await;

    // Unknown choice and stale sequence change nothing.
    let bad = post(
        &router,
        &token,
        "/story/dialogue/choose",
        serde_json::json!({"npc_id": "play:hermit", "seq": seq, "choice": 7}),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::CONFLICT);
    let stale = post(
        &router,
        &token,
        "/story/dialogue/choose",
        serde_json::json!({"npc_id": "play:hermit", "seq": seq + 5, "choice": 0}),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(gold_of(&pool, player_id).await, gold_before);

    // The same answer sent twice at once is applied exactly once.
    let body = serde_json::json!({"npc_id": "play:hermit", "seq": seq, "choice": 0});
    let (first, second) = tokio::join!(
        post(&router, &token, "/story/dialogue/choose", body.clone()),
        post(&router, &token, "/story/dialogue/choose", body.clone())
    );
    let mut codes = [first.status(), second.status()];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::CONFLICT]);
    assert_eq!(gold_of(&pool, player_id).await, gold_before - 5 + 20);

    // Replaying the old answer is refused; a new conversation pays 5 but the reward is once.
    let replay = post(&router, &token, "/story/dialogue/choose", body).await;
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    let again = response_json(
        post(
            &router,
            &token,
            "/story/dialogue/start",
            serde_json::json!({"npc_id": "play:hermit"}),
        )
        .await,
    )
    .await;
    let reply = post(
        &router,
        &token,
        "/story/dialogue/choose",
        serde_json::json!({"npc_id": "play:hermit", "seq": again["seq"], "choice": 0}),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(response_json(reply).await["open"], false);
    assert_eq!(gold_of(&pool, player_id).await, gold_before + 15 - 5);

    // Quest log and flags are server state.
    let quests =
        response_json(player_request(&router, "GET", "/story/quests", "", &token).await).await;
    let first = quests
        .as_array()
        .unwrap()
        .iter()
        .find(|quest| quest["quest"] == "play:q")
        .unwrap();
    assert_eq!(first["stage"], "s2");
    assert_eq!(first["final"], true);

    // A quest that asks the player to reach a place advances only once they are there.
    sqlx::query("UPDATE player_quests SET stage = 'a' WHERE quest_id = 'play:r'")
        .execute(&pool)
        .await
        .unwrap();
    teleport(&pool, player_id, far).await;
    let away =
        response_json(player_request(&router, "GET", "/story/quests", "", &token).await).await;
    let walk = |list: &serde_json::Value| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|q| q["quest"] == "play:r")
            .map(|q| q["stage"].clone())
    };
    assert_eq!(walk(&away), Some(serde_json::json!("a")));
    teleport(&pool, player_id, npc.position).await;
    let there =
        response_json(player_request(&router, "GET", "/story/quests", "", &token).await).await;
    assert_eq!(walk(&there), Some(serde_json::json!("b")));

    // Poor players cannot pay: the choice is not even offered.
    set_gold(&pool, Some(player_id), 3).await;
    let poor = response_json(
        post(
            &router,
            &token,
            "/story/dialogue/start",
            serde_json::json!({"npc_id": "play:hermit"}),
        )
        .await,
    )
    .await;
    assert_eq!(poor["node"]["choices"], serde_json::json!([]));
    assert_eq!(gold_of(&pool, player_id).await, 3);

    // Translations and media.
    let strings = request(&router, "/story/strings").await;
    assert_eq!(strings.status(), StatusCode::OK);
    let tag = strings.headers()[header::ETAG].to_str().unwrap().to_owned();
    let json = response_json(strings).await;
    assert_eq!(json["languages"]["cs"]["play:hi"], "Hi");
    let cached = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/story/strings")
                .header(header::IF_NONE_MATCH, tag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);

    let image = request(&router, "/story/media/play/media/portraits/hermit.jpg").await;
    assert_eq!(image.status(), StatusCode::OK);
    assert_eq!(image.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        request(&router, "/story/media/play/media/music/t.ogg")
            .await
            .headers()[header::CONTENT_TYPE],
        "audio/ogg"
    );
    for path in [
        "/story/media/play/datadisk.yaml",
        "/story/media/play/media/portraits/missing.jpg",
        "/story/media/play/media/../datadisk.yaml",
        "/story/media/other/media/portraits/hermit.jpg",
        "/story/media/play/%2e%2e/datadisk.yaml",
    ] {
        let code = request(&router, path).await.status();
        assert!(
            code == StatusCode::NOT_FOUND || code == StatusCode::BAD_REQUEST,
            "{path} gave {code}"
        );
    }

    // A disk that changed after world generation is refused instead of silently used.
    write(&dir.join("play"), "i18n/en.yaml", "tampered: yes\n");
    world::reset_cache().await;
    assert_eq!(
        request(&router, "/story/strings").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );

    std::env::remove_var("ISHTARIA_DATADISK_DIR");
    world::reset_cache().await;
    let _ = fs::remove_dir_all(dir);
}
