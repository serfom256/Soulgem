use crate::repo::{
    Changes, FileEntry, Hash, IGNORE_FILE, Repo, SOULGEM_DIR, TMP_PREFIX, Tree, diff, hash_file,
    io_pool, now_ns, rel_string, set_mtime, system_time_ns, write_atomic,
};
use anyhow::{Context, Result};
use ignore::{WalkBuilder, WalkState};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

const RACY_NS: i64 = 2_000_000_000;

const PROGRESS_MIN_BYTES: u64 = 64 << 20;

pub fn progress(label: &str, total: u64) -> ProgressBar {
    if total < PROGRESS_MIN_BYTES {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{msg:>9} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    bar.set_message(label.to_string());
    bar
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime_ns: i64,
    ino: u64,
    ctime_ns: i64,
}

impl Stamp {
    fn of(meta: &Metadata) -> Result<Stamp> {
        #[cfg(unix)]
        let (ino, ctime_ns) = {
            use std::os::unix::fs::MetadataExt;
            (meta.ino(), meta.ctime() * 1_000_000_000 + meta.ctime_nsec())
        };
        #[cfg(not(unix))]
        let (ino, ctime_ns) = (0, 0);
        Ok(Stamp {
            size: meta.len(),
            mtime_ns: system_time_ns(meta.modified()?),
            ino,
            ctime_ns,
        })
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    entries: HashMap<String, (Stamp, Hash)>,
}

impl Cache {
    fn path(repo: &Repo) -> PathBuf {
        repo.dir.join("index")
    }

    fn load(repo: &Repo) -> Cache {
        fs::read(Self::path(repo))
            .ok()
            .and_then(|bytes| postcard::from_bytes(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, repo: &Repo) -> Result<()> {
        write_atomic(&Self::path(repo), &postcard::to_allocvec(self)?)
    }

    fn record(&mut self, rel: String, stamp: Stamp, hash: Hash, now: i64) {
        if now - stamp.mtime_ns > RACY_NS {
            self.entries.insert(rel, (stamp, hash));
        } else {
            self.entries.remove(&rel);
        }
    }
}

struct Found {
    rel: String,
    stamp: Stamp,
}

fn walk(root: &Path) -> Result<Vec<Found>> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<Found>>();
    WalkBuilder::new(root)
        .standard_filters(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .ignore_case_insensitive(true)
        .follow_links(false)
        .filter_entry(|e| !(e.depth() == 1 && e.file_name() == SOULGEM_DIR))
        .build_parallel()
        .run(|| {
            let tx = tx.clone();
            Box::new(move |result| {
                let found = (|| -> Result<Option<Found>> {
                    let entry = result?;
                    if !entry.file_type().is_some_and(|t| t.is_file())
                        || entry.file_name().to_string_lossy().starts_with(TMP_PREFIX)
                    {
                        return Ok(None);
                    }
                    let rel = rel_string(entry.path().strip_prefix(root)?)?;
                    Ok(Some(Found {
                        rel,
                        stamp: Stamp::of(&entry.metadata()?)?,
                    }))
                })();
                match found {
                    Ok(None) => {}
                    other => {
                        let _ = tx.send(other.map(Option::unwrap));
                    }
                }
                WalkState::Continue
            })
        });
    drop(tx);
    rx.into_iter().collect()
}

pub fn scan(repo: &Repo) -> Result<Tree> {
    let found = walk(&repo.root)?;
    let cache = Cache::load(repo);
    let (hits, misses): (Vec<_>, Vec<_>) = found
        .into_iter()
        .map(|f| {
            let cached = cache
                .entries
                .get(&f.rel)
                .filter(|(s, _)| *s == f.stamp)
                .map(|(_, h)| *h);
            (f, cached)
        })
        .partition(|(_, cached)| cached.is_some());

    let bar = progress("hashing", misses.iter().map(|(f, _)| f.stamp.size).sum());
    let hashed = misses
        .into_par_iter()
        .map(|(f, _)| {
            let hash = hash_file(&repo.root.join(&f.rel))?;
            bar.inc(f.stamp.size);
            Ok((f, hash))
        })
        .collect::<Result<Vec<_>>>();
    bar.finish_and_clear();

    let now = now_ns();
    let mut tree = Tree::new();
    let mut new_cache = Cache::default();
    for (f, hash) in hits
        .into_iter()
        .map(|(f, h)| (f, h.unwrap()))
        .chain(hashed?)
    {
        tree.insert(
            f.rel.clone(),
            FileEntry {
                hash,
                size: f.stamp.size,
                mtime_ns: f.stamp.mtime_ns,
            },
        );
        new_cache.record(f.rel, f.stamp, hash, now);
    }
    new_cache.save(repo)?;
    Ok(tree)
}

pub fn apply(
    repo: &Repo,
    current: &Tree,
    target: &Tree,
) -> Result<(Changes, Vec<(String, FileEntry)>)> {
    let changes = diff(current, target);

    changes.deleted.par_iter().try_for_each(|rel| {
        let path = repo.root.join(rel);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
    })?;
    prune_empty_dirs(&repo.root, &changes.deleted);

    let writes: Vec<&String> = changes.added.iter().chain(&changes.modified).collect();
    let same_content_other_mtime: Vec<&String> = target
        .iter()
        .filter(|(rel, e)| {
            current
                .get(*rel)
                .is_some_and(|c| c.hash == e.hash && c.mtime_ns != e.mtime_ns)
        })
        .map(|(rel, _)| rel)
        .collect();

    let bar = progress(
        "restoring",
        writes.iter().map(|rel| target[*rel].size).sum(),
    );
    let pool = io_pool()?;
    let decompressed = std::sync::Mutex::new(Vec::new());
    let restored = pool.install(|| {
        writes
            .par_iter()
            .map(|rel| {
                let entry = &target[*rel];
                let path = repo.root.join(rel);
                if repo.restore_object(entry, &path)? {
                    decompressed.lock().unwrap().push((rel.to_string(), *entry));
                }
                bar.inc(entry.size);
                Ok((
                    rel.to_string(),
                    Stamp::of(&fs::metadata(&path)?)?,
                    entry.hash,
                ))
            })
            .collect::<Result<Vec<_>>>()
    });
    bar.finish_and_clear();
    let retimed = same_content_other_mtime
        .par_iter()
        .map(|rel| {
            let entry = &target[*rel];
            let path = repo.root.join(rel);
            set_mtime(&path, entry.mtime_ns)?;
            Ok((
                rel.to_string(),
                Stamp::of(&fs::metadata(&path)?)?,
                entry.hash,
            ))
        })
        .collect::<Result<Vec<_>>>();

    let mut cache = Cache::load(repo);
    for rel in &changes.deleted {
        cache.entries.remove(rel);
    }
    let now = now_ns();
    for (rel, stamp, hash) in restored?.into_iter().chain(retimed?) {
        cache.record(rel, stamp, hash, now);
    }
    cache.save(repo)?;
    Ok((changes, decompressed.into_inner().unwrap()))
}

fn prune_empty_dirs(root: &Path, deleted: &[String]) {
    let mut dirs: Vec<&Path> = deleted
        .iter()
        .flat_map(|rel| Path::new(rel).ancestors().skip(1))
        .filter(|d| !d.as_os_str().is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        let _ = fs::remove_dir(root.join(dir));
    }
}
