//! Height maps: reading, validating and hashing the imported faces of the planet.

use anyhow::{bail, Context, Result};
use image::{ColorType, ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use std::io::Cursor;

pub const MAX_MAP_BYTES: u64 = 16 * 1024 * 1024;

pub struct Heightmap {
    pub face_size: i32,
    pub pixels: Vec<u8>,
    pub sha256: String,
}

impl Heightmap {
    pub fn parse(pgm: &[u8]) -> Result<Self> {
        if pgm.len() as u64 > MAX_MAP_BYTES || !pgm.starts_with(b"P5") {
            bail!("expected a binary P5 PGM no larger than 16 MiB");
        }
        let mut reader = ImageReader::with_format(Cursor::new(pgm), ImageFormat::Pnm);
        let mut limits = Limits::default();
        limits.max_image_width = Some(6144);
        limits.max_image_height = Some(1024);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode().context("invalid PGM heightmap")?;
        let face_size = image.height();
        if face_size == 0 || image.width() != face_size * 6 || image.color() != ColorType::L8 {
            bail!("heightmap must contain six square faces in an 8-bit grayscale strip");
        }
        Ok(Self {
            face_size: face_size as i32,
            pixels: image.into_luma8().into_raw(),
            sha256: format!("{:x}", Sha256::digest(pgm)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_binary_pixels_including_whitespace() {
        let pgm = b"P5\n# cube faces\n6 1\n255\n\x0a\x20\x00\x80\xfe\xff";
        let map = Heightmap::parse(pgm).unwrap();
        assert_eq!(map.face_size, 1);
        assert_eq!(map.pixels, [10, 32, 0, 128, 254, 255]);
        assert_eq!(map.sha256.len(), 64);
    }

    #[test]
    fn rejects_invalid_maps() {
        for pgm in [
            b"P5\n6 1\n255\n\x00".as_slice(),
            b"P5\n1 1\n255\n\x00".as_slice(),
            b"P2\n6 1\n255\n0 0 0 0 0 0".as_slice(),
            b"P5\n6 1\n65535\n\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".as_slice(),
        ] {
            assert!(Heightmap::parse(pgm).is_err());
        }
    }
}
