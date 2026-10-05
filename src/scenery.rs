//! Generated scenery: buildings and props built from the Kenney kits around a site.
//!
//! Everything here works in the *local plane* of a site, in metres: `x` and `z` are the axes of
//! the client's own local frame at the site (see [`world_from_local`]), `yaw` is the model's
//! rotation about the up axis exactly as the client applies it, and `y` is the height above the
//! ground. The layouts are pure functions of a seed, so the same site always gives the same town.
//!
//! Models are named `<kit>.<name>`: `graveyard`, `town` (Fantasy Town Kit), `castle`, `retro`
//! (Retro Fantasy Kit) and `pirate`. One kit unit is [`MODULE_M`] metres unless a prop says otherwise.

use sha2::{Digest, Sha256};
use std::f64::consts::{FRAC_PI_2, PI, TAU};

/// Metres in one unit of a kit's modular pieces (a wall panel is one unit wide and high).
pub const MODULE_M: f64 = 2.5;
/// Castle pieces are larger so that a town wall is not made of thousands of panels.
const WALL_M: f64 = 4.0;
const GRAVE_M: f64 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub struct LocalProp {
    pub model: String,
    pub x: f64,
    pub z: f64,
    /// Height above the ground at the prop's position (metres).
    pub y: f64,
    pub yaw: f64,
    /// Metres per model unit.
    pub scale: f64,
    /// Stands at sea level instead of on the ground (ships and docks).
    pub at_sea: bool,
}

impl LocalProp {
    fn new(model: &str, x: f64, z: f64, yaw: f64, scale: f64) -> Self {
        Self {
            model: model.to_owned(),
            x,
            z,
            y: 0.0,
            yaw,
            scale,
            at_sea: false,
        }
    }

    fn high(mut self, y: f64) -> Self {
        self.y = y;
        self
    }

    fn sea(mut self) -> Self {
        self.at_sea = true;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Hamlet,
    Village,
    Town,
}

impl Size {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "hamlet" => Some(Self::Hamlet),
            "village" => Some(Self::Village),
            "town" => Some(Self::Town),
            _ => None,
        }
    }

    /// Radius of the built-up area in metres.
    pub fn radius_m(self) -> f64 {
        match self {
            Self::Hamlet => 34.0,
            Self::Village => 55.0,
            Self::Town => 85.0,
        }
    }

    fn houses(self) -> usize {
        match self {
            Self::Hamlet => 7,
            Self::Village => 16,
            Self::Town => 34,
        }
    }
}

/// Deterministic random numbers for one layout.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: &str, id: &str) -> Self {
        let hash = Sha256::digest(format!("ishtaria-scenery:{seed}:{id}").as_bytes());
        Self(u64::from_le_bytes(hash[..8].try_into().unwrap()) | 1)
    }

    pub fn next(&mut self) -> f64 {
        // SplitMix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn between(&mut self, low: f64, high: f64) -> f64 {
        low + self.next() * (high - low)
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[((self.next() * items.len() as f64) as usize).min(items.len() - 1)]
    }
}

/// Rotates a vector about the up axis the way the client rotates a model by `yaw`.
fn rotate(x: f64, z: f64, yaw: f64) -> (f64, f64) {
    (
        x * yaw.cos() + z * yaw.sin(),
        -x * yaw.sin() + z * yaw.cos(),
    )
}

/// The yaw that makes a model's front (+Z) point along the horizontal direction `(dx, dz)`.
fn facing(dx: f64, dz: f64) -> f64 {
    dx.atan2(dz)
}

/// A house of `width` x 2 modules, door and windows in front (+Z of the house), gable roof.
/// `(cx, cz)` is its centre, `yaw` turns it as a whole.
fn house(rng: &mut Rng, cx: f64, cz: f64, yaw: f64, width: usize, props: &mut Vec<LocalProp>) {
    let m = MODULE_M;
    let place =
        |lx: f64, lz: f64, y: f64, model: &str, own_yaw: f64, props: &mut Vec<LocalProp>| {
            let (rx, rz) = rotate(lx, lz, yaw);
            props.push(LocalProp::new(model, cx + rx, cz + rz, yaw + own_yaw, m).high(y));
        };
    let door_cell = rng.between(0.0, width as f64) as usize;
    for i in 0..width {
        for j in 0..2usize {
            let lx = (i as f64 - (width as f64 - 1.0) / 2.0) * m;
            let lz = (j as f64 - 0.5) * m;
            let side = |edge_yaw: f64, model: &str, props: &mut Vec<LocalProp>| {
                place(lx, lz, 0.0, model, edge_yaw, props)
            };
            if i + 1 == width {
                side(0.0, "town.wall", props);
            }
            if i == 0 {
                side(PI, "town.wall", props);
            }
            if j == 1 {
                // The front: a door in one cell, shuttered windows in the others.
                let model = if i == door_cell {
                    "town.wall-door"
                } else {
                    "town.wall-window-shutters"
                };
                side(-FRAC_PI_2, model, props);
            }
            if j == 0 {
                let model = if rng.next() < 0.4 {
                    "town.wall-window-small"
                } else {
                    "town.wall"
                };
                side(FRAC_PI_2, model, props);
            }
            // The gable roof: the rear row slopes towards the back, the front row to the front.
            let roof_yaw = if j == 1 { FRAC_PI_2 } else { -FRAC_PI_2 };
            place(lx, lz, m, "town.roof", roof_yaw, props);
        }
    }
    // A chimney on the rear slope.
    let (rx, rz) = rotate(0.0, -0.6 * m, yaw);
    props.push(LocalProp::new("town.chimney", cx + rx, cz + rz, yaw, m).high(m * 1.25));
}

/// A settlement: plaza with fountain and stalls, crossing roads, houses facing the plaza,
/// trees, and for a town a wall with gates and towers. `sea` is the horizontal unit direction to
/// the nearest sea and its distance in metres when the settlement is by the coast.
pub fn settlement(
    seed: &str,
    id: &str,
    size: Size,
    sea: Option<((f64, f64), f64)>,
) -> Vec<LocalProp> {
    let mut rng = Rng::new(seed, id);
    let radius = size.radius_m();
    let mut props = Vec::new();

    // Plaza and roads.
    props.push(LocalProp::new(
        "town.fountain-round",
        0.0,
        0.0,
        0.0,
        MODULE_M * 1.2,
    ));
    let mut step = 6.0;
    while step <= radius {
        for sign in [-1.0, 1.0] {
            props.push(LocalProp::new("town.road", sign * step, 0.0, 0.0, MODULE_M));
            props.push(LocalProp::new("town.road", 0.0, sign * step, 0.0, MODULE_M));
        }
        step += MODULE_M;
    }
    for k in 0..4 {
        let angle = f64::from(k) * FRAC_PI_2 + FRAC_PI_2 / 2.0;
        let model = if k % 2 == 0 {
            "town.stall-red"
        } else {
            "town.stall-green"
        };
        props.push(LocalProp::new(
            model,
            8.5 * angle.cos(),
            8.5 * angle.sin(),
            facing(-angle.cos(), -angle.sin()),
            MODULE_M,
        ));
    }

    // Barrels and crates by the stalls.
    for (k, model) in [
        "retro.barrels",
        "retro.detail-crate",
        "retro.barrels",
        "retro.detail-crate",
    ]
    .iter()
    .enumerate()
    {
        let angle = k as f64 * FRAC_PI_2 + FRAC_PI_2 / 2.0 + 0.22;
        props.push(LocalProp::new(
            model,
            9.6 * angle.cos(),
            9.6 * angle.sin(),
            angle,
            MODULE_M,
        ));
    }

    // Houses on the free ground between the roads, their doors towards the plaza.
    let wanted = size.houses();
    let mut centers: Vec<(f64, f64)> = vec![(0.0, 0.0)];
    let mut tries = 0;
    while centers.len() <= wanted && tries < 4000 {
        tries += 1;
        let angle = rng.between(0.0, TAU);
        let distance = rng.between(15.0, radius - 6.0);
        let (x, z) = (distance * angle.cos(), distance * angle.sin());
        if x.abs() < 6.0 || z.abs() < 6.0 {
            continue;
        }
        if centers.iter().any(|(ox, oz)| (ox - x).hypot(oz - z) < 14.0) {
            continue;
        }
        centers.push((x, z));
        let width = 2 + (rng.next() * 2.0) as usize;
        house(&mut rng, x, z, facing(-x, -z), width, &mut props);
    }
    // Trees in the gaps.
    for _ in 0..wanted {
        let angle = rng.between(0.0, TAU);
        let distance = rng.between(12.0, radius);
        let (x, z) = (distance * angle.cos(), distance * angle.sin());
        if x.abs() < 5.0
            || z.abs() < 5.0
            || centers.iter().any(|(ox, oz)| (ox - x).hypot(oz - z) < 9.0)
        {
            continue;
        }
        let model = *rng.pick(&[
            "town.tree",
            "town.tree-crooked",
            "town.tree-high",
            "retro.tree-large",
            "retro.tree-shrub",
        ]);
        props.push(LocalProp::new(model, x, z, rng.between(0.0, TAU), MODULE_M));
    }

    if size == Size::Town {
        wall(
            &mut props,
            radius + 12.0,
            sea.map(|(direction, _)| direction),
        );
    }
    if let Some((direction, distance)) = sea {
        harbour(&mut rng, &mut props, direction, distance.min(300.0));
    }
    props
}

/// A 16-sided wall of castle pieces with a tower at every corner and a gate on each road.
fn wall(props: &mut Vec<LocalProp>, radius: f64, sea: Option<(f64, f64)>) {
    const SIDES: usize = 16;
    let corner = |k: usize| {
        let angle = k as f64 * TAU / SIDES as f64;
        (radius * angle.cos(), radius * angle.sin())
    };
    for k in 0..SIDES {
        let (ax, az) = corner(k);
        let (bx, bz) = corner((k + 1) % SIDES);
        let length = (bx - ax).hypot(bz - az);
        let pieces = (length / WALL_M).round().max(1.0) as usize;
        let along = (bx - ax).atan2(bz - az); // heading of the side
        let middle = (ax + bx) / 2.0;
        let middle_z = (az + bz) / 2.0;
        // Sides that cross the roads get a gate in the middle; the seaward one stays open.
        let on_road = middle.abs() < length || middle_z.abs() < length;
        let towards_sea = sea.is_some_and(|(sx, sz)| (middle * sx + middle_z * sz) > radius * 0.8);
        for p in 0..pieces {
            let t = (p as f64 + 0.5) / pieces as f64;
            let (x, z) = (ax + (bx - ax) * t, az + (bz - az) * t);
            let centre_piece = p == pieces / 2;
            let model = if centre_piece && on_road && !towards_sea {
                "castle.wall-doorway"
            } else if towards_sea && p % 3 == 1 {
                "castle.wall-half"
            } else {
                "castle.wall"
            };
            // Castle wall pieces run along their local X axis: turn that along the side.
            props.push(LocalProp::new(model, x, z, along - FRAC_PI_2, WALL_M));
        }
    }
    for k in 0..SIDES {
        let (x, z) = corner(k);
        let yaw = facing(-x, -z);
        props.push(LocalProp::new(
            "castle.tower-square-base",
            x,
            z,
            yaw,
            WALL_M * 1.1,
        ));
        props.push(
            LocalProp::new("castle.tower-square-mid", x, z, yaw, WALL_M * 1.1).high(WALL_M * 1.1),
        );
        props.push(
            LocalProp::new("castle.tower-square-top-roof", x, z, yaw, WALL_M * 1.1)
                .high(WALL_M * 2.2),
        );
    }
}

/// A landing on the shore with a shipyard hut, crates and moored boats. `direction` is where the
/// sea lies; `distance` is how far away the water starts. Returns nothing: the props are added.
fn harbour(rng: &mut Rng, props: &mut Vec<LocalProp>, direction: (f64, f64), distance: f64) {
    let (dx, dz) = direction;
    let yaw = facing(dx, dz);
    let at = |along: f64, across: f64| (dx * along - dz * across, dz * along + dx * across);
    // The shipyard hut on the shore, its front to the water.
    let (x, z) = at(distance - 10.0, 0.0);
    props.push(LocalProp::new(
        "pirate.structure",
        x,
        z,
        yaw,
        MODULE_M * 1.6,
    ));
    props.push(
        LocalProp::new("pirate.structure-roof", x, z, yaw, MODULE_M * 1.6).high(MODULE_M * 1.6),
    );
    for (k, model) in [
        "pirate.crate",
        "pirate.barrel",
        "pirate.chest",
        "pirate.crate-bottles",
        "retro.pulley-crate",
    ]
    .iter()
    .enumerate()
    {
        let (cx, cz) = at(distance - 5.0, 4.0 + k as f64 * 1.6 - 3.0);
        props.push(LocalProp::new(
            model,
            cx,
            cz,
            rng.between(0.0, TAU),
            MODULE_M,
        ));
    }
    // A pier reaching 24 metres into the water.
    let mut along = distance - 4.0;
    while along < distance + 24.0 {
        let (px, pz) = at(along, 0.0);
        props.push(
            LocalProp::new(
                "pirate.structure-platform-dock",
                px,
                pz,
                yaw,
                MODULE_M * 1.2,
            )
            .sea(),
        );
        along += MODULE_M * 1.2;
    }
    // Boats and ships on both sides of the pier.
    let fleet = [
        "pirate.ship-small",
        "pirate.boat-row-small",
        "pirate.ship-medium",
        "pirate.boat-row-large",
        "pirate.ship-large",
    ];
    for (k, model) in fleet.iter().enumerate() {
        let side = if k % 2 == 0 { 1.0 } else { -1.0 };
        let (px, pz) = at(
            distance + 14.0 + (k / 2) as f64 * 12.0,
            side * (9.0 + (k / 2) as f64 * 2.0),
        );
        let scale = if model.contains("boat") {
            1.2
        } else if model.contains("large") {
            1.1
        } else {
            0.9
        };
        props.push(LocalProp::new(model, px, pz, yaw + rng.between(-0.15, 0.15), scale).sea());
    }
}

/// A graveyard: an iron fence with a gate, rows of gravestones and crosses, a crypt, lamps and pines.
pub fn graveyard(seed: &str, id: &str, radius: f64) -> Vec<LocalProp> {
    let mut rng = Rng::new(seed, id);
    let mut props = Vec::new();
    // The fence: one piece is a metre long, scaled to GRAVE_M.
    let sides = ((TAU * radius) / GRAVE_M).round() as usize;
    for k in 0..sides {
        let angle = k as f64 * TAU / sides as f64;
        let (x, z) = (radius * angle.cos(), radius * angle.sin());
        // The gate faces +Z (south on the plane): the pieces nearest to it open up.
        let model = if (angle - FRAC_PI_2).abs() < 0.12 {
            "graveyard.iron-fence-border-gate"
        } else if rng.next() < 0.08 {
            "graveyard.iron-fence-damaged"
        } else {
            "graveyard.iron-fence"
        };
        props.push(LocalProp::new(
            model,
            x,
            z,
            facing(angle.cos(), angle.sin()) + FRAC_PI_2,
            GRAVE_M,
        ));
    }
    // The crypt at the back, facing the gate.
    props.push(LocalProp::new(
        "graveyard.crypt-large",
        0.0,
        -radius * 0.55,
        0.0,
        GRAVE_M,
    ));
    props.push(LocalProp::new(
        "graveyard.crypt-large-roof",
        0.0,
        -radius * 0.55,
        0.0,
        GRAVE_M,
    ));
    props.push(LocalProp::new(
        "graveyard.crypt-large-door",
        0.0,
        -radius * 0.55,
        0.0,
        GRAVE_M,
    ));
    // Rows of gravestones, all looking at the gate.
    let stones = [
        "graveyard.gravestone-round",
        "graveyard.gravestone-cross",
        "graveyard.gravestone-wide",
        "graveyard.gravestone-bevel",
        "graveyard.gravestone-decorative",
        "graveyard.gravestone-broken",
        "graveyard.cross",
    ];
    let mut z = -radius * 0.2;
    while z < radius - 4.0 {
        let mut x = -radius + 5.0;
        while x < radius - 4.0 {
            if (x * x + z * z).sqrt() < radius - 3.0 && x.abs() > 2.0 && rng.next() < 0.7 {
                let model = *rng.pick(&stones);
                props.push(LocalProp::new(
                    model,
                    x + rng.between(-0.3, 0.3),
                    z,
                    rng.between(-0.15, 0.15) + PI,
                    GRAVE_M,
                ));
                if rng.next() < 0.5 {
                    props.push(LocalProp::new(
                        "graveyard.grave",
                        x,
                        z - 1.5,
                        PI,
                        GRAVE_M * 0.7,
                    ));
                }
            }
            x += 3.2;
        }
        z += 3.4;
    }
    for (x, z) in [
        (-radius * 0.5, radius * 0.7),
        (radius * 0.5, radius * 0.7),
        (-radius * 0.4, -radius * 0.2),
        (radius * 0.4, -radius * 0.2),
    ] {
        props.push(LocalProp::new(
            "graveyard.lightpost-single",
            x,
            z,
            0.0,
            GRAVE_M,
        ));
    }
    // Pines outside the fence.
    for _ in 0..9 {
        let angle = rng.between(0.0, TAU);
        let distance = radius + rng.between(3.0, 10.0);
        let model = *rng.pick(&[
            "graveyard.pine",
            "graveyard.pine-crooked",
            "graveyard.pine-fall",
        ]);
        props.push(LocalProp::new(
            model,
            distance * angle.cos(),
            distance * angle.sin(),
            rng.between(0.0, TAU),
            GRAVE_M * 0.9,
        ));
    }
    props
}

/// A round obstacle in a site's local plane that players cannot walk through.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Collider {
    pub x: f64,
    pub z: f64,
    pub radius: f64,
}

/// The obstacles a prop makes. Walls are chains of circles along their panels; doors, gates,
/// roofs, piers, ships and floor details leave the way open.
pub fn colliders(prop: &LocalProp) -> Vec<Collider> {
    let name = prop.model.as_str();
    let scale = prop.scale;
    // A wall panel sits on the +X edge of its cell: move the circle there, along the panel's yaw.
    let panel = |radius: f64| {
        let (dx, dz) = rotate(0.45 * scale, 0.0, prop.yaw);
        vec![Collider {
            x: prop.x + dx,
            z: prop.z + dz,
            radius: radius * scale,
        }]
    };
    let round = |radius: f64| {
        vec![Collider {
            x: prop.x,
            z: prop.z,
            radius: radius * scale,
        }]
    };
    match name {
        "town.wall" | "town.wall-window-shutters" | "town.wall-window-small" => panel(0.5),
        "castle.wall" | "castle.wall-half" => round(0.5),
        "castle.tower-square-base" => round(0.6),
        "town.fountain-round" => round(0.9),
        "town.stall-red" | "town.stall-green" => round(0.5),
        "town.tree" | "town.tree-crooked" | "town.tree-high" | "retro.tree-large" => round(0.12),
        "retro.barrels" | "retro.detail-crate" | "retro.pulley-crate" => round(0.4),
        "graveyard.iron-fence" | "graveyard.iron-fence-damaged" => round(0.5),
        "graveyard.crypt-large" => round(1.1),
        "graveyard.lightpost-single" => round(0.1),
        "graveyard.pine" | "graveyard.pine-crooked" | "graveyard.pine-fall" => round(0.2),
        "pirate.structure" => round(0.5),
        "pirate.crate" | "pirate.barrel" | "pirate.chest" | "pirate.crate-bottles" => round(0.4),
        _ if name.starts_with("graveyard.gravestone") || name == "graveyard.cross" => round(0.2),
        _ => Vec::new(),
    }
}

/// Every model name the generators can use, for checking the bundled kits.
#[cfg(test)]
pub fn used_models() -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for size in [Size::Hamlet, Size::Village, Size::Town] {
        for sea in [None, Some(((0.0, 1.0), 200.0))] {
            for seed in 0..6 {
                for prop in settlement(&seed.to_string(), "probe", size, sea) {
                    models.push(prop.model);
                }
            }
        }
    }
    for prop in graveyard("1", "probe", 16.0) {
        models.push(prop.model);
    }
    models.sort();
    models.dedup();
    models
}

/// World metres of a point at local `(x, z)` around `site` (a unit direction from the planet's
/// centre), given the ground height there. The local axes are the client's: the rotation that
/// carries +Y to the site's up direction by the shortest arc applied to +X and +Z.
pub fn world_from_local(
    site: [f64; 3],
    x: f64,
    z: f64,
    height: impl Fn([f64; 3]) -> f64,
    radius: f64,
) -> ([f64; 3], [f64; 3]) {
    let (axis_x, axis_z) = client_axes(site);
    let ground =
        std::array::from_fn(|axis| site[axis] * radius + axis_x[axis] * x + axis_z[axis] * z);
    let length = ground.iter().map(|value| value * value).sum::<f64>().sqrt();
    let direction = ground.map(|value| value / length);
    let metres = height(direction);
    (direction, direction.map(|value| value * (radius + metres)))
}

/// The client's local +X and +Z axes at a site (see [`world_from_local`]).
pub fn client_axes(up: [f64; 3]) -> ([f64; 3], [f64; 3]) {
    let cos = up[1].clamp(-1.0, 1.0);
    let axis = [up[2], 0.0, -up[0]]; // +Y x up
    let norm = (axis[0] * axis[0] + axis[2] * axis[2]).sqrt();
    if norm < 1e-12 {
        return if cos > 0.0 {
            ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0])
        } else {
            ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0])
        };
    }
    let k = [axis[0] / norm, 0.0, axis[2] / norm];
    let sin = (1.0 - cos * cos).sqrt();
    let rotate = |v: [f64; 3]| {
        // Rodrigues: v cos + (k x v) sin + k (k . v) (1 - cos)
        let cross = [
            k[1] * v[2] - k[2] * v[1],
            k[2] * v[0] - k[0] * v[2],
            k[0] * v[1] - k[1] * v[0],
        ];
        let dot = k[0] * v[0] + k[1] * v[1] + k[2] * v[2];
        std::array::from_fn(|i| v[i] * cos + cross[i] * sin + k[i] * dot * (1.0 - cos))
    };
    (rotate([1.0, 0.0, 0.0]), rotate([0.0, 0.0, 1.0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_deterministic_and_use_known_models() {
        let first = settlement("42", "world:town_1", Size::Village, None);
        assert_eq!(first, settlement("42", "world:town_1", Size::Village, None));
        assert_ne!(first, settlement("43", "world:town_1", Size::Village, None));
        let kits = ["graveyard.", "town.", "castle.", "retro.", "pirate."];
        for model in used_models() {
            assert!(kits.iter().any(|kit| model.starts_with(kit)), "{model}");
        }
    }

    #[test]
    fn bigger_settlements_have_more_houses_and_only_towns_have_walls() {
        let count = |size| {
            settlement("7", "x", size, None)
                .iter()
                .filter(|p| p.model == "town.chimney")
                .count()
        };
        assert!(
            count(Size::Hamlet) < count(Size::Village) && count(Size::Village) < count(Size::Town)
        );
        let walls = |size| {
            settlement("7", "x", size, None)
                .iter()
                .filter(|p| p.model.starts_with("castle."))
                .count()
        };
        assert_eq!(walls(Size::Village), 0);
        assert!(walls(Size::Town) > 60);
    }

    #[test]
    fn a_coastal_settlement_has_a_harbour_with_ships_at_sea() {
        let by_sea = settlement("7", "x", Size::Village, Some(((1.0, 0.0), 120.0)));
        assert!(by_sea
            .iter()
            .any(|p| p.model == "pirate.ship-small" && p.at_sea));
        assert!(by_sea
            .iter()
            .any(|p| p.model == "pirate.structure-platform-dock"));
        assert!(!settlement("7", "x", Size::Village, None)
            .iter()
            .any(|p| p.model.starts_with("pirate.")));
    }

    #[test]
    fn the_graveyard_has_a_fence_a_crypt_and_gravestones() {
        let props = graveyard("1", "yard", 16.0);
        assert!(props.iter().filter(|p| p.model.contains("fence")).count() > 30);
        assert!(props.iter().any(|p| p.model == "graveyard.crypt-large"));
        assert!(
            props
                .iter()
                .filter(|p| p.model.contains("gravestone") || p.model == "graveyard.cross")
                .count()
                > 8
        );
    }

    #[test]
    fn local_axes_are_the_clients_and_stay_on_the_sphere() {
        let up = [0.3_f64, 0.8, 0.52];
        let length = up.iter().map(|v| v * v).sum::<f64>().sqrt();
        let up = up.map(|v| v / length);
        let (x_axis, z_axis) = client_axes(up);
        let dot = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).map(|(p, q)| p * q).sum::<f64>();
        assert!(
            dot(x_axis, up).abs() < 1e-9
                && dot(z_axis, up).abs() < 1e-9
                && dot(x_axis, z_axis).abs() < 1e-9
        );
        let (direction, position) = world_from_local(up, 100.0, -50.0, |_| 10.0, 6_371_000.0);
        let distance = position.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!((distance - 6_371_010.0).abs() < 1e-6);
        assert!((direction.iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-9);
        // Straight overhead of +Y the axes are the identity.
        assert_eq!(
            client_axes([0.0, 1.0, 0.0]),
            ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0])
        );
    }

    #[test]
    fn walls_block_but_doors_and_gates_stay_open() {
        let village = settlement("9", "x", Size::Village, None);
        let blocking = |model: &str| {
            village
                .iter()
                .filter(|p| p.model == model)
                .flat_map(colliders)
                .count()
        };
        assert!(blocking("town.wall") > 20 && blocking("town.wall-window-shutters") > 0);
        assert_eq!(blocking("town.wall-door"), 0, "a door is a way in");
        assert_eq!(blocking("town.roof"), 0, "roofs are overhead");
        // The panel's circle lies on its edge of the cell, not at the cell's centre.
        let panel = LocalProp::new("town.wall", 10.0, 20.0, 0.0, MODULE_M);
        let circle = colliders(&panel)[0];
        assert!(
            (circle.x - (10.0 + 0.45 * MODULE_M)).abs() < 1e-9 && (circle.z - 20.0).abs() < 1e-9
        );
        let turned = LocalProp::new("town.wall", 10.0, 20.0, -FRAC_PI_2, MODULE_M);
        let circle = colliders(&turned)[0];
        assert!(
            (circle.x - 10.0).abs() < 1e-9 && (circle.z - (20.0 + 0.45 * MODULE_M)).abs() < 1e-9,
            "yaw -90 puts it on the +Z edge"
        );
        let yard = graveyard("1", "y", 16.0);
        let gate = yard
            .iter()
            .filter(|p| p.model == "graveyard.iron-fence-border-gate")
            .flat_map(colliders)
            .count();
        assert_eq!(gate, 0, "the graveyard gate is open");
        assert!(yard.iter().flat_map(colliders).count() > 50);
    }

    #[test]
    fn list_models_for_bundling() {
        // Writes the model names the generators can use when SCENERY_MODELS names a file.
        let Some(path) = std::env::var_os("SCENERY_MODELS") else {
            return;
        };
        let models: Vec<String> = used_models().iter().map(|m| format!("\"{m}\"")).collect();
        std::fs::write(path, format!("[{}]", models.join(","))).unwrap();
    }

    #[test]
    fn dump_for_the_viewer() {
        // Writes the layouts as JSON for tools/scenery-view when SCENERY_DUMP names a file.
        let Some(path) = std::env::var_os("SCENERY_DUMP") else {
            return;
        };
        let kind = std::env::var("SCENERY_KIND").unwrap_or_else(|_| "village".into());
        let props = match kind.as_str() {
            "graveyard" => graveyard("1", "yard", 16.0),
            "hamlet" => settlement("3", "x", Size::Hamlet, None),
            "town" => settlement("3", "x", Size::Town, Some(((0.0, 1.0), 120.0))),
            _ => settlement("3", "x", Size::Village, Some(((0.0, 1.0), 110.0))),
        };
        let json: Vec<String> = props
            .iter()
            .map(|p| {
                format!(
                    "{{\"m\":\"{}\",\"p\":[{:.3},{:.3},{:.3}],\"yaw\":{:.4},\"s\":{:.3}}}",
                    p.model,
                    p.x,
                    p.y + if p.at_sea { -0.2 } else { 0.0 },
                    p.z,
                    p.yaw,
                    p.scale
                )
            })
            .collect();
        std::fs::write(path, format!("[{}]", json.join(","))).unwrap();
    }
}
