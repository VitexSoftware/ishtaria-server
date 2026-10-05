//! Command-line helpers used by `ishtaria-admin` and `ishtaria-server-init`.

use super::{disk::Disk, world::disk_dir, Story};
use anyhow::{bail, Result};
use sqlx::PgPool;
use std::fs;

/// Prints `id<TAB>version<TAB>name` of every installed datadisk that loads cleanly.
pub fn list() -> Result<()> {
    let dir = disk_dir();
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.join("datadisk.yaml").is_file() {
            continue;
        }
        match Disk::load(&path, false) {
            Ok(disk) => found.push((disk.manifest.id, disk.manifest.version, disk.manifest.name)),
            Err(error) => eprintln!("skipping {}: {error:#}", path.display()),
        }
    }
    found.sort();
    for (id, version, name) in found {
        println!("{id}\t{version}\t{}", name.replace(['\t', '\n'], " "));
    }
    Ok(())
}

/// Checks that these installed disks can be combined into one world.
pub fn check(ids: &[String]) -> Result<Story> {
    if ids.is_empty() {
        bail!("no datadisk selected");
    }
    let dir = disk_dir();
    let mut paths = Vec::new();
    for id in ids {
        if !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            bail!("invalid datadisk id {id}");
        }
        let path = dir.join(id);
        if !path.join("datadisk.yaml").is_file() {
            bail!("datadisk {id} is not installed");
        }
        paths.push(path);
    }
    Story::load(&paths, false)
}

/// Records the disks a freshly generated world uses (only while it has none).
pub async fn apply(pool: &PgPool, world_id: i64, ids: &[String]) -> Result<()> {
    let story = check(ids)?;
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT id FROM worlds WHERE id = $1 FOR UPDATE")
        .bind(world_id)
        .execute(&mut *transaction)
        .await?;
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM world_datadisks WHERE world_id = $1")
            .bind(world_id)
            .fetch_one(&mut *transaction)
            .await?;
    if existing > 0 {
        bail!("the world already has datadisks; refusing to change them");
    }
    for (position, (id, version, sha256)) in story.disks.iter().enumerate() {
        sqlx::query("INSERT INTO world_datadisks (world_id, disk_id, version, sha256, position) VALUES ($1, $2, $3, $4, $5)")
            .bind(world_id)
            .bind(id)
            .bind(version)
            .bind(sha256)
            .bind(position as i32)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}
