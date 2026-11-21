// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax admin backup` / `restore` (H-015).
//!
//! A backup is a directory:
//!
//! ```text
//! manifest.json            format version, Fornax version, time, repository count
//! fornax.db                consistent SQLite copy (VACUUM INTO, safe while serving)
//! repos/<owner>/<name>.git bare repositories
//! ssh_host_ed25519_key     if present
//! ```
//!
//! Repositories are copied file by file. Git objects are immutable, so a copy taken during a
//! push can at worst miss that push's ref update; run `git fsck` on restored repositories if
//! a backup was taken under heavy write load.

use std::path::Path;

use anyhow::{Context, Result, bail};
use fornax_db::Db;
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;
const HOST_KEY: &str = "ssh_host_ed25519_key";

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub fornax_version: String,
    pub created_at: i64,
    pub repositories: usize,
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let mut files = 0;
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if kind.is_dir() {
            files += copy_dir(&entry.path(), &to)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &to)?;
            files += 1;
        }
        // Symlinks are skipped: Fornax never creates them inside repositories.
    }
    Ok(files)
}

pub async fn backup(db: &Db, data_dir: &Path, out: &Path) -> Result<Manifest> {
    if out.exists() {
        bail!("{} already exists; choose a new directory", out.display());
    }
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    db.backup_to(&out.join("fornax.db"))
        .await
        .context("copying database")?;

    let repos = db.all_repos().await?;
    let (src, dst) = (data_dir.join("repos"), out.join("repos"));
    tokio::task::spawn_blocking(move || copy_dir(&src, &dst))
        .await?
        .context("copying repositories")?;
    let key = data_dir.join(HOST_KEY);
    if key.exists() {
        std::fs::copy(&key, out.join(HOST_KEY)).context("copying SSH host key")?;
    }

    let manifest = Manifest {
        format: FORMAT,
        fornax_version: env!("CARGO_PKG_VERSION").into(),
        created_at: fornax_core::now_unix(),
        repositories: repos.len(),
    };
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    db.audit(
        None,
        "admin.backup",
        &out.display().to_string(),
        &serde_json::json!({ "repositories": repos.len() }).to_string(),
    )
    .await?;
    Ok(manifest)
}

pub async fn restore(from: &Path, data_dir: &Path) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(from.join("manifest.json")).context("reading manifest.json")?,
    )
    .context("parsing manifest.json")?;
    if manifest.format != FORMAT {
        bail!("unsupported backup format {}", manifest.format);
    }
    if data_dir.join("fornax.db").exists() {
        bail!(
            "{} already contains fornax.db; restore needs an empty data directory",
            data_dir.display()
        );
    }
    std::fs::create_dir_all(data_dir)?;
    std::fs::copy(from.join("fornax.db"), data_dir.join("fornax.db"))
        .context("restoring database")?;
    let (src, dst) = (from.join("repos"), data_dir.join("repos"));
    if src.exists() {
        tokio::task::spawn_blocking(move || copy_dir(&src, &dst))
            .await?
            .context("restoring repositories")?;
    }
    if from.join(HOST_KEY).exists() {
        std::fs::copy(from.join(HOST_KEY), data_dir.join(HOST_KEY))?;
    }

    // Verify: the database opens (and migrates) and every repository is on disk.
    let db = Db::open(&data_dir.join("fornax.db")).await?;
    let store = fornax_git::RepoStore::new(data_dir.join("repos"));
    let repos = db.all_repos().await?;
    let missing: Vec<String> = repos
        .iter()
        .filter(|r| {
            let (o, n) = r.storage_key();
            !store.repo_path(&o, &n).exists()
        })
        .map(fornax_db::Repo::full_name)
        .collect();
    if !missing.is_empty() {
        bail!("restored database references missing repositories: {missing:?}");
    }
    db.audit(None, "admin.restore", &from.display().to_string(), "{}")
        .await?;
    db.close().await;
    Ok(manifest)
}
