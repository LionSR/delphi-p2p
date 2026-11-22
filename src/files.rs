//! Directory snapshots backed by iroh-blobs collections.
//!
//! Each snapshot is an iroh-blobs collection mapping relative file paths to BLAKE3 content hashes,
//! identified by a single root hash. Identical files are deduplicated across snapshots both on disk
//! and over the network.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use iroh_blobs::api::Store;
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::format::collection::Collection;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, ChunkRangesSeq, GetRequest};
use iroh_blobs::{BlobFormat, Hash, HashAndFormat};
use serde::Serialize;

/// Directories excluded from snapshots: local client metadata, Git history, and Lean build artifacts.
const SKIPPED: [&str; 3] = [".delphi", ".git", ".lake"];

pub struct Snapshot {
    pub hash: Hash,
    pub files: u64,
    pub bytes: u64,
}

/// Imports a directory into the blob store and returns its snapshot metadata.
pub async fn import(store: &Store, dir: &Path) -> Result<Snapshot> {
    let mut paths = Vec::new();
    walk(dir, dir, &mut paths)?;
    if paths.is_empty() {
        bail!("{} has no files to publish", dir.display());
    }
    paths.sort();

    let batch = store.blobs().batch().await?;
    let mut tags = Vec::new();
    let mut collection = Collection::default();
    let mut bytes = 0;
    for name in &paths {
        let path = dir.join(name);
        bytes += std::fs::metadata(&path)?.len();
        let tag = batch
            .add_path_with_opts(AddPathOptions {
                path,
                format: BlobFormat::Raw,
                mode: ImportMode::Copy,
            })
            .temp_tag()
            .await
            .with_context(|| format!("failed to store {name}"))?;
        collection.push(name.clone(), tag.hash());
        tags.push(tag);
    }
    let root = collection.store(store).await?;
    let hash = root.hash();
    // Pin the snapshot with a named tag to prevent garbage collection.
    store
        .tags()
        .set(format!("snapshot/{hash}"), HashAndFormat::hash_seq(hash))
        .await?;
    Ok(Snapshot {
        hash,
        files: paths.len() as u64,
        bytes,
    })
}

/// Exports all files of a locally complete snapshot into `dir`.
pub async fn export(store: &Store, hash: Hash, dir: &Path) -> Result<u64> {
    let collection = Collection::load(hash, store).await?;
    std::fs::create_dir_all(dir)?;
    let mut files = 0;
    for (name, file) in collection.iter() {
        let target = dir.join(checked(name)?);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        store.blobs().export(*file, std::path::absolute(&target)?).await?;
        files += 1;
    }
    Ok(files)
}

/// Single file entry within a snapshot.
#[derive(Clone, Serialize)]
pub struct Entry {
    pub path: String,
    pub bytes: u64,
    pub hash: String,
}

/// Builds a partial iroh-blobs request for listing a snapshot without downloading full files:
/// fetches the collection metadata in full plus only the last chunk of each blob to verify sizes
/// (files up to 1,024 bytes fit in a single chunk).
pub fn listing(hash: Hash) -> GetRequest {
    let ranges = [ChunkRanges::all(), ChunkRanges::all(), ChunkRanges::last_chunk()];
    GetRequest::new(hash, ChunkRangesSeq::from_ranges_infinite(ranges))
}

/// Returns the snapshot's file list, or `None` if the collection or any file size is not yet stored.
pub async fn list(store: &Store, hash: Hash) -> Result<Option<Vec<Entry>>> {
    let Ok(collection) = Collection::load(hash, store).await else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    for (name, file) in collection.iter() {
        let Some(bytes) = store.blobs().observe(*file).await?.validated_size() else {
            return Ok(None);
        };
        entries.push(Entry {
            path: name.clone(),
            bytes,
            hash: file.to_string(),
        });
    }
    Ok(Some(entries))
}

/// Returns true if every file in the snapshot is present in the local store.
pub async fn complete(store: &Store, hash: Hash) -> Result<bool> {
    Ok(store.remote().local(HashAndFormat::hash_seq(hash)).await?.is_complete())
}

/// Validates that a relative path from an untrusted snapshot stays within the target directory.
pub fn checked(name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    let inside = !name.is_empty() && path.components().all(|part| matches!(part, Component::Normal(_)));
    if !inside {
        bail!("snapshot contains a path outside the target directory: {name:?}");
    }
    Ok(path.to_path_buf())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("{}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !SKIPPED.iter().any(|skipped| name == *skipped) {
                walk(root, &entry.path(), out)?;
            }
        } else if kind.is_file() && name != ".DS_Store" {
            let relative = entry.path().strip_prefix(root)?.to_path_buf();
            let name = relative
                .to_str()
                .context("file path is not valid UTF-8")?
                .replace('\\', "/");
            out.push(name);
        }
        // Skip symlinks so snapshots contain only regular files inside the directory.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::checked;

    #[test]
    fn snapshot_paths_stay_within_directory() {
        assert!(checked("Proof.lean").is_ok());
        assert!(checked("src/Proof.lean").is_ok());
        assert!(checked("../escape").is_err());
        assert!(checked("src/../../escape").is_err());
        assert!(checked("/etc/passwd").is_err());
        assert!(checked("").is_err());
    }
}
