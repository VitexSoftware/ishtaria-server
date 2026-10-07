//! Local relief and spawning: the gentle hills laid over the imported map, and where a new
//! character may appear (dry, temperate land away from water).

use rand::{rngs::OsRng, seq::IteratorRandom};
use serde::Serialize;
use std::{cmp::Reverse, collections::BinaryHeap};

pub const MAX_FACE_SIZE: usize = 64;
pub const DRY: i32 = -8001;

pub fn relief_offset(direction: [f64; 3], base_height: f64, seed: &str) -> f64 {
    let radius = direction.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
    let point = direction.map(|axis| axis / radius * 6_371_000.0);
    let phase = seed
        .bytes()
        .fold(0u32, |value, byte| (value * 31 + u32::from(byte)) % 997);
    let phase = f64::from(phase) * std::f64::consts::TAU / 997.0;
    let coast = (base_height / 150.0).clamp(0.0, 1.0);
    let fade = coast * coast * (3.0 - 2.0 * coast);
    fade * (9.0
        * ((point[0] * 0.73 + point[1] * 0.41 + point[2] * 0.55) / 93.0 + phase).sin()
        * ((point[0] * -0.38 + point[1] * 0.87 + point[2] * 0.31) / 127.0 - phase).sin()
        + 3.0 * ((point[0] * 0.21 + point[1] * -0.52 + point[2] * 0.83) / 37.0 + phase).sin())
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Environment {
    pub version: u32,
    pub heightmap_sha256: String,
    pub seed: String,
    pub face_size: usize,
    pub elevation_m: Vec<i32>,
    pub water_m: Vec<i32>,
    pub downstream: Vec<i32>,
    pub flow: Vec<u32>,
    pub biomes: Vec<u8>,
}

fn cube(face: usize, horizontal: f64, vertical: f64) -> [f64; 3] {
    match face {
        0 => [1.0, vertical, -horizontal],
        1 => [-1.0, vertical, horizontal],
        2 => [horizontal, 1.0, -vertical],
        3 => [horizontal, -1.0, vertical],
        4 => [horizontal, vertical, 1.0],
        _ => [-horizontal, vertical, -1.0],
    }
}

fn neighbor(index: usize, horizontal: isize, vertical: isize, size: usize) -> usize {
    let face = index / (size * size);
    let column = (index % size) as isize + horizontal;
    let row = ((index / size) % size) as isize + vertical;
    let point = cube(
        face,
        (column as f64 + 0.5) * 2.0 / size as f64 - 1.0,
        1.0 - (row as f64 + 0.5) * 2.0 / size as f64,
    );
    let [x_axis, y_axis, z_axis] = point.map(f64::abs);
    let (target, horizontal, vertical) = if x_axis >= y_axis && x_axis >= z_axis {
        if point[0] > 0.0 {
            (0, -point[2] / x_axis, point[1] / x_axis)
        } else {
            (1, point[2] / x_axis, point[1] / x_axis)
        }
    } else if y_axis >= z_axis {
        if point[1] > 0.0 {
            (2, point[0] / y_axis, -point[2] / y_axis)
        } else {
            (3, point[0] / y_axis, point[2] / y_axis)
        }
    } else if point[2] > 0.0 {
        (4, point[0] / z_axis, point[1] / z_axis)
    } else {
        (5, -point[0] / z_axis, point[1] / z_axis)
    };
    let column = (((horizontal + 1.0) * 0.5 * size as f64) as usize).min(size - 1);
    let row = (((1.0 - vertical) * 0.5 * size as f64) as usize).min(size - 1);
    target * size * size + row * size + column
}

fn moisture(seed: u64, point: [f64; 3]) -> f64 {
    let mut mixed = seed;
    for coordinate in point {
        mixed ^= ((coordinate * 5.0).floor() as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    }
    (mixed >> 11) as f64 / (1u64 << 53) as f64
}

pub fn generate(size: usize, pixels: &[u8], seed: &str, sha256: &str) -> Option<Environment> {
    if size == 0 || size > 1024 || pixels.len() != size * size * 6 {
        return None;
    }
    let resolution = size.min(MAX_FACE_SIZE);
    let count = resolution * resolution * 6;
    let mut elevation_m = Vec::with_capacity(count);
    for face in 0..6 {
        for row in 0..resolution {
            for column in 0..resolution {
                let source_row = ((row * 2 + 1) * size / (resolution * 2)).min(size - 1);
                let source_column = ((column * 2 + 1) * size / (resolution * 2)).min(size - 1);
                let value = pixels[source_row * size * 6 + face * size + source_column];
                elevation_m.push(i32::from(value) * 16000 / 255 - 8000);
            }
        }
    }
    let mut filled = elevation_m.clone();
    let mut visited = vec![false; count];
    let mut downstream = vec![-1; count];
    let mut queue = BinaryHeap::new();
    for index in 0..count {
        if elevation_m[index] <= 0 {
            visited[index] = true;
            queue.push(Reverse((0, index)));
            filled[index] = 0;
        }
    }
    if queue.is_empty() {
        let index = (0..count).min_by_key(|&index| elevation_m[index])?;
        visited[index] = true;
        queue.push(Reverse((elevation_m[index], index)));
    }
    let mut order = Vec::with_capacity(count);
    while let Some(Reverse((level, index))) = queue.pop() {
        order.push(index);
        for (horizontal, vertical) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let adjacent = neighbor(index, horizontal, vertical, resolution);
            if visited[adjacent] {
                continue;
            }
            visited[adjacent] = true;
            filled[adjacent] = elevation_m[adjacent].max(level);
            downstream[adjacent] = index as i32;
            queue.push(Reverse((filled[adjacent], adjacent)));
        }
    }
    let mut flow = vec![1u32; count];
    for &index in order.iter().rev() {
        if downstream[index] >= 0 {
            flow[downstream[index] as usize] += flow[index];
        }
    }
    let mut water_m = vec![DRY; count];
    let mut biomes = vec![0; count];
    let numeric_seed = seed.parse::<u64>().ok()?;
    for index in 0..count {
        let face = index / (resolution * resolution);
        let horizontal =
            (index % resolution) as f64 * 2.0 / resolution as f64 + 1.0 / resolution as f64 - 1.0;
        let vertical = 1.0
            - ((index / resolution) % resolution) as f64 * 2.0 / resolution as f64
            - 1.0 / resolution as f64;
        let point = cube(face, horizontal, vertical);
        let latitude = point[1]
            * (1.0 - point[0].powi(2) / 2.0 - point[2].powi(2) / 2.0
                + point[0].powi(2) * point[2].powi(2) / 3.0)
                .sqrt();
        let elevation = elevation_m[index];
        biomes[index] = if elevation <= 0 {
            water_m[index] = 0;
            0
        } else if filled[index] > elevation + 32 {
            water_m[index] = filled[index];
            1
        } else if flow[index] >= 18 && downstream[index] >= 0 {
            water_m[index] = filled[index];
            2
        } else if latitude.abs() + f64::from(elevation) / 6000.0 > 0.72 {
            7
        } else if elevation > 1600 {
            6
        } else if elevation < 150 {
            3
        } else if moisture(numeric_seed, point) > 0.35 {
            5
        } else {
            4
        };
    }
    Some(Environment {
        version: 2,
        heightmap_sha256: sha256.to_owned(),
        seed: seed.to_owned(),
        face_size: resolution,
        elevation_m,
        water_m,
        downstream,
        flow,
        biomes,
    })
}

pub fn spawn_position(size: usize, pixels: &[u8], seed: &str, sha256: &str) -> Option<[f64; 3]> {
    let environment = generate(size, pixels, seed, sha256)?;
    let resolution = environment.face_size;
    let radius = 6_371_000.0;
    (0..environment.biomes.len())
        .filter_map(|index| {
            let column = index % resolution;
            let row = (index / resolution) % resolution;
            if column == 0 || row == 0 || column + 1 == resolution || row + 1 == resolution {
                return None;
            }
            for vertical in -1..=1 {
                for horizontal in -1..=1 {
                    let adjacent = neighbor(index, horizontal, vertical, resolution);
                    if !matches!(environment.biomes[adjacent], 4 | 5)
                        || environment.water_m[adjacent] != DRY
                    {
                        return None;
                    }
                }
            }
            let face = index / (resolution * resolution);
            let source_column = (column * 2 + 1) * size / (resolution * 2);
            let source_row = (row * 2 + 1) * size / (resolution * 2);
            let mut minimum = 8000.0_f64;
            let mut maximum = -8000.0_f64;
            for sample_row in source_row.saturating_sub(1)..=(source_row + 1).min(size - 1) {
                for sample_column in
                    source_column.saturating_sub(1)..=(source_column + 1).min(size - 1)
                {
                    let height =
                        f64::from(pixels[sample_row * size * 6 + face * size + sample_column])
                            * 16000.0
                            / 255.0
                            - 8000.0;
                    minimum = minimum.min(height);
                    maximum = maximum.max(height);
                }
            }
            if minimum < 150.0
                || maximum > 1600.0
                || (maximum - minimum) / (radius / size as f64) > 0.2
            {
                return None;
            }
            let horizontal = (column as f64 + 0.5) * size as f64 / resolution as f64 - 0.5;
            let vertical = (row as f64 + 0.5) * size as f64 / resolution as f64 - 0.5;
            let height_at = |sample_column: usize, sample_row: usize| {
                f64::from(pixels[sample_row * size * 6 + face * size + sample_column]) * 16000.0
                    / 255.0
                    - 8000.0
            };
            let left = horizontal.floor() as usize;
            let top = vertical.floor() as usize;
            let fraction_x = horizontal.fract();
            let fraction_y = vertical.fract();
            let upper =
                height_at(left, top) * (1.0 - fraction_x) + height_at(left + 1, top) * fraction_x;
            let lower = height_at(left, top + 1) * (1.0 - fraction_x)
                + height_at(left + 1, top + 1) * fraction_x;
            let elevation = upper * (1.0 - fraction_y) + lower * fraction_y;
            let point = cube(
                face,
                (column as f64 + 0.5) * 2.0 / resolution as f64 - 1.0,
                1.0 - (row as f64 + 0.5) * 2.0 / resolution as f64,
            );
            let direction = [
                point[0]
                    * (1.0 - point[1].powi(2) / 2.0 - point[2].powi(2) / 2.0
                        + point[1].powi(2) * point[2].powi(2) / 3.0)
                        .sqrt(),
                point[1]
                    * (1.0 - point[0].powi(2) / 2.0 - point[2].powi(2) / 2.0
                        + point[0].powi(2) * point[2].powi(2) / 3.0)
                        .sqrt(),
                point[2]
                    * (1.0 - point[0].powi(2) / 2.0 - point[1].powi(2) / 2.0
                        + point[0].powi(2) * point[1].powi(2) / 3.0)
                        .sqrt(),
            ];
            let elevation = elevation + relief_offset(direction, elevation, seed);
            Some(direction.map(|coordinate| coordinate * (radius + elevation)))
        })
        .choose(&mut OsRng)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relief_is_bounded_continuous_seeded_and_coast_safe() {
        for (direction, seed, expected) in [
            ([1.0, 0.0, 0.0], "42", -7.595497428807521),
            ([1.0, 1.0, 0.0], "42", 4.106134823054352),
            ([1.0, 2.0, 3.0], "18446744073709551615", -1.7645037024417682),
            ([6371000.0, 0.01, 0.0], "42", -7.594750622916318),
        ] {
            assert!((relief_offset(direction, 1000.0, seed) - expected).abs() < 0.000001);
        }
        let mut minimum = f64::MAX;
        let mut maximum = f64::MIN;
        for metre in 0..1000 {
            let point = [6_371_000.0, f64::from(metre), 0.0];
            let height = relief_offset(point, 1000.0, "42");
            minimum = minimum.min(height);
            maximum = maximum.max(height);
            assert!(height.abs() <= 12.0);
            let adjacent = [point[0], point[1] + 0.01, point[2]];
            assert!((height - relief_offset(adjacent, 1000.0, "42")).abs() < 0.003);
            assert_eq!(relief_offset(point, 0.0, "42"), 0.0);
            assert_eq!(relief_offset(point, -1000.0, "42"), 0.0);
        }
        assert!(maximum - minimum > 10.0);
        assert_ne!(
            relief_offset([1.0, 0.0, 0.0], 1000.0, "42"),
            relief_offset([1.0, 0.0, 0.0], 1000.0, "43")
        );
    }

    #[test]
    fn spawns_only_on_dry_temperate_lowlands() {
        let size = 16;
        let pixels = vec![144; size * size * 6];
        for _ in 0..32 {
            let position = spawn_position(size, &pixels, "42", "test").unwrap();
            let radius = position.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
            let base = 144.0 * 16000.0 / 255.0 - 8000.0;
            assert!(
                (radius - (6_371_000.0 + base + relief_offset(position, base, "42"))).abs()
                    < 0.000001
            );
            assert!(position[1].abs() / radius + (radius - 6_371_000.0) / 6000.0 <= 0.72);
        }
        for value in [0, 127, 128, 160, 255] {
            assert!(spawn_position(size, &vec![value; size * size * 6], "42", "test").is_none());
        }
    }

    #[test]
    fn spawns_away_from_ocean_and_lakes() {
        let size = 16;
        let mut pixels = vec![144; size * size * 6];
        for row in 0..size {
            pixels[row * size * 6..row * size * 6 + size].fill(80);
        }
        pixels[8 * size * 6 + 4 * size + 8] = 135;
        let environment = generate(size, &pixels, "42", "test").unwrap();
        assert!(environment.biomes.contains(&0));
        assert!(environment.biomes.contains(&1));
        for _ in 0..32 {
            let position = spawn_position(size, &pixels, "42", "test").unwrap();
            assert!(position[0] < position[1].abs().max(position[2].abs()));
            let radius = position.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
            let base = 144.0 * 16000.0 / 255.0 - 8000.0;
            assert!(
                (radius - (6_371_000.0 + base + relief_offset(position, base, "42"))).abs()
                    < 0.000001
            );
            if position[2] > position[0].abs().max(position[1].abs()) {
                let horizontal = position[0] / radius;
                let vertical = position[1] / radius;
                assert!(
                    !(-0.045..=0.134).contains(&horizontal)
                        || !(-0.134..=0.045).contains(&vertical)
                );
            }
        }
    }

    #[test]
    fn cube_neighbors_connect_faces_bidirectionally() {
        let size = 8;
        for index in 0..size * size * 6 {
            for (horizontal, vertical) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let adjacent = neighbor(index, horizontal, vertical, size);
                assert_ne!(index, adjacent);
                assert!([(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(
                    |&(horizontal, vertical)| neighbor(adjacent, horizontal, vertical, size)
                        == index
                ));
            }
        }
    }

    #[test]
    fn fills_lakes_routes_rivers_and_never_cycles() {
        let size = 8;
        let mut pixels = vec![160; size * size * 6];
        pixels[0] = 80;
        pixels[4 * size * 6 + 4] = 140;
        let map = generate(size, &pixels, "42", "test").unwrap();
        assert_eq!(map, generate(size, &pixels, "42", "test").unwrap());
        assert!(map.biomes.contains(&0));
        assert!(map.biomes.contains(&1));
        assert!(map.biomes.contains(&2));
        for start in 0..map.downstream.len() {
            let mut index = start;
            let mut steps = 0;
            while map.downstream[index] >= 0 {
                let target = map.downstream[index] as usize;
                assert!(
                    map.elevation_m[target].max(map.water_m[target])
                        <= map.elevation_m[index].max(map.water_m[index])
                );
                index = target;
                steps += 1;
                assert!(steps < map.downstream.len());
            }
        }
    }

    #[test]
    fn bounds_work_and_rejects_invalid_maps() {
        assert!(generate(0, &[], "42", "test").is_none());
        assert!(generate(1025, &[], "42", "test").is_none());
        assert!(generate(1, &[0; 6], "bad", "test").is_none());
        let map = generate(128, &vec![180; 128 * 128 * 6], "42", "test").unwrap();
        assert_eq!(map.face_size, MAX_FACE_SIZE);
        assert_eq!(map.biomes.len(), MAX_FACE_SIZE * MAX_FACE_SIZE * 6);
        assert!(!map.biomes.contains(&0));
    }
}
