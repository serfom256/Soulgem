use crate::repo::{BIG_OBJECT, FileEntry, Hash, MIN_PACK_SIZE, Object, Repo, TMP_PREFIX, Tree};
use crate::worktree::progress;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Stats {
    pub objects: usize,
    pub before: u64,
    pub after: u64,
    pub settled: u64,
    pub settled_bytes: u64,
}

impl Stats {
    pub fn saved_percent(&self) -> f64 {
        100.0 * (1.0 - self.after as f64 / self.before.max(1) as f64)
    }
}

fn compress(repo: &Repo, mut objects: Vec<(Hash, u64)>) -> Result<Stats> {
    objects.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    let (big, small): (Vec<_>, Vec<_>) = objects.iter().partition(|(_, size)| *size >= BIG_OBJECT);
    let bar = progress("compressing", objects.iter().map(|(_, size)| size).sum());
    let (before, after) = (AtomicU64::new(0), AtomicU64::new(0));
    let (settled, settled_bytes) = (AtomicU64::new(0), AtomicU64::new(0));
    let pack = |(hash, size): &(Hash, u64)| -> Result<()> {
        let p = repo.pack_object(hash, *size)?;
        if p.settled {
            settled.fetch_add(1, Ordering::Relaxed);
            settled_bytes.fetch_add(*size, Ordering::Relaxed);
        }
        before.fetch_add(p.before, Ordering::Relaxed);
        after.fetch_add(p.after, Ordering::Relaxed);
        bar.inc(*size);
        Ok(())
    };
    let result = big
        .iter()
        .try_for_each(pack)
        .and_then(|()| small.into_iter().par_bridge().try_for_each(|o| pack(&o)));
    bar.finish_and_clear();
    result?;
    Ok(Stats {
        objects: objects.len(),
        before: before.into_inner(),
        after: after.into_inner(),
        settled: settled.into_inner(),
        settled_bytes: settled_bytes.into_inner(),
    })
}

pub fn reshare(repo: &Repo, files: &[(String, FileEntry)]) -> Result<(usize, u64)> {
    if files.is_empty() || !repo.clones_supported() {
        return Ok((0, 0));
    }
    let freed = AtomicU64::new(0);
    let count = crate::repo::io_pool()?.install(|| {
        files
            .par_iter()
            .filter(
                |(rel, e)| match repo.share_object(&repo.root.join(rel), e) {
                    Ok(n) => {
                        freed.fetch_add(n, Ordering::Relaxed);
                        true
                    }
                    Err(_) => false,
                },
            )
            .count()
    });
    Ok((count, freed.into_inner()))
}

pub fn compress_history(repo: &Repo, keep: &Tree) -> Result<Stats> {
    for dir in [
        repo.dir.clone(),
        repo.dir.join("trees"),
        repo.dir.join("commits"),
    ] {
        remove_leftovers(&dir)?;
    }
    let keep: HashSet<&Hash> = keep.values().map(|e| &e.hash).collect();
    let mut objects = Vec::new();
    for shard in fs::read_dir(repo.dir.join("objects"))? {
        let shard = shard?;
        let prefix = shard.file_name().to_string_lossy().into_owned();
        remove_leftovers(&shard.path())?;
        for obj in fs::read_dir(shard.path())? {
            let obj = obj?;
            let Some((hex, false)) =
                Repo::parse_object_name(&prefix, &obj.file_name().to_string_lossy())
            else {
                continue;
            };
            let Ok(hash) = blake3::Hash::from_hex(&hex) else {
                continue;
            };
            let size = obj.metadata()?.len();
            if size >= MIN_PACK_SIZE && !keep.contains(hash.as_bytes()) {
                objects.push((*hash.as_bytes(), size));
            }
        }
    }
    compress(repo, objects)
}

fn remove_leftovers(dir: &std::path::Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub fn uncompressed_bytes(repo: &Repo, objects: impl IntoIterator<Item = (Hash, u64)>) -> u64 {
    let mut seen = HashSet::new();
    objects
        .into_iter()
        .filter(|(hash, _)| {
            seen.insert(*hash) && matches!(repo.find_object(hash), Some(Object::Raw(_)))
        })
        .map(|(_, size)| size)
        .sum()
}
