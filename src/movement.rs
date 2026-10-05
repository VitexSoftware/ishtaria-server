use super::{environment, players, AppState};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use parry2d_f64::{
    na::{Isometry2, Vector2},
    query::{cast_shapes, ShapeCastOptions},
    shape::Ball,
};
use parry3d_f64::{
    na::{Isometry3, Point3, Translation3, UnitQuaternion, Vector3},
    query::{cast_shapes as cast_shapes3, contact, ShapeCastOptions as ShapeCastOptions3},
    shape::{Capsule, TriMesh},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};
use tokio::sync::Mutex;

#[cfg(test)]
use parry3d_f64::query::{Ray, RayCast};

pub(super) const RADIUS: f64 = 6_371_000.0;
const WALK_SPEED: f64 = 4.0;
const RUN_SPEED: f64 = 6.0;
pub(super) const OBJECT_GRID: i32 = 600_000;
/// Animals live on a coarser grid: about one cell per 170 metres.
const FAUNA_GRID: i32 = 60_000;
const FAUNA_PRESENCE: f64 = 0.35;
/// Fish are only placed where the water is at least this deep.
const MIN_WATER_DEPTH_M: f64 = 3.0;
/// Animals are sent to clients only within this distance of the requested point.
const FAUNA_RADIUS_M: f64 = 280.0;
const PLAYER_RADIUS: f64 = 0.35;
const JUMP_SPEED: f64 = 6.5;
const RUN_JUMP_SPEED: f64 = 8.5;
const GRAVITY: f64 = 9.81;
const COLLISION_SKIN: f64 = 0.001;
const WALKABLE_NORMAL: f64 = 0.8192319205190405;

fn flight_step(height: f64, velocity: f64, seconds: f64) -> (f64, f64) {
    (
        height + velocity * seconds - 0.5 * GRAVITY * seconds * seconds,
        velocity - GRAVITY * seconds,
    )
}
static TERRAIN: Mutex<Option<(String, Arc<WalkingTerrain>)>> = Mutex::const_new(None);
/// Obstacles that places add to the world (walls, fences, gravestones), for the terrain with
/// the given key `<seed>:<heightmap hash>`.
static STATICS: std::sync::RwLock<(String, Vec<WorldObject>)> =
    std::sync::RwLock::new((String::new(), Vec::new()));

pub(super) fn set_statics(key: String, obstacles: Vec<WorldObject>) {
    if let Ok(mut statics) = STATICS.write() {
        *statics = (key, obstacles);
    }
}
static COLLIDERS: OnceLock<HashMap<String, TriMesh>> = OnceLock::new();

#[derive(Deserialize)]
struct CollisionMesh {
    vertices: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
}

fn colliders() -> &'static HashMap<String, TriMesh> {
    COLLIDERS.get_or_init(|| {
        let source: HashMap<String, CollisionMesh> =
            serde_json::from_str(include_str!("../etc/world_colliders.json"))
                .expect("bundled collision geometry must be valid");
        source
            .into_iter()
            .map(|(id, mesh)| {
                (
                    id,
                    TriMesh::new(
                        mesh.vertices.into_iter().map(Point3::from).collect(),
                        mesh.triangles,
                    ),
                )
            })
            .collect()
    })
}

fn object_shape(object: &WorldObject) -> Option<(Isometry3<f64>, &'static TriMesh)> {
    let up = Vector3::from(unit(object.position));
    let rotation = UnitQuaternion::rotation_between(&Vector3::y(), &up)?
        * UnitQuaternion::from_axis_angle(&Vector3::y_axis(), object.yaw);
    Some((
        Isometry3::from_parts(Translation3::from(Vector3::from(object.position)), rotation),
        colliders().get(&object.model)?,
    ))
}

#[cfg(test)]
fn object_floor(point: [f64; 3], object: &WorldObject) -> Option<f64> {
    if object.collision_radius_m <= 0.0 {
        return None;
    }
    let (transform, mesh) = object_shape(object)?;
    let up = unit(point);
    let ray = Ray::new(
        transform.inverse_transform_point(&Point3::from(point)) / object.scale_m,
        transform.inverse_transform_vector(&-Vector3::from(up)),
    );
    let distance = mesh.cast_local_ray(&ray, 100.0 / object.scale_m, false)?;
    Some(length(point) - distance * object.scale_m)
}

fn object_capsule(
    point: [f64; 3],
    object_transform: &Isometry3<f64>,
    scale: f64,
) -> (Isometry3<f64>, Capsule) {
    let up = Vector3::from(unit(point));
    let rotation = UnitQuaternion::rotation_between(&Vector3::y(), &up).unwrap_or_default();
    let transform = Isometry3::from_parts(Translation3::from(Vector3::from(point)), rotation);
    let mut local_transform = object_transform.inverse() * transform;
    local_transform.translation.vector /= scale;
    (
        local_transform,
        Capsule::new(
            Point3::new(0.0, PLAYER_RADIUS / scale, 0.0),
            Point3::new(0.0, 1.45 / scale, 0.0),
            PLAYER_RADIUS / scale,
        ),
    )
}

fn capsule_hit(
    point: [f64; 3],
    motion: [f64; 3],
    objects: &[WorldObject],
) -> Option<(f64, [f64; 3])> {
    let mut closest: Option<(f64, [f64; 3])> = None;
    for object in objects
        .iter()
        .filter(|object| object.collision_radius_m > 0.0)
    {
        let Some((object_transform, mesh)) = object_shape(object) else {
            return Some((0.0, unit(motion).map(|value| -value)));
        };
        let (local_transform, body) = object_capsule(point, &object_transform, object.scale_m);
        let local_velocity =
            object_transform.inverse_transform_vector(&Vector3::from(motion)) / object.scale_m;
        match cast_shapes3(
            &local_transform,
            &local_velocity,
            &body,
            &Isometry3::identity(),
            &Vector3::zeros(),
            mesh,
            ShapeCastOptions3 {
                max_time_of_impact: 1.0,
                target_distance: COLLISION_SKIN / object.scale_m,
                stop_at_penetration: false,
                ..ShapeCastOptions3::default()
            },
        ) {
            Ok(Some(hit)) => {
                let normal: [f64; 3] =
                    (object_transform.rotation * hit.normal2.into_inner()).into();
                if dot(motion, normal) < -1e-10
                    && closest.map_or(true, |(time, _)| hit.time_of_impact < time)
                {
                    closest = Some((hit.time_of_impact.clamp(0.0, 1.0), normal));
                }
            }
            Ok(None) => {}
            Err(_) => return Some((0.0, unit(motion).map(|value| -value))),
        }
    }
    closest
}

fn recover_capsule(mut point: [f64; 3], objects: &[WorldObject]) -> [f64; 3] {
    for _ in 0..8 {
        let mut recovered = false;
        for object in objects
            .iter()
            .filter(|object| object.collision_radius_m > 0.0)
        {
            let Some((object_transform, mesh)) = object_shape(object) else {
                continue;
            };
            let (transform, body) = object_capsule(point, &object_transform, object.scale_m);
            if let Ok(Some(hit)) = contact(&transform, &body, &Isometry3::identity(), mesh, 0.0) {
                if hit.dist < -1e-7 / object.scale_m {
                    let normal: [f64; 3] =
                        (object_transform.rotation * hit.normal2.into_inner()).into();
                    let distance = (COLLISION_SKIN - hit.dist * object.scale_m).min(0.5);
                    point = std::array::from_fn(|axis| point[axis] + normal[axis] * distance);
                    recovered = true;
                }
            }
        }
        if !recovered {
            break;
        }
    }
    point
}

fn slide_capsule(
    mut point: [f64; 3],
    mut motion: [f64; 3],
    objects: &[WorldObject],
) -> ([f64; 3], Vec<[f64; 3]>) {
    let mut normals = Vec::new();
    for _ in 0..4 {
        if length(motion) < 1e-8 {
            break;
        }
        let Some((time, normal)) = capsule_hit(point, motion, objects) else {
            point = std::array::from_fn(|axis| point[axis] + motion[axis]);
            break;
        };
        point = std::array::from_fn(|axis| point[axis] + motion[axis] * time + normal[axis] * 1e-6);
        motion = motion.map(|value| value * (1.0 - time));
        let incoming = dot(motion, normal).min(0.0);
        motion = std::array::from_fn(|axis| motion[axis] - normal[axis] * incoming);
        normals.push(normal);
    }
    (point, normals)
}

#[cfg(test)]
fn clear_jump_path(point: [f64; 3], destination: [f64; 3], objects: &[WorldObject]) -> bool {
    capsule_hit(
        point,
        std::array::from_fn(|axis| destination[axis] - point[axis]),
        objects,
    )
    .is_none()
}

#[derive(Default)]
struct Flight {
    airborne: bool,
    vertical_speed: f64,
    horizontal: [f64; 3],
}

#[derive(Serialize, FromRow)]
pub(super) struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub sequence: String,
    pub airborne: bool,
    pub on_object: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    direction: [f64; 3],
    sequence: String,
    #[serde(default)]
    jump: bool,
    #[serde(default)]
    run: bool,
}

#[derive(Serialize)]
pub(super) struct Reply {
    position: Position,
    moving: bool,
    stats: players::Stats,
}

pub(super) struct WalkingTerrain {
    size: usize,
    pixels: Vec<u8>,
    environment: environment::Environment,
    catalog: Vec<ObjectKind>,
    removed: Removed,
}

/// Objects that players have harvested, with the unix second at which they grow back.
/// They are absent from rendering and collision until then.
#[derive(Default)]
struct Removed(std::sync::RwLock<HashMap<String, Remains>>);

/// When an object grows back and what, if anything, stands in its place meanwhile.
type Remains = (i64, Option<(String, f64)>);

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[derive(Deserialize)]
struct ObjectCatalog {
    version: u32,
    objects: Vec<ObjectKind>,
}

#[derive(Deserialize)]
struct ObjectKind {
    id: String,
    biomes: Vec<u8>,
    weight: f64,
    scale_m: f64,
    max_slope: f64,
    collision_radius: f64,
    /// Animals form their own layer with its own grid and ids, so adding them
    /// does not change where trees and rocks stand.
    #[serde(default)]
    fauna: bool,
    /// Swims below the surface of oceans, lakes and rivers instead of standing on land.
    #[serde(default)]
    aquatic: bool,
    /// Deepest the animal swims below the surface, in metres.
    #[serde(default = "default_depth")]
    depth_m: f64,
}

fn default_depth() -> f64 {
    8.0
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct WorldObject {
    pub id: String,
    pub model: String,
    pub position: [f64; 3],
    pub scale_m: f64,
    pub yaw: f64,
    pub collision_radius_m: f64,
    pub biome: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegionQuery {
    x: f64,
    y: f64,
    z: f64,
}

/// A world object as sent to clients, with what it can be harvested for.
#[derive(Serialize)]
struct ObjectDescriptor {
    #[serde(flatten)]
    object: WorldObject,
    /// `tree` or `rock` when the object can be harvested, with the tool it needs.
    #[serde(skip_serializing_if = "Option::is_none")]
    harvest: Option<super::gathering::HarvestInfo>,
}

#[derive(Serialize)]
pub(super) struct ObjectRegion {
    version: u32,
    heightmap_sha256: String,
    seed: String,
    objects: Vec<ObjectDescriptor>,
    /// Characters of the story datadisks standing nearby (passable for now).
    npcs: Vec<super::story::world::NpcDescriptor>,
    /// Buildings and props of towns, graveyards and harbours nearby.
    props: Vec<super::story::settlements::WorldProp>,
    memorials: Vec<super::survival::Memorial>,
}

fn object_catalog() -> Option<Vec<ObjectKind>> {
    let catalog: ObjectCatalog =
        serde_json::from_str(include_str!("../etc/world_objects.json")).ok()?;
    (catalog.version == 1).then_some(catalog.objects)
}

fn fauna_catalog() -> Option<Vec<ObjectKind>> {
    let catalog: ObjectCatalog =
        serde_json::from_str(include_str!("../etc/world_fauna.json")).ok()?;
    (catalog.version == 1).then(|| {
        catalog
            .objects
            .into_iter()
            .map(|kind| ObjectKind {
                fauna: true,
                ..kind
            })
            .collect()
    })
}

pub(super) fn unit(point: [f64; 3]) -> [f64; 3] {
    let radius = length(point);
    point.map(|value| value / radius)
}

pub(super) fn cross(first: [f64; 3], second: [f64; 3]) -> [f64; 3] {
    [
        first[1] * second[2] - first[2] * second[1],
        first[2] * second[0] - first[0] * second[2],
        first[0] * second[1] - first[1] * second[0],
    ]
}

pub(super) fn dot(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(first, second)| first * second)
        .sum()
}

fn cube_point(face: usize, horizontal: f64, vertical: f64) -> [f64; 3] {
    let cube: [f64; 3] = match face {
        0 => [1.0, vertical, -horizontal],
        1 => [-1.0, vertical, horizontal],
        2 => [horizontal, 1.0, -vertical],
        3 => [horizontal, -1.0, vertical],
        4 => [horizontal, vertical, 1.0],
        _ => [-horizontal, vertical, -1.0],
    };
    unit(std::array::from_fn(|axis| {
        let first = cube[(axis + 1) % 3].powi(2);
        let second = cube[(axis + 2) % 3].powi(2);
        cube[axis] * (1.0 - first / 2.0 - second / 2.0 + first * second / 3.0).sqrt()
    }))
}

fn clear_path(point: [f64; 3], destination: [f64; 3], objects: &[WorldObject]) -> bool {
    let up = unit(point);
    let reference = if up[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let sideways = unit(cross(up, reference));
    let forward = cross(up, sideways);
    let delta = std::array::from_fn(|axis| destination[axis] - point[axis]);
    let velocity = Vector2::new(dot(delta, sideways), dot(delta, forward));
    objects
        .iter()
        .filter(|object| object.collision_radius_m > 0.0)
        .all(|object| {
            let offset = std::array::from_fn(|axis| object.position[axis] - point[axis]);
            let center = Vector2::new(dot(offset, sideways), dot(offset, forward));
            let radius = PLAYER_RADIUS + object.collision_radius_m;
            if center.norm_squared() < radius * radius && (-center).dot(&velocity) > 0.0 {
                return true;
            }
            cast_shapes(
                &Isometry2::identity(),
                &velocity,
                &Ball::new(PLAYER_RADIUS),
                &Isometry2::translation(center.x, center.y),
                &Vector2::zeros(),
                &Ball::new(object.collision_radius_m),
                ShapeCastOptions {
                    max_time_of_impact: 1.0,
                    ..ShapeCastOptions::default()
                },
            )
            .is_ok_and(|hit| hit.is_none())
        })
}

pub(super) fn length(point: [f64; 3]) -> f64 {
    point.iter().map(|value| value * value).sum::<f64>().sqrt()
}

/// The map tile (face, column, row) of the object grid that contains a position.
pub(super) fn tile_of(point: [f64; 3]) -> (i32, i32, i32) {
    let (face, horizontal, vertical) = coordinates(unit(point));
    (
        face as i32,
        ((horizontal * f64::from(OBJECT_GRID)) as i32).clamp(0, OBJECT_GRID - 1),
        ((vertical * f64::from(OBJECT_GRID)) as i32).clamp(0, OBJECT_GRID - 1),
    )
}

fn coordinates(point: [f64; 3]) -> (usize, f64, f64) {
    let [horizontal, vertical, depth] = point;
    let magnitude = point.map(f64::abs);
    let (face, pair) = if magnitude[0] >= magnitude[1] && magnitude[0] >= magnitude[2] {
        if horizontal >= 0.0 {
            (0, [-depth, vertical])
        } else {
            (1, [depth, vertical])
        }
    } else if magnitude[1] >= magnitude[2] {
        if vertical >= 0.0 {
            (2, [horizontal, -depth])
        } else {
            (3, [horizontal, depth])
        }
    } else if depth >= 0.0 {
        (4, [horizontal, vertical])
    } else {
        (5, [-horizontal, vertical])
    };
    let squared = pair.map(|value| value * value);
    let result: [f64; 2] = std::array::from_fn(|axis| {
        let coefficient = 3.0 + 2.0 * (squared[axis] - squared[1 - axis]);
        pair[axis].signum()
            * (0.5
                * (coefficient
                    - (coefficient * coefficient - 24.0 * squared[axis])
                        .max(0.0)
                        .sqrt()))
            .max(0.0)
            .sqrt()
    });
    (
        face,
        ((result[0] + 1.0) * 0.5).clamp(0.0, 1.0),
        ((1.0 - result[1]) * 0.5).clamp(0.0, 1.0),
    )
}

/// The unit direction of a point on a cube face; both fractions run from 0 to 1.
pub(super) fn direction_at(face: usize, horizontal: f64, vertical: f64) -> [f64; 3] {
    cube_point(face, horizontal * 2.0 - 1.0, 1.0 - vertical * 2.0)
}

impl WalkingTerrain {
    /// For story placement: the biome and height of dry land in this direction, or
    /// nothing over water.
    pub(super) fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
        let height = self.surface(direction)?;
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        Some((self.environment.biomes[index], height))
    }

    /// True where the coarse environment map says open sea.
    pub(super) fn is_sea(&self, direction: [f64; 3]) -> bool {
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        self.environment.biomes[index] == 0
    }

    pub(super) fn heightmap_sha256(&self) -> &str {
        &self.environment.heightmap_sha256
    }

    pub(super) fn seed(&self) -> &str {
        &self.environment.seed
    }

    /// Height of the ground (or sea level surface) in this direction, including local relief.
    pub(super) fn ground_height(&self, direction: [f64; 3]) -> f64 {
        self.height(direction)
    }

    /// Finds a generated object by its id (`seed:face:column:row`) unless it is currently harvested.
    pub(super) fn object_by_id(&self, id: &str) -> Option<WorldObject> {
        if self.is_removed(id) {
            return None;
        }
        self.object_cell_by_id(id)
    }

    /// The generated object with this id, ignoring harvesting.
    pub(super) fn object_cell_by_id(&self, id: &str) -> Option<WorldObject> {
        let mut parts = id.rsplitn(4, ':');
        let row: i32 = parts.next()?.parse().ok()?;
        let column: i32 = parts.next()?.parse().ok()?;
        let face: usize = parts.next()?.parse().ok()?;
        let seed = parts.next()?;
        if seed != self.environment.seed
            || face > 5
            || !(0..OBJECT_GRID).contains(&column)
            || !(0..OBJECT_GRID).contains(&row)
        {
            return None;
        }
        self.object_cell(face, column, row)
            .filter(|object| object.id == id)
    }

    /// Hides an object until `until_unix`; a `leaves` model (and its size relative to the
    /// original) is shown in its place, for example a stump where a tree stood.
    /// Where a portal may be built: dry land. Returns the map position (face, column, row).
    pub(super) fn build_site(&self, point: [f64; 3]) -> Option<(i32, i32, i32)> {
        let direction = unit(point);
        self.surface(direction)?;
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let column = ((horizontal * size as f64) as usize).min(size - 1);
        let row = ((vertical * size as f64) as usize).min(size - 1);
        Some((face as i32, column as i32, row as i32))
    }

    pub(super) fn mark_removed(&self, id: &str, until_unix: i64, leaves: Option<(String, f64)>) {
        if let Ok(mut removed) = self.removed.0.write() {
            let now = unix_now();
            removed.retain(|_, (until, _)| *until > now);
            removed.insert(id.to_owned(), (until_unix, leaves));
        }
    }

    fn is_removed(&self, id: &str) -> bool {
        self.removed.0.read().is_ok_and(|removed| {
            removed
                .get(id)
                .is_some_and(|(until, _)| *until > unix_now())
        })
    }

    fn advance(
        &self,
        mut point: [f64; 3],
        intent: &Intent,
        seconds: f64,
        flight: &mut Flight,
        on_object: bool,
    ) -> ([f64; 3], bool) {
        let speed = if intent.run { RUN_SPEED } else { WALK_SPEED };
        if !flight.airborne && !intent.jump && !on_object {
            return (
                self.step_at_speed(point, intent.direction, seconds, speed)
                    .unwrap_or(point),
                false,
            );
        }
        if !flight.airborne && !on_object {
            point = unit(point).map(|value| value * (RADIUS + self.height(unit(point))));
        }
        let up = unit(point);
        let tangent = std::array::from_fn(|axis| {
            (intent.direction[axis] - up[axis] * dot(intent.direction, up)) * speed
        });
        if intent.jump && !flight.airborne {
            flight.airborne = true;
            flight.vertical_speed = if intent.run && length(tangent) > 1e-6 {
                RUN_JUMP_SPEED
            } else {
                JUMP_SPEED
            };
            flight.horizontal = tangent;
        } else if !flight.airborne {
            flight.horizontal = tangent;
        }
        let objects = self.walking_objects(point, 2);
        point = self.advance_over_objects(point, seconds, flight, &objects);
        (
            point,
            !flight.airborne && length(point) > RADIUS + self.height(unit(point)) + 0.02,
        )
    }

    fn advance_over_objects(
        &self,
        mut point: [f64; 3],
        seconds: f64,
        flight: &mut Flight,
        objects: &[WorldObject],
    ) -> [f64; 3] {
        let seconds = seconds.clamp(0.0, 0.25);
        let steps = (seconds / 0.016).ceil().max(1.0) as usize;
        let delta = seconds / steps as f64;
        point = recover_capsule(point, objects);
        for _ in 0..steps {
            let radius = length(point);
            let up = unit(point);
            let radial = dot(flight.horizontal, up);
            let lateral: [f64; 3] =
                std::array::from_fn(|axis| flight.horizontal[axis] - up[axis] * radial);
            let direction = unit(std::array::from_fn(|axis| {
                point[axis] + lateral[axis] * delta
            }));
            let lateral = if self.surface(direction).is_none() {
                flight.horizontal = [0.0; 3];
                [0.0; 3]
            } else {
                lateral
            };
            let (height, speed) = if flight.airborne {
                flight_step(radius, flight.vertical_speed, delta)
            } else {
                (radius, 0.0)
            };
            let motion =
                std::array::from_fn(|axis| lateral[axis] * delta + up[axis] * (height - radius));
            let normals;
            (point, normals) = slide_capsule(point, motion, objects);
            let up = unit(point);
            let mut velocity =
                std::array::from_fn(|axis| lateral[axis] + up[axis] * speed.max(-100.0));
            for normal in normals {
                let incoming = dot(velocity, normal).min(0.0);
                velocity = std::array::from_fn(|axis| velocity[axis] - normal[axis] * incoming);
            }
            let ground = RADIUS + self.surface(up).unwrap_or_else(|| self.height(up));
            let mut grounded = length(point) <= ground + COLLISION_SKIN && speed <= 0.0;
            if grounded {
                point = up.map(|value| value * ground);
            }
            if speed <= 0.0 {
                if let Some((time, normal)) =
                    capsule_hit(point, up.map(|value| -value * 0.05), objects)
                {
                    if dot(normal, up) >= WALKABLE_NORMAL {
                        point = std::array::from_fn(|axis| point[axis] - up[axis] * 0.05 * time);
                        grounded = true;
                    }
                }
            }
            flight.airborne = !grounded;
            flight.vertical_speed = if grounded {
                0.0
            } else {
                dot(velocity, up).clamp(-100.0, RUN_JUMP_SPEED)
            };
            flight.horizontal =
                std::array::from_fn(|axis| velocity[axis] - up[axis] * dot(velocity, up));
            let horizontal_speed = length(flight.horizontal);
            if horizontal_speed > RUN_SPEED {
                flight.horizontal = flight
                    .horizontal
                    .map(|value| value * RUN_SPEED / horizontal_speed);
            }
        }
        point
    }

    fn surface(&self, direction: [f64; 3]) -> Option<f64> {
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        if self.environment.biomes[index] < 3 || self.environment.water_m[index] != environment::DRY
        {
            return None;
        }
        let height = self.height(direction);
        (height > 0.0).then_some(height)
    }

    fn height(&self, direction: [f64; 3]) -> f64 {
        let (face, horizontal, vertical) = coordinates(direction);
        let horizontal = (horizontal * self.size as f64 - 0.5).clamp(0.0, (self.size - 1) as f64);
        let vertical = (vertical * self.size as f64 - 0.5).clamp(0.0, (self.size - 1) as f64);
        let column = horizontal as usize;
        let row = vertical as usize;
        let pixel = |column: usize, row: usize| {
            f64::from(self.pixels[row * self.size * 6 + face * self.size + column])
        };
        let interpolate =
            |first: f64, second: f64, fraction: f64| first + (second - first) * fraction;
        let upper = interpolate(
            pixel(column, row),
            pixel((column + 1).min(self.size - 1), row),
            horizontal.fract(),
        );
        let lower = interpolate(
            pixel(column, (row + 1).min(self.size - 1)),
            pixel(
                (column + 1).min(self.size - 1),
                (row + 1).min(self.size - 1),
            ),
            horizontal.fract(),
        );
        let base = interpolate(upper, lower, vertical.fract()) * 16000.0 / 255.0 - 8000.0;
        base + environment::relief_offset(direction, base, &self.environment.seed)
    }

    fn object_cell(&self, face: usize, column: i32, row: i32) -> Option<WorldObject> {
        let id = format!("{}:{face}:{column}:{row}", self.environment.seed);
        self.place(id, face, column, row, OBJECT_GRID, false)
    }

    fn fauna_cell(&self, face: usize, column: i32, row: i32) -> Option<WorldObject> {
        let id = format!("{}:fauna:{face}:{column}:{row}", self.environment.seed);
        self.place(id, face, column, row, FAUNA_GRID, true)
    }

    /// Water animals: placed below the water surface where the water is deep enough.
    fn place_aquatic(
        &self,
        id: String,
        point: [f64; 3],
        index: usize,
        biome: u8,
        sample: &dyn Fn(usize) -> f64,
    ) -> Option<WorldObject> {
        if sample(2) > FAUNA_PRESENCE {
            return None;
        }
        let level = f64::from(self.environment.water_m[index]);
        let depth = level - self.height(point);
        if depth < MIN_WATER_DEPTH_M {
            return None;
        }
        let candidates: Vec<_> = self
            .catalog
            .iter()
            .filter(|kind| kind.fauna && kind.aquatic && kind.biomes.contains(&biome))
            .collect();
        let total: f64 = candidates.iter().map(|kind| kind.weight).sum();
        let mut choice = sample(3) * total;
        let kind = candidates.into_iter().find(|kind| {
            choice -= kind.weight;
            choice <= 0.0
        })?;
        let deepest = kind.depth_m.min(depth - 1.5).max(1.0);
        let below = 0.8 + sample(6) * (deepest - 0.8).max(0.0);
        let scale_m = kind.scale_m * (0.8 + sample(5) * 0.4);
        Some(WorldObject {
            id,
            model: kind.id.clone(),
            position: point.map(|value| value * (RADIUS + level - below)),
            scale_m,
            yaw: sample(4) * std::f64::consts::TAU,
            collision_radius_m: 0.0,
            biome,
        })
    }

    /// Chooses the object of one grid cell from the hash of its identifier.
    fn place(
        &self,
        id: String,
        face: usize,
        column: i32,
        row: i32,
        grid: i32,
        fauna: bool,
    ) -> Option<WorldObject> {
        let hash = Sha256::digest(id.as_bytes());
        let sample = |index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        let point = cube_point(
            face,
            (f64::from(column) + 0.15 + sample(0) * 0.7) * 2.0 / f64::from(grid) - 1.0,
            1.0 - (f64::from(row) + 0.15 + sample(1) * 0.7) * 2.0 / f64::from(grid),
        );
        let (face, horizontal, vertical) = coordinates(point);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        let biome = self.environment.biomes[index];
        if fauna && biome < 3 {
            return self.place_aquatic(id, point, index, biome, &sample);
        }
        let height = self.surface(point)?;
        if sample(2)
            > if fauna {
                FAUNA_PRESENCE
            } else if biome == 5 {
                0.7
            } else {
                0.18
            }
        {
            return None;
        }
        let reference = if point[1].abs() < 0.9 {
            [0.0, 1.0, 0.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        let tangent = unit(cross(point, reference));
        let second = cross(point, tangent);
        let slope = [tangent, second]
            .into_iter()
            .map(|tangent| {
                (self.height(unit(std::array::from_fn(|axis| {
                    point[axis] + tangent[axis] * 0.00001
                }))) - height)
                    .abs()
                    / (RADIUS * 0.00001)
            })
            .fold(0.0, f64::max);
        let candidates: Vec<_> = self
            .catalog
            .iter()
            .filter(|kind| {
                kind.fauna == fauna
                    && !kind.aquatic
                    && kind.biomes.contains(&biome)
                    && slope <= kind.max_slope
            })
            .collect();
        let total: f64 = candidates.iter().map(|kind| kind.weight).sum();
        let mut choice = sample(3) * total;
        let kind = candidates.into_iter().find(|kind| {
            choice -= kind.weight;
            choice <= 0.0
        })?;
        let scale_m = kind.scale_m * (0.8 + sample(5) * 0.4);
        Some(WorldObject {
            id,
            model: kind.id.clone(),
            position: point.map(|value| value * (RADIUS + height)),
            scale_m,
            yaw: sample(4) * std::f64::consts::TAU,
            collision_radius_m: kind.collision_radius * scale_m,
            biome,
        })
    }

    /// Objects of the cells around a point; `cell` picks the object of one cell.
    fn collect(
        &self,
        point: [f64; 3],
        span: i32,
        grid: i32,
        cell: impl Fn(usize, i32, i32) -> Option<WorldObject>,
    ) -> Vec<WorldObject> {
        let (face, horizontal, vertical) = coordinates(unit(point));
        let center_column = (horizontal * f64::from(grid)) as i32;
        let center_row = (vertical * f64::from(grid)) as i32;
        let mut seen = HashSet::new();
        let mut objects = Vec::new();
        for row in center_row - span..=center_row + span {
            for column in center_column - span..=center_column + span {
                let (target, horizontal, vertical) = coordinates(cube_point(
                    face,
                    (f64::from(column) + 0.5) * 2.0 / f64::from(grid) - 1.0,
                    1.0 - (f64::from(row) + 0.5) * 2.0 / f64::from(grid),
                ));
                let column = ((horizontal * f64::from(grid)) as i32).min(grid - 1);
                let row = ((vertical * f64::from(grid)) as i32).min(grid - 1);
                if seen.insert((target, column, row)) {
                    if let Some(object) = cell(target, column, row) {
                        objects.push(object);
                    }
                }
            }
        }
        objects
    }

    /// Animals near a point. They are passable scenery for now: they stand where the
    /// seed puts them and do not move or collide.
    pub(super) fn fauna(&self, point: [f64; 3]) -> Vec<WorldObject> {
        let mut animals = self.collect(point, 2, FAUNA_GRID, |face, column, row| {
            self.fauna_cell(face, column, row)
        });
        animals.retain(|animal| {
            length(std::array::from_fn(|axis| {
                animal.position[axis] - point[axis]
            })) <= FAUNA_RADIUS_M
        });
        animals.sort_by(|first, second| first.id.cmp(&second.id));
        animals
    }

    /// What blocks a step: harvestable scenery plus the walls and fences of places.
    pub(super) fn walking_objects(&self, point: [f64; 3], span: i32) -> Vec<WorldObject> {
        let mut objects = self.objects(point, span);
        let key = format!(
            "{}:{}",
            self.environment.seed, self.environment.heightmap_sha256
        );
        if let Ok(statics) = STATICS.read() {
            if statics.0 == key {
                // Only what is near the walker matters; steps are a few metres long.
                objects.extend(
                    statics
                        .1
                        .iter()
                        .filter(|obstacle| {
                            (0..3).all(|axis| (obstacle.position[axis] - point[axis]).abs() < 60.0)
                        })
                        .cloned(),
                );
            }
        }
        objects
    }

    pub(super) fn objects(&self, point: [f64; 3], span: i32) -> Vec<WorldObject> {
        let mut objects = self.collect(point, span, OBJECT_GRID, |face, column, row| {
            self.object_cell(face, column, row)
        });
        if let Ok(removed) = self.removed.0.read() {
            let now = unix_now();
            objects = objects
                .into_iter()
                .filter_map(|mut object| match removed.get(&object.id) {
                    Some((until, leaves)) if *until > now => {
                        leaves.as_ref().map(|(model, scale)| {
                            // The remains are small and can be walked over.
                            object.model.clone_from(model);
                            object.scale_m *= scale;
                            object.collision_radius_m = 0.0;
                            object
                        })
                    }
                    _ => Some(object),
                })
                .collect();
        }
        objects.sort_by(|first, second| {
            let squared = |object: &WorldObject| {
                object
                    .position
                    .into_iter()
                    .zip(point)
                    .map(|(first, second)| (first - second).powi(2))
                    .sum::<f64>()
            };
            squared(first)
                .total_cmp(&squared(second))
                .then_with(|| first.id.cmp(&second.id))
        });
        objects
    }

    #[cfg(test)]
    fn step(&self, point: [f64; 3], intent: [f64; 3], seconds: f64) -> Option<[f64; 3]> {
        self.step_at_speed(point, intent, seconds, WALK_SPEED)
    }

    fn step_at_speed(
        &self,
        point: [f64; 3],
        intent: [f64; 3],
        seconds: f64,
        speed: f64,
    ) -> Option<[f64; 3]> {
        let radius = length(point);
        if !(RADIUS - 8000.0..=RADIUS + 8012.0).contains(&radius) {
            return None;
        }
        let up = point.map(|value| value / radius);
        let radial = intent
            .iter()
            .zip(up)
            .map(|(value, axis)| value * axis)
            .sum::<f64>();
        let tangent: [f64; 3] = std::array::from_fn(|axis| intent[axis] - up[axis] * radial);
        let distance = length(tangent) * speed * seconds.clamp(0.0, 0.25);
        if distance < 0.000001 {
            return None;
        }
        let intermediate: [f64; 3] = std::array::from_fn(|axis| {
            point[axis] + tangent[axis] * speed * seconds.clamp(0.0, 0.25)
        });
        let unit = intermediate.map(|value| value / length(intermediate));
        let height = self.surface(unit)?;
        let previous = self.surface(up)?;
        if (height - previous).abs() > distance * 0.7 {
            return None;
        }
        let destination = unit.map(|value| value * (RADIUS + height));
        clear_path(point, destination, &self.walking_objects(point, 2)).then_some(destination)
    }
}

pub(super) async fn terrain(state: &AppState) -> Result<Arc<WalkingTerrain>, players::Error> {
    let unavailable = || {
        players::Error::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "walking terrain unavailable",
        )
    };
    let (size, seed, sha256): (i32, String, String) =
        sqlx::query_as("SELECT face_size, seed, sha256 FROM heightmaps WHERE world_id = $1")
            .bind(state.world_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(unavailable)?;
    let key = format!("{}:{sha256}:{seed}:{size}", state.world_id);
    let mut cache = TERRAIN.lock().await;
    if let Some((stored, terrain)) = cache.as_ref() {
        if stored == &key {
            return Ok(terrain.clone());
        }
    }
    let pixels: Vec<u8> =
        sqlx::query_scalar("SELECT pixels FROM heightmaps WHERE world_id = $1 AND sha256 = $2")
            .bind(state.world_id)
            .bind(sha256.clone())
            .fetch_one(&state.pool)
            .await?;
    let terrain = tokio::task::spawn_blocking(move || {
        let mut catalog = object_catalog()?;
        catalog.extend(fauna_catalog()?);
        environment::generate(size as usize, &pixels, &seed, &sha256).map(|environment| {
            Arc::new(WalkingTerrain {
                size: size as usize,
                pixels,
                environment,
                catalog,
                removed: Removed::default(),
            })
        })
    })
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    let depleted: Vec<(String, i64)> = sqlx::query_as("SELECT object_id, extract(epoch FROM depleted_until)::bigint FROM world_object_state WHERE world_id = $1 AND depleted_until > now()")
        .bind(state.world_id)
        .fetch_all(&state.pool)
        .await?;
    for (id, until) in depleted {
        let leaves = terrain
            .object_cell_by_id(&id)
            .and_then(|object| super::gathering::leaves_for(&object.model));
        terrain.mark_removed(&id, until, leaves);
    }
    *cache = Some((key, terrain.clone()));
    Ok(terrain)
}

pub(super) async fn world_objects(
    State(state): State<AppState>,
    Query(query): Query<RegionQuery>,
) -> Result<Json<ObjectRegion>, players::Error> {
    let point = [query.x, query.y, query.z];
    if point.iter().any(|value| !value.is_finite())
        || !(RADIUS - 8000.0..=RADIUS + 8000.0).contains(&length(point))
    {
        return Err(players::Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid object region",
        ));
    }
    let terrain = terrain(&state).await?;
    let worker = terrain.clone();
    let objects = tokio::task::spawn_blocking(move || {
        let mut objects = worker.objects(point, 16);
        objects.truncate(512 - 64);
        let mut animals = worker.fauna(point);
        animals.truncate(64);
        objects.extend(animals);
        objects
    })
    .await
    .map_err(|_| {
        players::Error::Status(StatusCode::SERVICE_UNAVAILABLE, "object region unavailable")
    })?;
    Ok(Json(ObjectRegion {
        memorials: super::survival::memorials(&state, unit(point)).await?,
        npcs: super::story::world::npcs_near(&state, point).await,
        props: super::story::world::props_near(&state, point).await,
        version: 1,
        heightmap_sha256: terrain.environment.heightmap_sha256.clone(),
        seed: terrain.environment.seed.clone(),
        objects: objects
            .into_iter()
            .map(|object| ObjectDescriptor {
                harvest: super::gathering::harvest_info(&object.model),
                object,
            })
            .collect(),
    }))
}

pub(super) async fn world_memorials(
    State(state): State<AppState>,
    Query(query): Query<RegionQuery>,
) -> Result<Json<Vec<super::survival::Memorial>>, players::Error> {
    let point = [query.x, query.y, query.z];
    if point.iter().any(|value| !value.is_finite())
        || !(RADIUS - 8000.0..=RADIUS + 8000.0).contains(&length(point))
    {
        return Err(players::Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid memorial region",
        ));
    }
    Ok(Json(super::survival::memorials(&state, unit(point)).await?))
}

pub(super) async fn walk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut intent): Json<Intent>,
) -> Result<Json<Reply>, players::Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let sequence = intent
        .sequence
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0 && value.to_string() == intent.sequence);
    if sequence.is_none()
        || intent.direction.iter().any(|value| !value.is_finite())
        || length(intent.direction) > 1.000001
    {
        return Err(players::Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid movement intent",
        ));
    }
    let sequence = sequence.unwrap();
    let terrain = terrain(&state).await?;
    let _ = super::story::world::world(&state).await; // loads the walls and fences that block steps
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&mut transaction, state.world_id, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(players::Error::Obituary(notice));
    }
    let mut position: Position = sqlx::query_as("SELECT position_x AS x, position_y AS y, position_z AS z, movement_sequence::text AS sequence, movement_airborne AS airborne, movement_support AS on_object FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE")
        .bind(player_id).fetch_optional(&mut *transaction).await?
        .ok_or(players::Error::Status(StatusCode::CONFLICT, "player position unavailable"))?;
    let mut moving = false;
    if sequence > position.sequence.parse::<i64>().unwrap_or(i64::MAX) {
        let stamina: i32 = sqlx::query_scalar("SELECT stamina FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(&mut *transaction)
            .await?;
        intent.run &= stamina > 0;
        let seconds: f64 = sqlx::query_scalar("SELECT greatest(0.0, least(0.25, extract(epoch FROM (clock_timestamp() - last_moved_at))))::float8 FROM players WHERE id = $1")
            .bind(player_id).fetch_one(&mut *transaction).await?;
        let (vertical_speed, jump_x, jump_y, jump_z): (f64, f64, f64, f64) = sqlx::query_as(
            "SELECT jump_vertical_speed, jump_x, jump_y, jump_z FROM players WHERE id = $1",
        )
        .bind(player_id)
        .fetch_one(&mut *transaction)
        .await?;
        let mut flight = Flight {
            airborne: position.airborne,
            vertical_speed,
            horizontal: [jump_x, jump_y, jump_z],
        };
        let previous = [position.x, position.y, position.z];
        let (point, on_object) =
            terrain.advance(previous, &intent, seconds, &mut flight, position.on_object);
        [position.x, position.y, position.z] = point;
        position.airborne = flight.airborne;
        position.on_object = on_object;
        moving = length(std::array::from_fn(|axis| point[axis] - previous[axis])) > 0.000001;
        position.sequence = sequence.to_string();
        sqlx::query("UPDATE players SET position_x = $2, position_y = $3, position_z = $4, movement_sequence = $5, last_moved_at = clock_timestamp(), movement_airborne = $6, movement_support = $7, jump_vertical_speed = $8, jump_x = $9, jump_y = $10, jump_z = $11 WHERE id = $1")
            .bind(player_id).bind(position.x).bind(position.y).bind(position.z).bind(sequence)
            .bind(position.airborne).bind(on_object).bind(flight.vertical_speed)
            .bind(flight.horizontal[0]).bind(flight.horizontal[1]).bind(flight.horizontal[2])
            .execute(&mut *transaction).await?;
        if moving
            && !super::survival::activity(&mut transaction, player_id, seconds, intent.run).await?
        {
            let notice = super::survival::obituary(&mut transaction, player_id).await?;
            transaction.commit().await?;
            return Err(players::Error::Obituary(notice));
        }
    }
    let stats = players::stats(&mut transaction, player_id).await?;
    transaction.commit().await?;
    Ok(Json(Reply {
        position,
        moving,
        stats,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_capsule_clear(point: [f64; 3], object: &WorldObject) {
        let (transform, mesh) = object_shape(object).unwrap();
        let (body_transform, body) = object_capsule(point, &transform, object.scale_m);
        let hit = contact(&body_transform, &body, &Isometry3::identity(), mesh, 0.0).unwrap();
        assert!(
            hit.map_or(true, |hit| hit.dist * object.scale_m >= -0.00001),
            "capsule penetrates {} at {point:?}: {hit:?}",
            object.model
        );
    }

    fn assert_supported(point: [f64; 3], object: &WorldObject) {
        assert_capsule_clear(point, object);
        let up = unit(point);
        let (time, normal) = capsule_hit(
            point,
            up.map(|value| -value * 0.05),
            std::slice::from_ref(object),
        )
        .unwrap();
        assert!(time < 0.01 && dot(normal, up) >= WALKABLE_NORMAL);
    }

    #[test]
    fn jump_has_a_bounded_apex_and_returns_to_the_ground() {
        let apex_time = JUMP_SPEED / GRAVITY;
        let (height, velocity) = flight_step(0.0, JUMP_SPEED, apex_time);
        assert!((height - JUMP_SPEED * JUMP_SPEED / (2.0 * GRAVITY)).abs() < 1e-10);
        assert!(height > 2.0 && height < 2.2);
        assert!(velocity.abs() < 1e-10);
        let (height, velocity) = flight_step(height, velocity, apex_time);
        assert!(height.abs() < 1e-10);
        assert!((velocity + JUMP_SPEED).abs() < 1e-10);
    }

    #[test]
    fn jump_lands_on_original_rock_geometry() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let object = WorldObject {
            id: "rock".into(),
            model: "nature.rock".into(),
            position: [RADIUS + height, 0.0, 0.0],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 1.0,
            biome: 4,
        };
        let floor = object_floor([RADIUS + height + 10.0, 0.0, 0.0], &object).unwrap();
        assert!(floor > RADIUS + height + 0.1);
        let mut point = [floor + 1.0, 0.0, 0.0];
        let mut flight = Flight {
            airborne: true,
            vertical_speed: -1.0,
            horizontal: [0.0; 3],
        };
        for _ in 0..20 {
            point = terrain.advance_over_objects(
                point,
                0.1,
                &mut flight,
                std::slice::from_ref(&object),
            );
        }
        assert!(!flight.airborne);
        assert!(length(point) >= floor);
        assert_supported(point, &object);
        let previous = point;
        point = terrain.advance_over_objects(point, 0.25, &mut flight, &[object]);
        assert!(length(std::array::from_fn(|axis| point[axis] - previous[axis])) < 0.00001);
    }

    #[test]
    fn directional_jump_cannot_restart_in_air_and_returns_to_ground() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let mut point = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let mut flight = Flight::default();
        (point, _) = terrain.advance(
            point,
            &Intent {
                direction: [0.0, 0.0, 1.0],
                jump: true,
                ..Intent::default()
            },
            0.1,
            &mut flight,
            false,
        );
        assert!(flight.airborne && point[2] > 0.39 && point[2] < 0.41);
        let velocity = flight.vertical_speed;
        (point, _) = terrain.advance(
            point,
            &Intent {
                direction: [0.0, 0.0, -1.0],
                jump: true,
                ..Intent::default()
            },
            0.1,
            &mut flight,
            false,
        );
        assert!(flight.vertical_speed < velocity && point[2] > 0.79);
        for _ in 0..20 {
            (point, _) = terrain.advance(point, &Intent::default(), 0.1, &mut flight, false);
        }
        assert!(!flight.airborne);
        assert!((length(point) - RADIUS - terrain.height(unit(point))).abs() < 0.000001);
    }

    #[test]
    fn directional_jump_clears_rock_side_and_lands_on_top() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let object_direction = unit([RADIUS + height, 0.0, 5.2]);
        let object = WorldObject {
            id: "rock".into(),
            model: "nature.rock".into(),
            position: object_direction
                .map(|value| value * (RADIUS + terrain.height(object_direction))),
            scale_m: 2.0,
            yaw: 0.0,
            collision_radius_m: 2.0,
            biome: 4,
        };
        let mut point = [RADIUS + height, 0.0, 0.0];
        assert!(!clear_jump_path(
            point,
            object.position,
            std::slice::from_ref(&object)
        ));
        let high = unit(point).map(|value| value * (length(point) + 5.0));
        let high_end = object_direction.map(|value| value * length(high));
        assert!(clear_jump_path(
            high,
            high_end,
            std::slice::from_ref(&object)
        ));
        let mut flight = Flight {
            airborne: true,
            vertical_speed: JUMP_SPEED,
            horizontal: [0.0, 0.0, WALK_SPEED],
        };
        for _ in 0..40 {
            point = terrain.advance_over_objects(
                point,
                0.05,
                &mut flight,
                std::slice::from_ref(&object),
            );
            if !flight.airborne {
                break;
            }
        }
        assert!(!flight.airborne);
        let support = object_floor(
            unit(point).map(|value| value * (length(point) + 0.05)),
            &object,
        );
        assert!(
            support.is_some(),
            "directional jump must land on the actual rock, not bypass it: {point:?}"
        );
        assert!(length(point) >= support.unwrap());
        assert_supported(point, &object);
    }

    #[test]
    fn descending_capsule_slides_off_steep_rock_without_getting_stuck() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let original_center = [5830295.028572648, 2142795.0373625495, -1419450.942963967];
        let original_point = [5830295.971761411, 2142794.0988089717, -1419451.9966884125];
        let up = unit(original_center);
        let center = up.map(|value| value * (RADIUS + terrain.height(up)));
        let object = WorldObject {
            id: "steep-rock".into(),
            model: "nature.stone_largeD".into(),
            position: center,
            scale_m: 3.0614058257080616,
            yaw: 5.782347146974182,
            collision_radius_m: 1.8062294371677563,
            biome: 4,
        };
        let offset: [f64; 3] =
            std::array::from_fn(|axis| original_point[axis] - original_center[axis]);
        let mut point = std::array::from_fn(|axis| center[axis] + offset[axis] + up[axis] * 2.0);
        let mut flight = Flight {
            airborne: true,
            vertical_speed: -1.0,
            horizontal: [0.0; 3],
        };
        let sample = |point: [f64; 3], flight: &Flight| {
            serde_json::json!({
                "offset": std::array::from_fn::<_, 3, _>(|axis| point[axis] - center[axis]),
                "airborne": flight.airborne,
                "on_object": !flight.airborne && length(point) > RADIUS + terrain.height(unit(point)) + 0.02,
            })
        };
        let mut trajectory = vec![sample(point, &flight)];
        for _ in 0..40 {
            point = terrain.advance_over_objects(
                point,
                0.05,
                &mut flight,
                std::slice::from_ref(&object),
            );
            assert_capsule_clear(point, &object);
            trajectory.push(sample(point, &flight));
        }
        assert!(!flight.airborne, "a side impact must eventually settle");
        let before = point;
        let outward = unit(std::array::from_fn(|axis| {
            offset[axis] - up[axis] * dot(offset, up)
        }));
        flight.horizontal = outward.map(|value| value * WALK_SPEED);
        point =
            terrain.advance_over_objects(point, 0.25, &mut flight, std::slice::from_ref(&object));
        assert!(
            dot(
                std::array::from_fn(|axis| point[axis] - before[axis]),
                outward
            ) > 0.2,
            "the character must be able to walk away after a side landing: {before:?} -> {point:?}"
        );
        assert_capsule_clear(point, &object);
        trajectory.push(sample(point, &flight));
        point = std::array::from_fn(|axis| center[axis] + offset[axis]);
        flight = Flight {
            horizontal: outward.map(|value| value * WALK_SPEED),
            ..Flight::default()
        };
        let before = point;
        let mut recovery = vec![sample(point, &flight)];
        for _ in 0..40 {
            point = terrain.advance_over_objects(
                point,
                0.05,
                &mut flight,
                std::slice::from_ref(&object),
            );
            assert_capsule_clear(point, &object);
            recovery.push(sample(point, &flight));
        }
        assert!(!flight.airborne);
        assert!(
            dot(
                std::array::from_fn(|axis| point[axis] - before[axis]),
                outward
            ) > 0.2,
            "persisted overlap must recover without a database reset"
        );
        if let Some(path) = std::env::var_os("ISHTARIA_SLIDE_CAPTURE") {
            let capture = serde_json::json!({"object": object, "trajectory": trajectory, "recovery": recovery});
            std::fs::write(path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        }
    }

    #[test]
    fn running_is_bounded_and_launch_speed_survives_shift_release() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let start = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let walking = Intent {
            direction: [0.0, 0.0, 1.0],
            ..Intent::default()
        };
        let running = Intent {
            direction: walking.direction,
            run: true,
            ..Intent::default()
        };
        let mut flight = Flight::default();
        let (point, _) = terrain.advance(start, &running, 100.0, &mut flight, false);
        assert!((point[2] - 1.5).abs() < 0.00001);
        let (point, _) = terrain.advance(point, &walking, 0.25, &mut flight, false);
        assert!((point[2] - 2.5).abs() < 0.00001);
        let jumping = Intent {
            direction: walking.direction,
            jump: true,
            run: true,
            ..Intent::default()
        };
        let (point, _) = terrain.advance(start, &jumping, 0.1, &mut flight, false);
        assert!(flight.airborne && (point[2] - 0.6).abs() < 0.00001);
        let (point, _) = terrain.advance(point, &walking, 0.1, &mut flight, false);
        assert!(flight.airborne && (point[2] - 1.2).abs() < 0.00001);
        assert!(length(flight.horizontal) <= RUN_SPEED + 1e-8);
        let stationary_jump = Intent {
            jump: true,
            run: true,
            ..Intent::default()
        };
        let mut stationary_flight = Flight::default();
        terrain.advance(start, &stationary_jump, 0.05, &mut stationary_flight, false);
        assert!(stationary_flight.vertical_speed < JUMP_SPEED);
        let trajectories = [false, true].map(|run| {
            let mut flight = Flight::default();
            let mut point = start;
            let mut apex: f64 = 0.0;
            let mut duration = 0.0;
            for step in 0..60 {
                let intent = Intent {
                    direction: walking.direction,
                    jump: step == 0,
                    run: run && step == 0,
                    ..Intent::default()
                };
                (point, _) = terrain.advance(point, &intent, 0.05, &mut flight, false);
                apex = apex.max(length(point) - length(start));
                duration += 0.05;
                if !flight.airborne {
                    break;
                }
            }
            assert!(!flight.airborne);
            (apex, duration, point[2])
        });
        let [walking_jump, running_jump] = trajectories;
        assert!((2.0..2.3).contains(&walking_jump.0));
        assert!((3.5..3.9).contains(&running_jump.0));
        assert!(running_jump.1 > walking_jump.1 * 1.2);
        assert!(running_jump.2 > walking_jump.2 * 1.5);
        terrain.environment.biomes.fill(0);
        assert!(terrain
            .step_at_speed(start, running.direction, 0.25, RUN_SPEED)
            .is_none());
        let object = WorldObject {
            id: "run-wall".into(),
            model: "nature.stone_largeD".into(),
            position: [start[0], 0.0, 1.0],
            scale_m: 3.0,
            yaw: 0.0,
            collision_radius_m: 1.0,
            biome: 4,
        };
        assert!(!clear_path(start, [start[0], 0.0, 1.5], &[object]));
    }

    #[test]
    fn walking_is_bounded_and_cannot_fly_or_swim() {
        let pixels = vec![144; 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let mut terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let point = [RADIUS + height, 0.0, 0.0];
        let moved = terrain.step(point, [0.0, 0.0, 1.0], 100.0).unwrap();
        assert!((length(moved) - (RADIUS + terrain.height(unit(moved)))).abs() < 1e-8);
        assert!((length(moved) - length(point)).abs() < 0.7);
        assert!((moved[2] - 1.0).abs() < 1e-6);
        assert!(terrain.step(point, [1.0, 0.0, 0.0], 0.25).is_none());
        assert!(terrain.step(point, [0.0; 3], 0.25).is_none());
        terrain.environment.biomes.fill(0);
        assert!(terrain.step(point, [0.0, 0.0, 1.0], 0.25).is_none());
    }

    #[test]
    fn swept_obstacles_block_tunnelling_but_allow_escape() {
        let point = [RADIUS, 0.0, 0.0];
        let mut object = WorldObject {
            id: "test".into(),
            model: "rock".into(),
            position: [RADIUS, 0.0, 0.5],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 0.1,
            biome: 4,
        };
        assert!(!clear_path(point, [RADIUS, 0.0, 1.0], &[object.clone()]));
        assert!(clear_path(point, [RADIUS, 1.0, 0.0], &[object.clone()]));
        object.position[2] = 0.2;
        assert!(clear_path(point, [RADIUS, 0.0, -1.0], &[object.clone()]));
        assert!(!clear_path(point, [RADIUS, 0.0, 1.0], &[object.clone()]));
        object.collision_radius_m = 0.0;
        assert!(clear_path(point, [RADIUS, 0.0, 1.0], &[object]));
    }

    #[test]
    fn walls_of_places_block_steps_for_the_terrain_they_belong_to() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut environment = environment::generate(16, &pixels, "42", "walls").unwrap();
        environment.water_m.fill(environment::DRY);
        environment.biomes.fill(4);
        let terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        let height = terrain.height([1.0, 0.0, 0.0]);
        let origin = [RADIUS + height, 0.0, 0.0];
        let direction = unit(cross(unit(origin), [0.0, 1.0, 0.0]));
        let wall_at = |meters: f64| -> [f64; 3] {
            unit(std::array::from_fn(|axis| {
                origin[axis] + direction[axis] * meters
            }))
            .map(|value| value * (RADIUS + height))
        };
        let wall = WorldObject {
            id: "world:town_00#1.0".into(),
            model: String::new(),
            position: wall_at(1.0),
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 1.25,
            biome: 4,
        };
        // Without the obstacle (or for another terrain) the step is free.
        set_statics("other:key".into(), vec![wall.clone()]);
        assert!(terrain.step(origin, direction, 0.25).is_some());
        set_statics("42:walls".into(), vec![wall]);
        assert!(
            terrain.step(origin, direction, 0.25).is_none(),
            "the wall blocks the step"
        );
        assert!(
            terrain
                .step(origin, direction.map(|value| -value), 0.25)
                .is_some(),
            "walking away is allowed"
        );
        set_statics(String::new(), Vec::new());
    }

    #[test]
    fn authoritative_objects_are_stable_and_block_actual_steps() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut environment = environment::generate(16, &pixels, "42", "test").unwrap();
        environment.water_m.fill(environment::DRY);
        environment.biomes.fill(5);
        let terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: object_catalog().unwrap(),
            removed: Default::default(),
        };
        let height = terrain.height([1.0, 0.0, 0.0]);
        let point = [RADIUS + height, 0.0, 0.0];
        let objects = terrain.objects(point, 16);
        assert_eq!(objects, terrain.objects(point, 16));
        assert!(objects.len() > 100 && objects.len() <= 1089);
        let obstacle = objects
            .iter()
            .find(|object| object.collision_radius_m > 0.0)
            .unwrap();
        let direction = unit(obstacle.position);
        let tangent = unit(cross(direction, [0.0, 1.0, 0.0]));
        let distance = obstacle.collision_radius_m + PLAYER_RADIUS + 0.1;
        let start = unit(std::array::from_fn(|axis| {
            obstacle.position[axis] - tangent[axis] * distance
        }))
        .map(|value| value * (RADIUS + height));
        assert!(terrain.step(start, tangent, 0.25).is_none());
        assert!(terrain
            .step(start, tangent.map(|value| -value), 0.25)
            .is_some());
        let seam = terrain.objects(
            unit([1.0, 0.0, 1.0]).map(|value| value * (RADIUS + height)),
            16,
        );
        assert!(seam
            .iter()
            .any(|object| object.id.split(':').nth(1) == Some("0")));
        assert!(seam
            .iter()
            .any(|object| object.id.split(':').nth(1) == Some("4")));
        assert_eq!(
            seam.iter()
                .map(|object| &object.id)
                .collect::<HashSet<_>>()
                .len(),
            seam.len()
        );
    }

    #[test]
    fn every_nature_variant_can_be_selected_on_authoritative_terrain() {
        let pixels = vec![144; 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: object_catalog().unwrap(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        let mut selected = HashSet::new();
        for biome in 3..=7 {
            terrain.environment.biomes.fill(biome);
            for row in 300_000..300_150 {
                for column in 300_000..300_150 {
                    if let Some(object) = terrain.object_cell(0, column, row) {
                        assert!(object.collision_radius_m <= 20.0);
                        assert!(
                            (length(object.position)
                                - RADIUS
                                - terrain.height(unit(object.position)))
                            .abs()
                                < 0.000001
                        );
                        selected.insert(object.model);
                    }
                }
            }
        }
        for kind in terrain
            .catalog
            .iter()
            .filter(|kind| kind.id.starts_with("nature."))
        {
            assert!(
                selected.contains(&kind.id),
                "unreachable model: {}",
                kind.id
            );
        }
    }

    fn terrain_with(pixel: u8, biome: u8, catalog: Vec<ObjectKind>) -> WalkingTerrain {
        let pixels = vec![pixel; 16 * 16 * 6];
        let mut environment = environment::generate(16, &pixels, "42", "test").unwrap();
        environment.biomes.fill(biome);
        if biome < 3 {
            environment.water_m.fill(0);
        } else {
            environment.water_m.fill(environment::DRY);
        }
        WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog,
            removed: Default::default(),
        }
    }

    fn catalog_with_fauna() -> Vec<ObjectKind> {
        let mut catalog = object_catalog().unwrap();
        catalog.extend(fauna_catalog().unwrap());
        catalog
    }

    #[test]
    fn animals_do_not_change_where_trees_and_rocks_stand() {
        let flora = terrain_with(144, 5, object_catalog().unwrap());
        let both = terrain_with(144, 5, catalog_with_fauna());
        let point = [RADIUS + flora.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        assert!(!flora.objects(point, 16).is_empty());
        assert_eq!(flora.objects(point, 16), both.objects(point, 16));
    }

    #[test]
    fn land_animals_are_deterministic_passable_and_fit_their_biome() {
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let point = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let mut found = Vec::new();
        // Animals are sparse: look around a few places on the map.
        for step in 0..40 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            let origin = place.map(|value| value * (RADIUS + terrain.height(place)));
            found.extend(terrain.fauna(origin));
            assert_eq!(terrain.fauna(origin), terrain.fauna(origin));
        }
        assert!(!found.is_empty(), "grassland has animals near {point:?}");
        for animal in &found {
            assert!(animal.id.contains(":fauna:"), "{}", animal.id);
            assert!(animal.model.starts_with("animal."), "{}", animal.model);
            assert_eq!(animal.collision_radius_m, 0.0, "animals are passable");
            assert!(
                terrain.object_by_id(&animal.id).is_none(),
                "animals are not harvestable"
            );
        }
        let kinds: std::collections::HashSet<_> =
            found.iter().map(|animal| &animal.model).collect();
        assert!(
            kinds.iter().all(|model| [
                "animal.cow",
                "animal.bull",
                "animal.horse",
                "animal.white_horse",
                "animal.donkey",
                "animal.deer",
                "animal.fox",
                "animal.shiba_inu",
                "animal.alpaca"
            ]
            .contains(&model.as_str())),
            "{kinds:?}"
        );
    }

    #[test]
    fn fish_swim_below_the_surface_of_deep_water_only() {
        let sea = terrain_with(80, 0, catalog_with_fauna());
        let mut fish = Vec::new();
        for step in 0..40 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            fish.extend(sea.fauna(place.map(|value| value * RADIUS)));
        }
        assert!(!fish.is_empty(), "the sea has fish");
        for animal in &fish {
            assert!(animal.model.starts_with("fish."), "{}", animal.model);
            let below = RADIUS - length(animal.position);
            assert!(
                (0.7..=60.5).contains(&below),
                "{} swims {below} m below the surface",
                animal.id
            );
            assert_eq!(animal.collision_radius_m, 0.0);
        }
        // Water above the seabed by less than three metres gets no fish; land gets no fish.
        let shallow = terrain_with(128, 0, catalog_with_fauna());
        let start = [RADIUS, 0.0, 0.0];
        assert!(shallow.fauna(start).is_empty());
        let land = terrain_with(144, 4, catalog_with_fauna());
        assert!(land
            .fauna([RADIUS + 1000.0, 0.0, 0.0])
            .iter()
            .all(|animal| animal.model.starts_with("animal.")));
        // Fresh water only holds freshwater fish.
        let lake = terrain_with(80, 1, catalog_with_fauna());
        let mut lake_fish = Vec::new();
        for step in 0..60 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            lake_fish.extend(lake.fauna(place.map(|value| value * RADIUS)));
        }
        assert!(
            lake_fish.iter().all(|animal| [
                "fish.armored_catfish",
                "fish.betta",
                "fish.blue_goldfish",
                "fish.flower_horn",
                "fish.goldfish",
                "fish.koi",
                "fish.piranha",
                "fish.tetra"
            ]
            .contains(&animal.model.as_str())),
            "{:?}",
            lake_fish.iter().map(|a| &a.model).collect::<Vec<_>>()
        );
    }
}
