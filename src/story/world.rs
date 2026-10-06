//! The story of one running world: the disks it was generated with, where their places
//! stand, and where their NPCs are. Built lazily from the database and cached.

use super::{
    placement::{self, Placed},
    Story,
};
use crate::{
    movement::{self, length, RADIUS},
    players::Error,
    AppState,
};
use axum::http::StatusCode;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// Directory holding installed datadisks (`<dir>/<disk id>/datadisk.yaml`).
const DEFAULT_DISK_DIR: &str = "/usr/share/ishtaria/datadisks";
/// How long a built world is reused before the disk selection is read again.
const REFRESH: Duration = Duration::from_secs(30);
/// NPCs are sent to clients within this distance of the requested point.
const NPC_RADIUS_M: f64 = 400.0;
/// A player may talk to an NPC within this distance.
pub const TALK_RANGE_M: f64 = 6.0;

pub fn disk_dir() -> PathBuf {
    std::env::var_os("ISHTARIA_DATADISK_DIR")
        .map_or_else(|| PathBuf::from(DEFAULT_DISK_DIR), PathBuf::from)
}

/// An NPC as sent to clients near a position.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NpcDescriptor {
    pub id: String,
    pub name_key: String,
    /// `<pack>/<skin>` of the Kenney character models.
    pub character: String,
    /// URL path of a glTF model drawn instead of `character`, when the NPC has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub position: [f64; 3],
    pub yaw: f64,
    pub scale_m: f64,
    /// URL path of the portrait, when the NPC has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<String>,
}

/// A region around a place where a track plays; clients fade it in and out by distance.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AreaDescriptor {
    pub id: String,
    pub position: [f64; 3],
    pub radius_m: f64,
    pub music: AreaMusic,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AreaMusic {
    pub url: String,
    pub title_key: String,
    #[serde(rename = "loop")]
    pub looped: bool,
}

pub struct StoryWorld {
    pub story: Story,
    /// Buildings and props of the places (towns, graveyards, harbours).
    pub props: Vec<super::settlements::WorldProp>,
    /// Where new characters appear, when a datadisk defines a spawn point.
    pub spawn: Option<[f64; 3]>,
    /// Persisted sites of the places, by qualified place id.
    pub sites: HashMap<String, Placed>,
    pub npcs: Vec<NpcDescriptor>,
    /// Places with background music.
    pub areas: Vec<AreaDescriptor>,
}

/// Path clients use to fetch a media file of a disk.
pub fn media_url(media: &str) -> String {
    format!("/story/media/{media}")
}

type Cached = Option<(Instant, String, Option<Arc<StoryWorld>>)>;
static WORLD: Mutex<Cached> = Mutex::const_new(None);

fn unavailable() -> Error {
    Error::Status(StatusCode::SERVICE_UNAVAILABLE, "story unavailable")
}

/// The world's story, or `None` when no datadisk was applied to it.
pub async fn world(state: &AppState) -> Result<Option<Arc<StoryWorld>>, Error> {
    let mut cache = WORLD.lock().await;
    let key = state.world_id.to_string();
    if let Some((at, stored, world)) = cache.as_ref() {
        if stored == &key && at.elapsed() < REFRESH {
            return Ok(world.clone());
        }
    }
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT disk_id, version, sha256 FROM world_datadisks WHERE world_id = $1 ORDER BY position",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;
    if rows.is_empty() && super::settlements::count() == 0 {
        *cache = Some((Instant::now(), key, None));
        return Ok(None);
    }
    let built = match build(state, rows).await {
        Ok(built) => built,
        Err(error) => {
            eprintln!("story unavailable: {error:#}");
            // Do not rebuild on every request: remember the failure for a while.
            *cache = Some((Instant::now(), key, None));
            return Err(unavailable());
        }
    };
    let built = Arc::new(built);
    *cache = Some((Instant::now(), key, Some(built.clone())));
    Ok(Some(built))
}

async fn build(
    state: &AppState,
    rows: Vec<(String, String, Option<String>)>,
) -> anyhow::Result<StoryWorld> {
    let dir = disk_dir();
    let paths: Vec<PathBuf> = rows.iter().map(|(id, _, _)| dir.join(id)).collect();
    let terrain = movement::terrain(state)
        .await
        .map_err(|_| anyhow::anyhow!("terrain unavailable"))?;
    let heightmap = terrain.heightmap_sha256().to_owned();
    let seed = terrain.seed().to_owned();
    let mut extra = Vec::new();
    if super::settlements::count() > 0 {
        extra.push(super::settlements::world_disk(
            &seed,
            super::settlements::count(),
        ));
    }
    let mut story =
        tokio::task::spawn_blocking(move || Story::load_with(&paths, extra, false)).await??;
    let loaded: Vec<(String, String, String)> = story.disks.clone();
    for (id, version, sha256) in &rows {
        let found = loaded
            .iter()
            .find(|(loaded_id, _, _)| loaded_id == id)
            .filter(|(_, v, _)| v == version);
        let Some((_, _, actual)) = found else {
            anyhow::bail!("datadisk {id} differs from the one the world was generated with");
        };
        match sha256 {
            Some(pinned) => anyhow::ensure!(
                pinned == actual,
                "datadisk {id} changed after the world was generated"
            ),
            // The admin tool only records which disks to use; the first load pins their content.
            None => {
                sqlx::query("UPDATE world_datadisks SET sha256 = $3 WHERE world_id = $1 AND disk_id = $2 AND sha256 IS NULL")
                    .bind(state.world_id)
                    .bind(id)
                    .bind(actual)
                    .execute(&state.pool)
                    .await?;
            }
        }
    }
    let stored: Vec<(String, String, f64, f64, f64, f64)> = sqlx::query_as(
        "SELECT anchor_id, heightmap_sha256, direction_x, direction_y, direction_z, height_m FROM story_anchors WHERE world_id = $1",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;
    anyhow::ensure!(
        stored.iter().all(|row| row.1 == heightmap),
        "story places were placed on a different heightmap"
    );
    let mut sites: HashMap<String, Placed> = stored
        .into_iter()
        .map(|(id, _, x, y, z, height)| {
            (
                id.clone(),
                Placed {
                    id,
                    direction: [x, y, z],
                    height_m: height,
                },
            )
        })
        .collect();
    let existing: Vec<Placed> = sites.values().cloned().collect();
    let (placing_story, placing_terrain, placing_seed) =
        (story.clone(), terrain.clone(), seed.clone());
    let fresh = tokio::task::spawn_blocking(move || {
        placement::place_anchors(
            &placing_story,
            placing_terrain.as_ref(),
            &placing_seed,
            &existing,
        )
    })
    .await??;
    if !fresh.is_empty() {
        let mut transaction = state.pool.begin().await?;
        for site in &fresh {
            sqlx::query("INSERT INTO story_anchors (world_id, anchor_id, heightmap_sha256, direction_x, direction_y, direction_z, height_m) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING")
                .bind(state.world_id)
                .bind(&site.id)
                .bind(&heightmap)
                .bind(site.direction[0])
                .bind(site.direction[1])
                .bind(site.direction[2])
                .bind(site.height_m)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        // Another process may have won the race; the stored rows are the truth.
        let rows: Vec<(String, f64, f64, f64, f64)> = sqlx::query_as(
            "SELECT anchor_id, direction_x, direction_y, direction_z, height_m FROM story_anchors WHERE world_id = $1",
        )
        .bind(state.world_id)
        .fetch_all(&state.pool)
        .await?;
        sites = rows
            .into_iter()
            .map(|(id, x, y, z, height)| {
                (
                    id.clone(),
                    Placed {
                        id,
                        direction: [x, y, z],
                        height_m: height,
                    },
                )
            })
            .collect();
    }
    let built = super::settlements::finish(&mut story, &mut sites, terrain.as_ref(), &seed)?;
    let props = built.props;
    let colliders = built.colliders;
    let npcs = npc_descriptors(&story, &sites, terrain.as_ref());
    let spawn = find_spawn(&story, &sites, &colliders, &npcs, terrain.as_ref(), &seed);
    let areas = area_descriptors(&story, &sites);
    movement::set_statics(format!("{seed}:{heightmap}"), colliders);
    Ok(StoryWorld {
        spawn,
        story,
        props,
        sites,
        npcs,
        areas,
    })
}

/// A free spot near the place a datadisk made the spawn point: inside its fence, clear of walls,
/// gravestones and characters. The first spawn place in the order of the disks wins.
pub fn find_spawn(
    story: &Story,
    sites: &HashMap<String, Placed>,
    colliders: &[movement::WorldObject],
    npcs: &[NpcDescriptor],
    ground: &dyn placement::Ground,
    seed: &str,
) -> Option<[f64; 3]> {
    let anchor = story
        .anchors
        .iter()
        .find(|anchor| anchor.place.spawn && sites.contains_key(&anchor.id))?;
    let site = &sites[&anchor.id];
    let reach = (anchor.place.radius_m.unwrap_or(20.0) * 0.4).clamp(3.0, 12.0);
    let mut random = crate::scenery::Rng::new(seed, &format!("spawn:{}", anchor.id));
    let mut fallback = None;
    for attempt in 0..400 {
        // Closer spots first, spreading outwards as the nearest ones turn out to be taken.
        let distance = 2.5 + (reach - 2.5) * f64::from(attempt) / 400.0 + random.between(0.0, 1.0);
        let angle = random.between(0.0, std::f64::consts::TAU);
        let (direction, position) = crate::scenery::world_from_local(
            site.direction,
            distance * angle.cos(),
            distance * angle.sin(),
            |direction| ground.height(direction).max(0.0),
            movement::RADIUS,
        );
        let _ = direction;
        fallback.get_or_insert(position);
        let clear = |other: [f64; 3], radius: f64| {
            length(std::array::from_fn(|axis| position[axis] - other[axis])) > radius
        };
        if colliders
            .iter()
            .all(|c| clear(c.position, c.collision_radius_m + 1.2))
            && npcs.iter().all(|npc| clear(npc.position, 2.0))
        {
            return Some(position);
        }
    }
    fallback
}

/// NPCs stand on a ring around their place; the spot comes from a hash of the NPC id.
/// The places of the story that have background music.
pub fn area_descriptors(story: &Story, sites: &HashMap<String, Placed>) -> Vec<AreaDescriptor> {
    story
        .anchors
        .iter()
        .filter_map(|anchor| {
            let track = story.music.get(anchor.place.music.as_ref()?)?;
            let site = sites.get(&anchor.id)?;
            Some(AreaDescriptor {
                id: anchor.id.clone(),
                position: site_position(site),
                radius_m: anchor
                    .place
                    .music_radius_m
                    .or(anchor.place.radius_m)
                    .unwrap_or(40.0),
                music: AreaMusic {
                    url: media_url(&track.0),
                    title_key: track.1.clone(),
                    looped: track.2,
                },
            })
        })
        .collect()
}

pub fn npc_descriptors(
    story: &Story,
    sites: &HashMap<String, Placed>,
    ground: &dyn placement::Ground,
) -> Vec<NpcDescriptor> {
    let mut npcs = Vec::new();
    for npc in story.npcs.values() {
        let (Some(site), Some(anchor)) = (
            sites.get(&npc.place),
            story.anchors.iter().find(|anchor| anchor.id == npc.place),
        ) else {
            continue;
        };
        let hash = Sha256::digest(format!("ishtaria-npc:{}", npc.id).as_bytes());
        let sample = |index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        let reach = (anchor.place.radius_m.unwrap_or(20.0) * 0.4).clamp(3.0, 12.0);
        let direction = placement::offset(
            site.direction,
            2.0 + sample(0) * (reach - 2.0),
            sample(1) * std::f64::consts::TAU,
        );
        let height = ground.height(direction).max(0.0);
        npcs.push(NpcDescriptor {
            id: npc.id.clone(),
            name_key: npc.name_key.clone(),
            character: format!("{}/{}", npc.character.pack, npc.character.skin),
            model: npc.character.model.as_deref().map(media_url),
            position: direction.map(|axis| axis * (RADIUS + height)),
            yaw: sample(2) * std::f64::consts::TAU,
            scale_m: 1.8,
            portrait: npc.portrait.as_deref().map(media_url),
        });
    }
    npcs
}

/// Props of places near a point (buildings, graveyard fences, ships). Like NPCs, they never
/// disturb the rest of the world when the story is unavailable.
pub async fn props_near(state: &AppState, point: [f64; 3]) -> Vec<super::settlements::WorldProp> {
    const PROP_RADIUS_M: f64 = 450.0;
    const MAX_PROPS: usize = 2500;
    match world(state).await {
        Ok(Some(world)) => world
            .props
            .iter()
            .filter(|prop| {
                length(std::array::from_fn(|axis| {
                    prop.position[axis] - point[axis]
                })) <= PROP_RADIUS_M
            })
            .take(MAX_PROPS)
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Where new characters appear, if a datadisk defines a spawn point.
pub async fn spawn_position(state: &AppState) -> Option<[f64; 3]> {
    world(state)
        .await
        .ok()
        .flatten()
        .and_then(|world| world.spawn)
}

/// NPCs near a point. A broken or missing story never disturbs the rest of the world.
pub async fn npcs_near(state: &AppState, point: [f64; 3]) -> Vec<NpcDescriptor> {
    match world(state).await {
        Ok(Some(world)) => world
            .npcs
            .iter()
            .filter(|npc| {
                length(std::array::from_fn(|axis| npc.position[axis] - point[axis])) <= NPC_RADIUS_M
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Music areas whose region is near a point (the client works out the distance itself).
pub async fn areas_near(state: &AppState, point: [f64; 3]) -> Vec<AreaDescriptor> {
    match world(state).await {
        Ok(Some(world)) => world
            .areas
            .iter()
            .filter(|area| {
                length(std::array::from_fn(|axis| {
                    area.position[axis] - point[axis]
                })) <= area.radius_m + NPC_RADIUS_M
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Position of a place in world metres.
pub fn site_position(site: &Placed) -> [f64; 3] {
    site.direction.map(|axis| axis * (RADIUS + site.height_m))
}

/// Distance in metres between a player and an NPC.
pub fn distance(player: [f64; 3], npc: &NpcDescriptor) -> f64 {
    length(std::array::from_fn(|axis| {
        player[axis] - npc.position[axis]
    }))
}

/// Forgets the cached world (tests use a fresh database per test).
#[cfg(test)]
pub async fn reset_cache() {
    *WORLD.lock().await = None;
}
