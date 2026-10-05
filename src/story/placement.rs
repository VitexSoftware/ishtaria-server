//! Deterministic placement of datadisk places on a planet.
//!
//! Candidates come from a hash of `(seed, place id, attempt)`, so the same seed, map and
//! disks always give the same sites. Results are persisted by the caller: once a place has
//! a site it never moves.

use super::{disk::BIOMES, Anchor, Story};
use crate::movement::{cross, direction_at, dot, unit, WalkingTerrain, RADIUS};
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const FREE_ATTEMPTS: u32 = 200_000;
const RELATIVE_ATTEMPTS: u32 = 5_000;
const DEFAULT_RADIUS_M: f64 = 20.0;
const DEFAULT_MAX_SLOPE_DEG: f64 = 20.0;
const SLOPE_PROBE_M: f64 = 40.0;
/// Where a place is allowed to stand inside its parent (fraction of the parent radius).
const PARENT_FILL: f64 = 0.8;

/// What placement needs to know about the ground.
pub trait Ground {
    /// Biome number and height of dry land in a direction; `None` over water.
    fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)>;
    /// Ground height in a direction.
    fn height(&self, direction: [f64; 3]) -> f64;
    /// Open sea in a direction.
    fn is_sea(&self, direction: [f64; 3]) -> bool;
}

impl Ground for WalkingTerrain {
    fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
        WalkingTerrain::land_site(self, direction)
    }
    fn height(&self, direction: [f64; 3]) -> f64 {
        self.ground_height(direction)
    }
    fn is_sea(&self, direction: [f64; 3]) -> bool {
        WalkingTerrain::is_sea(self, direction)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    /// Qualified place id, `<disk>:<place>`.
    pub id: String,
    /// Unit vector from the planet centre.
    pub direction: [f64; 3],
    pub height_m: f64,
}

fn samples(seed: &str, id: &str, attempt: u32) -> [f64; 8] {
    let hash = Sha256::digest(format!("ishtaria-story:{seed}:{id}:{attempt}").as_bytes());
    std::array::from_fn(|index| {
        f64::from(u32::from_le_bytes(
            hash[index * 4..index * 4 + 4].try_into().unwrap(),
        )) / 4_294_967_296.0
    })
}

/// Great-circle distance in metres.
pub fn distance_m(first: [f64; 3], second: [f64; 3]) -> f64 {
    RADIUS * dot(first, second).clamp(-1.0, 1.0).acos()
}

/// The point `meters` away from `from` in the compass direction `azimuth` (radians).
pub fn offset(from: [f64; 3], meters: f64, azimuth: f64) -> [f64; 3] {
    let reference = if from[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let east = unit(cross(from, reference));
    let north = cross(from, east);
    let angle = meters / RADIUS;
    let tangent: [f64; 3] =
        std::array::from_fn(|axis| azimuth.cos() * east[axis] + azimuth.sin() * north[axis]);
    unit(std::array::from_fn(|axis| {
        from[axis] * angle.cos() + tangent[axis] * angle.sin()
    }))
}

/// How far from a coastal place the sea may start.
pub const COAST_M: f64 = 300.0;

/// The nearest sea around `direction` within `max_m`: the heading in the site's local plane
/// (unit `(x, z)`) and the distance to the first water.
pub fn sea_toward(
    ground: &dyn Ground,
    direction: [f64; 3],
    max_m: f64,
) -> Option<((f64, f64), f64)> {
    let mut best: Option<((f64, f64), f64)> = None;
    for step in 0..16 {
        let angle = f64::from(step) * std::f64::consts::TAU / 16.0;
        let heading = (angle.cos(), angle.sin());
        let mut meters = 15.0;
        while meters <= max_m {
            let (probe, _) = crate::scenery::world_from_local(
                direction,
                heading.0 * meters,
                heading.1 * meters,
                |_| 0.0,
                RADIUS,
            );
            if ground.is_sea(probe) {
                if best.is_none_or_closer(meters) {
                    best = Some((heading, meters));
                }
                break;
            }
            meters += 15.0;
        }
    }
    best
}

/// A cheap test whether the sea lies within `max_m`: a few probes in eight directions.
pub fn sea_near(ground: &dyn Ground, direction: [f64; 3], max_m: f64) -> bool {
    (0..8).any(|step| {
        let angle = f64::from(step) * std::f64::consts::TAU / 8.0;
        [max_m * 0.5, max_m].iter().any(|meters| {
            let (probe, _) = crate::scenery::world_from_local(
                direction,
                angle.cos() * meters,
                angle.sin() * meters,
                |_| 0.0,
                RADIUS,
            );
            ground.is_sea(probe)
        })
    })
}

trait Closer {
    fn is_none_or_closer(&self, meters: f64) -> bool;
}

impl Closer for Option<((f64, f64), f64)> {
    fn is_none_or_closer(&self, meters: f64) -> bool {
        self.as_ref().map_or(true, |(_, best)| meters < *best)
    }
}

fn radius_of(anchor: &Anchor) -> f64 {
    anchor.place.radius_m.unwrap_or(DEFAULT_RADIUS_M)
}

fn slope_deg(ground: &dyn Ground, direction: [f64; 3], height: f64) -> f64 {
    let steepest = (0..4)
        .map(|quarter| {
            let probe = offset(
                direction,
                SLOPE_PROBE_M,
                f64::from(quarter) * std::f64::consts::FRAC_PI_2,
            );
            (ground.height(probe) - height).abs()
        })
        .fold(0.0, f64::max);
    (steepest / SLOPE_PROBE_M).atan().to_degrees()
}

fn is_ancestor(story: &Story, descendant: &Anchor, target: &str) -> bool {
    let mut current = descendant;
    while let Some(parent) = current.place.parent.as_deref() {
        if parent == target {
            return true;
        }
        match story.anchors.iter().find(|anchor| anchor.id == parent) {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

fn related(story: &Story, first: &Anchor, second: &Anchor) -> bool {
    is_ancestor(story, first, &second.id) || is_ancestor(story, second, &first.id)
}

/// Places every anchor of `story` that is not in `existing`, in dependency order. Returns
/// only the new sites. Fails, naming the place, when no site satisfies its requirements.
pub fn place_anchors(
    story: &Story,
    ground: &dyn Ground,
    seed: &str,
    existing: &[Placed],
) -> Result<Vec<Placed>> {
    let mut placed: HashMap<String, Placed> = existing
        .iter()
        .map(|site| (site.id.clone(), site.clone()))
        .collect();
    let mut fresh = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut remaining: Vec<&Anchor> = story
        .anchors
        .iter()
        .filter(|a| !placed.contains_key(&a.id))
        .collect();
    while !remaining.is_empty() {
        let position = remaining
            .iter()
            .position(|anchor| {
                anchor
                    .place
                    .parent
                    .iter()
                    .map(String::as_str)
                    .chain(anchor.place.requires.near.iter().map(|n| n.place.as_str()))
                    .all(|dependency| placed.contains_key(dependency))
            })
            .ok_or_else(|| anyhow::anyhow!("places depend on each other in a cycle"))?;
        let anchor = remaining.remove(position);
        let site = match find_site(story, anchor, ground, seed, &placed) {
            Ok(site) => site,
            // A generated settlement that finds no room is simply left out, with what depends on it.
            Err(error) if anchor.id.starts_with("world:") => {
                eprintln!("{error:#}");
                skipped.push(anchor.id.clone());
                remaining.retain(|other| {
                    !other
                        .place
                        .parent
                        .iter()
                        .map(String::as_str)
                        .chain(other.place.requires.near.iter().map(|n| n.place.as_str()))
                        .any(|dependency| skipped.iter().any(|gone| gone == dependency))
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        placed.insert(site.id.clone(), site.clone());
        fresh.push(site);
    }
    Ok(fresh)
}

fn find_site(
    story: &Story,
    anchor: &Anchor,
    ground: &dyn Ground,
    seed: &str,
    placed: &HashMap<String, Placed>,
) -> Result<Placed> {
    let requires = &anchor.place.requires;
    let allowed: Vec<u8> = requires
        .biome
        .iter()
        .filter_map(|name| {
            BIOMES
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, number)| *number)
        })
        .collect();
    let center = anchor
        .place
        .parent
        .as_ref()
        .map(|parent| {
            (
                parent,
                0.0,
                radius_of(
                    story
                        .anchors
                        .iter()
                        .find(|a| &a.id == parent)
                        .expect("checked at compose"),
                ) * PARENT_FILL,
            )
        })
        .or_else(|| {
            requires
                .near
                .first()
                .map(|near| (&near.place, near.min_m, near.max_m))
        });
    let attempts = if center.is_some() {
        RELATIVE_ATTEMPTS
    } else {
        FREE_ATTEMPTS
    };
    for attempt in 0..attempts {
        let sample = samples(seed, &anchor.id, attempt);
        let direction = match center {
            Some((origin, minimum, maximum)) => {
                let meters = (minimum * minimum
                    + sample[3] * (maximum * maximum - minimum * minimum))
                    .sqrt();
                offset(
                    placed[origin].direction,
                    meters,
                    sample[4] * std::f64::consts::TAU,
                )
            }
            None => direction_at((sample[0] * 6.0) as usize % 6, sample[1], sample[2]),
        };
        let Some((biome, height)) = ground.land_site(direction) else {
            continue;
        };
        if !allowed.is_empty() && !allowed.contains(&biome) {
            continue;
        }
        if let Some(range) = &requires.height_m {
            if range.min.is_some_and(|min| height < min)
                || range.max.is_some_and(|max| height > max)
            {
                continue;
            }
        }
        if requires.near.iter().any(|near| {
            let meters = distance_m(direction, placed[&near.place].direction);
            meters < near.min_m || meters > near.max_m
        }) {
            continue;
        }
        match requires.coast {
            Some(true) if !sea_near(ground, direction, COAST_M) => continue,
            Some(false) if sea_near(ground, direction, COAST_M * 2.0) => continue,
            _ => {}
        }
        if let Some(spacing) = anchor.place.spacing_m {
            let crowded = story.anchors.iter().any(|other| {
                other.id != anchor.id
                    && other.place.kind == anchor.place.kind
                    && placed
                        .get(&other.id)
                        .is_some_and(|site| distance_m(direction, site.direction) < spacing)
            });
            if crowded {
                continue;
            }
        }
        let separated = story.anchors.iter().all(|other| {
            other.id == anchor.id
                || related(story, anchor, other)
                || placed.get(&other.id).map_or(true, |site| {
                    distance_m(direction, site.direction) >= radius_of(anchor) + radius_of(other)
                })
        });
        if !separated {
            continue;
        }
        if slope_deg(ground, direction, height)
            > requires.max_slope_deg.unwrap_or(DEFAULT_MAX_SLOPE_DEG)
        {
            continue;
        }
        return Ok(Placed {
            id: anchor.id.clone(),
            direction,
            height_m: height,
        });
    }
    bail!(
        "no site found for place {} after {attempts} attempts",
        anchor.id
    )
}
