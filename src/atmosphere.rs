//! The sun above the planet: where it stands at a moment, announced to clients with the world
//! (`GET /world`) so that every client draws the same day and night.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize)]
pub struct Solar {
    pub version: u32,
    pub unix_seconds: f64,
    pub direction: [f64; 3],
    pub sidereal_day_seconds: f64,
    pub angular_radius_degrees: f64,
}

impl Solar {
    pub fn now() -> Self {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        Self::at(seconds)
    }

    pub fn at(unix_seconds: f64) -> Self {
        let days = unix_seconds / 86400.0 + 2440587.5 - 2451545.0;
        let anomaly = (357.528 + 0.9856003 * days).rem_euclid(360.0).to_radians();
        let longitude =
            (280.460 + 0.9856474 * days + 1.915 * anomaly.sin() + 0.020 * (2.0 * anomaly).sin())
                .rem_euclid(360.0)
                .to_radians();
        let obliquity = (23.439 - 0.0000004 * days).to_radians();
        let right_ascension = (obliquity.cos() * longitude.sin()).atan2(longitude.cos());
        let declination = (obliquity.sin() * longitude.sin()).asin();
        let sidereal = (280.46061837 + 360.98564736629 * days)
            .rem_euclid(360.0)
            .to_radians();
        let local_longitude = right_ascension - sidereal;
        let distance_au = 1.00014 - 0.01671 * anomaly.cos() - 0.00014 * (2.0 * anomaly).cos();
        Self {
            version: 1,
            unix_seconds,
            direction: [
                declination.cos() * local_longitude.cos(),
                declination.sin(),
                declination.cos() * local_longitude.sin(),
            ],
            sidereal_day_seconds: 86164.0905,
            angular_radius_degrees: 0.2666 / distance_au,
        }
    }
}

#[cfg(test)]
mod solar_tests {
    use super::Solar;

    #[test]
    fn noon_midnight_and_unit_direction() {
        let noon = Solar::at(946728000.0);
        let midnight = Solar::at(946728000.0 + 43200.0);
        assert!(noon.direction[0] > 0.9);
        assert!(midnight.direction[0] < -0.9);
        for solar in [noon, midnight] {
            assert!(
                (solar
                    .direction
                    .iter()
                    .map(|value| value * value)
                    .sum::<f64>()
                    - 1.0)
                    .abs()
                    < 1e-12
            );
            assert!((0.26..0.28).contains(&solar.angular_radius_degrees));
        }
    }

    #[test]
    fn seasons_and_polar_day_are_opposite() {
        let june = Solar::at((2460483.0 - 2440587.5) * 86400.0);
        let december = Solar::at((2460666.0 - 2440587.5) * 86400.0);
        assert!((june.direction[1] - 23.44_f64.to_radians().sin()).abs() < 0.001);
        assert!((december.direction[1] + 23.44_f64.to_radians().sin()).abs() < 0.001);
        for hours in 0..24 {
            let summer = Solar::at(june.unix_seconds + hours as f64 * 3600.0);
            let winter = Solar::at(december.unix_seconds + hours as f64 * 3600.0);
            assert!(summer.direction[1] > 0.0 && winter.direction[1] < 0.0);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Atmosphere {
    pub skybox: String,
    pub space_skybox: String,
    pub sun_color: [f32; 3],
    pub sun_energy: f32,
    pub ambient_color: [f32; 3],
    pub ambient_energy: f32,
    pub fog_color: [f32; 3],
    pub fog_density: f32,
    pub sky_energy: f32,
}

impl Default for Atmosphere {
    fn default() -> Self {
        Self {
            skybox: "day".into(),
            space_skybox: "galaxy".into(),
            sun_color: [1.0, 0.95, 0.85],
            sun_energy: 1.2,
            ambient_color: [0.7, 0.78, 0.84],
            ambient_energy: 0.45,
            fog_color: [0.72, 0.84, 0.85],
            fog_density: 0.000025,
            sky_energy: 1.0,
        }
    }
}

impl Atmosphere {
    pub fn validate(&self) -> Result<()> {
        if !["day", "morning", "night", "alien", "space"].contains(&self.skybox.as_str()) {
            bail!("unknown surface skybox");
        }
        if !["band", "dark", "day", "galaxy", "nebula"].contains(&self.space_skybox.as_str()) {
            bail!("unknown space skybox");
        }
        for color in [self.sun_color, self.ambient_color, self.fog_color] {
            if color
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            {
                bail!("atmosphere RGB values must be finite and in 0..1");
            }
        }
        for (value, maximum) in [
            (self.sun_energy, 16.0),
            (self.ambient_energy, 8.0),
            (self.sky_energy, 8.0),
            (self.fog_density, 0.02),
        ] {
            if !value.is_finite() || !(0.0..=maximum).contains(&value) {
                bail!("invalid atmosphere energy or fog density");
            }
        }
        Ok(())
    }
}
