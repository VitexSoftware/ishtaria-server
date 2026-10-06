//! HTTP API of the story: dialogues, quest log, translations and media.

use super::{
    engine::{self, Choose, Op, PlayerState, Reject, Shown},
    placement::Placed,
    world::{self, StoryWorld, TALK_RANGE_M},
    Story,
};
use crate::{players, AppState};
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use players::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use std::{collections::HashMap, sync::Arc};

type Tx<'a> = Transaction<'a, Postgres>;

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/story/dialogue/start", post(start))
        .route("/story/dialogue/choose", post(choose))
        .route("/story/dialogue", delete(leave))
        .route("/story/quests", get(quests))
        .route("/story/markers", get(markers))
        .layer(DefaultBodyLimit::max(1024))
        .route("/story/strings", get(strings))
        .route("/story/media/{disk}/{*path}", get(media))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    npc_id: String,
    /// Language of the client's UI (two lowercase letters): only selects the spoken line to send.
    /// It is never stored.
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChooseRequest {
    npc_id: String,
    /// The `seq` of the answer the choice was made from.
    seq: i64,
    choice: usize,
    /// See [`Start::lang`].
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Serialize)]
struct Music {
    url: String,
    title_key: String,
    #[serde(rename = "loop")]
    looped: bool,
}

#[derive(Serialize)]
struct Reply {
    npc_id: String,
    name_key: String,
    seq: i64,
    /// `false` once the conversation has ended; no node is shown then.
    open: bool,
    node: Option<Shown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    portrait: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    music: Option<Music>,
    player: players::Player,
}

async fn story_of(state: &AppState) -> Result<Arc<StoryWorld>, Error> {
    world::world(state)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "this world has no story"))
}

fn reject(reject: Reject) -> Error {
    match reject {
        Reject::Insufficient => status(StatusCode::CONFLICT, "not enough to pay"),
    }
}

/// Everything the engine needs to know about a player, read under row locks.
async fn load_state(tx: &mut Tx<'_>, player_id: i64) -> Result<PlayerState, Error> {
    let flags: Vec<String> =
        sqlx::query_scalar("SELECT flag FROM player_story_flags WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&mut **tx)
            .await?;
    let stages: Vec<(String, String)> =
        sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&mut **tx)
            .await?;
    let items: Vec<(String, i64)> = sqlx::query_as(
        "SELECT item_id, quantity FROM player_inventory WHERE player_id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_all(&mut **tx)
    .await?;
    let once: Vec<String> =
        sqlx::query_scalar("SELECT once_key FROM player_story_once WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&mut **tx)
            .await?;
    Ok(PlayerState {
        flags: flags.into_iter().collect(),
        stages: stages.into_iter().collect(),
        items: items.into_iter().collect(),
        once_done: once.into_iter().collect(),
    })
}

/// Commits the operations the engine produced. Any failure aborts the transaction.
async fn apply_ops(tx: &mut Tx<'_>, player_id: i64, ops: &[Op]) -> Result<(), Error> {
    for op in ops {
        match op {
            Op::Give(item, quantity) => {
                crate::gathering::add_items(tx, player_id, &[(item.clone(), *quantity)]).await?;
            }
            Op::Take(item, quantity) => {
                let taken = sqlx::query("UPDATE player_inventory SET quantity = quantity - $3 WHERE player_id = $1 AND item_id = $2 AND quantity >= $3")
                    .bind(player_id)
                    .bind(item)
                    .bind(quantity)
                    .execute(&mut **tx)
                    .await?;
                if taken.rows_affected() != 1 {
                    return Err(status(StatusCode::CONFLICT, "not enough to pay"));
                }
                sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2 AND quantity = 0")
                    .bind(player_id)
                    .bind(item)
                    .execute(&mut **tx)
                    .await?;
            }
            Op::Flag(flag) => {
                sqlx::query("INSERT INTO player_story_flags (player_id, flag) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                    .bind(player_id)
                    .bind(flag)
                    .execute(&mut **tx)
                    .await?;
            }
            Op::Stage(quest, stage) => {
                sqlx::query("INSERT INTO player_quests (player_id, quest_id, stage) VALUES ($1, $2, $3) ON CONFLICT (player_id, quest_id) DO UPDATE SET stage = EXCLUDED.stage, updated_at = now()")
                    .bind(player_id)
                    .bind(quest)
                    .bind(stage)
                    .execute(&mut **tx)
                    .await?;
            }
            Op::Once(key) => {
                sqlx::query("INSERT INTO player_story_once (player_id, once_key) VALUES ($1, $2)")
                    .bind(player_id)
                    .bind(key)
                    .execute(&mut **tx)
                    .await
                    .map_err(|_| status(StatusCode::CONFLICT, "reward already granted"))?;
            }
        }
    }
    Ok(())
}

/// Locks the player row and checks that the player stands near the NPC.
async fn near_npc<'a>(
    tx: &mut Tx<'_>,
    story: &'a StoryWorld,
    player_id: i64,
    npc_id: &str,
) -> Result<&'a world::NpcDescriptor, Error> {
    let position: Option<(Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((Some(x), Some(y), Some(z))) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let npc = story
        .npcs
        .iter()
        .find(|npc| npc.id == npc_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown character"))?;
    if world::distance([x, y, z], npc) > TALK_RANGE_M {
        return Err(status(StatusCode::FORBIDDEN, "character is out of reach"));
    }
    Ok(npc)
}

/// The language of a request: two lowercase ASCII letters, anything else is ignored.
fn request_language(lang: &Option<String>) -> Option<&str> {
    lang.as_deref()
        .filter(|l| l.len() == 2 && l.bytes().all(|b| b.is_ascii_lowercase()))
}

fn reply(
    story: &StoryWorld,
    npc: &world::NpcDescriptor,
    seq: i64,
    node: Option<Shown>,
    lang: Option<&str>,
    player: players::Player,
) -> Reply {
    let node = node.map(|mut shown| {
        shown.voice = lang
            .and_then(|l| {
                story
                    .story
                    .voices
                    .get(&(l.to_owned(), shown.text_key.clone()))
            })
            .map(|file| world::media_url(file));
        shown
    });
    let dialogue = story
        .story
        .npcs
        .get(&npc.id)
        .and_then(|n| story.story.dialogues.get(&n.dialogue));
    let music = dialogue
        .and_then(|d| d.music.as_ref())
        .and_then(|id| story.story.music.get(id))
        .map(|(file, title_key, looped)| Music {
            url: world::media_url(file),
            title_key: title_key.clone(),
            looped: *looped,
        });
    Reply {
        npc_id: npc.id.clone(),
        name_key: npc.name_key.clone(),
        seq,
        open: node.is_some(),
        node,
        portrait: npc.portrait.clone(),
        music,
        player,
    }
}

async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Start>,
) -> Result<Json<Reply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    advance_reach(&state, &story, player_id).await?;
    let mut tx = state.pool.begin().await?;
    let npc = near_npc(&mut tx, &story, player_id, &request.npc_id).await?;
    let definition = &story.story.npcs[&npc.id];
    let dialogue = &story.story.dialogues[&definition.dialogue];
    let mut player_state = load_state(&mut tx, player_id).await?;
    let mut ops = Vec::new();
    let shown = engine::enter(dialogue, &dialogue.start, &mut player_state, &mut ops)
        .map_err(reject)?
        .ok_or_else(|| {
            status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "dialogue has nothing to show",
            )
        })?;
    apply_ops(&mut tx, player_id, &ops).await?;
    let seq: i64 = sqlx::query_scalar("INSERT INTO dialogue_sessions (player_id, npc_id, node_id) VALUES ($1, $2, $3) ON CONFLICT (player_id) DO UPDATE SET npc_id = EXCLUDED.npc_id, node_id = EXCLUDED.node_id, seq = dialogue_sessions.seq + 1, updated_at = now() RETURNING seq")
        .bind(player_id)
        .bind(&npc.id)
        .bind(&shown.node)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let player = players::profile(&state, player_id).await?;
    Ok(Json(reply(
        &story,
        npc,
        seq,
        Some(shown),
        request_language(&request.lang),
        player,
    )))
}

async fn choose(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChooseRequest>,
) -> Result<Json<Reply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    let mut tx = state.pool.begin().await?;
    let npc = near_npc(&mut tx, &story, player_id, &request.npc_id).await?;
    let session: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT npc_id, node_id, seq FROM dialogue_sessions WHERE player_id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((session_npc, node, seq)) = session else {
        return Err(status(StatusCode::CONFLICT, "no open conversation"));
    };
    if session_npc != npc.id || seq != request.seq {
        return Err(status(StatusCode::CONFLICT, "the conversation moved on"));
    }
    let dialogue = &story.story.dialogues[&story.story.npcs[&npc.id].dialogue];
    let mut player_state = load_state(&mut tx, player_id).await?;
    let mut ops = Vec::new();
    let outcome = engine::choose(dialogue, &node, request.choice, &mut player_state, &mut ops);
    let (shown, next_seq) = match outcome {
        Choose::Unavailable => return Err(status(StatusCode::CONFLICT, "choice unavailable")),
        Choose::Rejected(why) => return Err(reject(why)),
        Choose::End => {
            apply_ops(&mut tx, player_id, &ops).await?;
            sqlx::query("DELETE FROM dialogue_sessions WHERE player_id = $1")
                .bind(player_id)
                .execute(&mut *tx)
                .await?;
            (None, seq + 1)
        }
        Choose::Next(shown) => {
            apply_ops(&mut tx, player_id, &ops).await?;
            sqlx::query("UPDATE dialogue_sessions SET node_id = $2, seq = seq + 1, updated_at = now() WHERE player_id = $1")
                .bind(player_id)
                .bind(&shown.node)
                .execute(&mut *tx)
                .await?;
            (Some(shown), seq + 1)
        }
    };
    tx.commit().await?;
    let player = players::profile(&state, player_id).await?;
    Ok(Json(reply(
        &story,
        npc,
        next_seq,
        shown,
        request_language(&request.lang),
        player,
    )))
}

/// Closes the conversation without choosing (the player walked away or pressed escape).
async fn leave(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    sqlx::query("DELETE FROM dialogue_sessions WHERE player_id = $1")
        .bind(player_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct QuestEntry {
    quest: String,
    title_key: String,
    stage: String,
    text_key: String,
    #[serde(rename = "final")]
    final_stage: bool,
}

/// Moves quests on whose current stage asks the player to reach a place, when they have.
/// Each step is conditional on the stage it started from, so concurrent calls cannot repeat it.
async fn advance_reach(state: &AppState, story: &StoryWorld, player_id: i64) -> Result<(), Error> {
    let position: Option<(Option<f64>, Option<f64>, Option<f64>)> =
        sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((Some(x), Some(y), Some(z))) = position else {
        return Ok(());
    };
    // A short chain of reach stages may complete in one go.
    for _ in 0..4 {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let mut moved = false;
        for (quest, stage) in rows {
            let Some(reach) = story
                .story
                .quests
                .get(&quest)
                .and_then(|q| q.stages.get(&stage))
                .and_then(|s| s.reach.as_ref())
            else {
                continue;
            };
            let Some(site) = story.sites.get(&reach.place) else {
                continue;
            };
            let at = world::site_position(site);
            let distance = ((x - at[0]).powi(2) + (y - at[1]).powi(2) + (z - at[2]).powi(2)).sqrt();
            if distance <= reach.radius_m {
                let done = sqlx::query("UPDATE player_quests SET stage = $4, updated_at = now() WHERE player_id = $1 AND quest_id = $2 AND stage = $3")
                    .bind(player_id)
                    .bind(&quest)
                    .bind(&stage)
                    .bind(&reach.goto)
                    .execute(&state.pool)
                    .await?;
                moved |= done.rows_affected() == 1;
            }
        }
        if !moved {
            break;
        }
    }
    Ok(())
}

async fn quests(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<QuestEntry>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    advance_reach(&state, &story, player_id).await?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT quest_id, stage FROM player_quests WHERE player_id = $1 ORDER BY updated_at DESC",
    )
    .bind(player_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .filter_map(|(quest, stage)| {
                let definition = story.story.quests.get(&quest)?;
                let found = definition.stages.get(&stage)?;
                Some(QuestEntry {
                    title_key: definition.title_key.clone(),
                    text_key: found.text_key.clone(),
                    final_stage: found.final_stage,
                    quest,
                    stage,
                })
            })
            .collect(),
    ))
}

/// Where the player is pointed to: a place of an active quest or the start of one not yet begun.
#[derive(Serialize)]
pub(super) struct Marker {
    /// Qualified place id.
    pub(super) id: String,
    pub(super) quest: String,
    pub(super) name_key: String,
    pub(super) kind: String,
    /// World metres, like the positions of NPCs.
    pub(super) position: [f64; 3],
    /// The place where a quest not yet begun waits (instead of one of an active quest).
    pub(super) next: bool,
}

/// The item that lets the player see the markers (given by Faust).
const GUIDE_ITEM: &str = "aetherglass";

/// Markers of the player's active quests. Empty until the player holds the aetherglass,
/// and it never names a place that no quest of the player points to.
async fn markers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Marker>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    let holds: Option<(i64,)> = sqlx::query_as(
        "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2 AND quantity > 0",
    )
    .bind(player_id)
    .bind(GUIDE_ITEM)
    .fetch_optional(&state.pool)
    .await?;
    if holds.is_none() {
        return Ok(Json(Vec::new()));
    }
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&state.pool)
            .await?;
    Ok(Json(markers_of(&story.story, &story.sites, &rows)))
}

/// The places the quests point to, given the player's `(quest, stage)` rows.
pub(super) fn markers_of(
    story: &Story,
    sites: &HashMap<String, Placed>,
    rows: &[(String, String)],
) -> Vec<Marker> {
    let mut found = Vec::new();
    for (id, definition) in &story.quests {
        let current = rows.iter().find(|(quest, _)| quest == id);
        let (place, next) = match current {
            Some((_, stage)) => {
                let Some(stage) = definition.stages.get(stage) else {
                    continue;
                };
                if stage.final_stage {
                    continue;
                }
                let place = stage
                    .guide
                    .as_ref()
                    .or(stage.reach.as_ref().map(|reach| &reach.place));
                (place, false)
            }
            None => (definition.start_place.as_ref(), true),
        };
        let Some(place) = place else { continue };
        let (Some(site), Some(anchor)) = (
            sites.get(place),
            story.anchors.iter().find(|anchor| &anchor.id == place),
        ) else {
            continue;
        };
        found.push(Marker {
            id: place.clone(),
            quest: id.clone(),
            name_key: anchor.place.name_key.clone(),
            kind: anchor.place.kind.clone(),
            position: world::site_position(site),
            next,
        });
    }
    found.truncate(64);
    found
}

fn etag(value: &str) -> String {
    format!("\"{value}\"")
}

fn not_modified(headers: &HeaderMap, tag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == tag)
}

/// All translations of every applied disk. They are not selected by language, so every
/// language is handed out and the client picks (dialogue requests do name a language, but
/// only to choose the spoken line).
async fn strings(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Error> {
    let story = story_of(&state).await?;
    let version = format!(
        "{:x}",
        Sha256::digest(
            story
                .story
                .disks
                .iter()
                .map(|(id, _, sha)| format!("{id}:{sha}"))
                .collect::<Vec<_>>()
                .join(",")
                .as_bytes()
        )
    );
    let tag = etag(&version);
    if not_modified(&headers, &tag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response());
    }
    let body = serde_json::json!({ "version": version, "languages": story.story.strings });
    Ok((
        [
            (header::ETAG, tag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        Json(body),
    )
        .into_response())
}

/// Portraits and music of the applied disks. Only files the loaded disks name are served.
async fn media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((disk, path)): Path<(String, String)>,
) -> Result<Response, Error> {
    let story = story_of(&state).await?;
    let key = format!("{disk}/{path}");
    let (file, sha256) = story
        .story
        .media
        .get(&key)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no such media"))?;
    let content_type = super::disk::media_type(&path)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no such media"))?;
    let tag = etag(sha256);
    if not_modified(&headers, &tag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response());
    }
    let bytes = tokio::fs::read(file)
        .await
        .map_err(|_| status(StatusCode::SERVICE_UNAVAILABLE, "media unavailable"))?;
    if format!("{:x}", Sha256::digest(&bytes)) != *sha256 {
        return Err(status(
            StatusCode::SERVICE_UNAVAILABLE,
            "media changed on disk",
        ));
    }
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ETAG, tag.parse().expect("hex digest"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}
