use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SOULGEM_DIR: &str = ".soulgem";
pub const IGNORE_FILE: &str = ".soulgemignore";
pub const TMP_PREFIX: &str = ".soulgem-tmp-";
pub const SHORT: usize = 10;
const FORMAT: &str = "soulgem 1";

pub type Hash = [u8; 32];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub hash: Hash,
    pub size: u64,
    pub mtime_ns: i64,
}

pub type Tree = BTreeMap<String, FileEntry>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Commit {
    #[serde(skip)]
    pub id: String,
    pub tree: String,
    pub parent: Option<String>,
    pub message: String,
    pub time_ms: i64,
    pub files: u64,
    pub size: u64,
    pub added: u64,
    pub modified: u64,
    pub deleted: u64,
}

#[derive(Default)]
pub struct Changes {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl Changes {
    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn diff(old: &Tree, new: &Tree) -> Changes {
    let mut ch = Changes::default();
    for (path, entry) in new {
        match old.get(path) {
            None => ch.added.push(path.clone()),
            Some(o) if o.hash != entry.hash => ch.modified.push(path.clone()),
            _ => {}
        }
    }
    ch.deleted = old
        .keys()
        .filter(|p| !new.contains_key(*p))
        .cloned()
        .collect();
    ch
}

pub fn hex(hash: &Hash) -> String {
    blake3::Hash::from(*hash).to_hex().to_string()
}

pub fn short(id: &str) -> &str {
    &id[..id.len().min(SHORT)]
}

const MMAP_MIN: u64 = 1 << 20;

fn already_in_parallel_loop() -> bool {
    rayon::current_thread_index().is_some()
}

pub fn hash_file(path: &Path) -> Result<Hash> {
    let read = || -> std::io::Result<Hash> {
        let mut file = File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        if file.metadata()?.len() < MMAP_MIN {
            hasher.update_reader(&mut file)?;
        } else if already_in_parallel_loop() {
            hasher.update_mmap(path)?;
        } else {
            hasher.update_mmap_rayon(path)?;
        }
        Ok(*hasher.finalize().as_bytes())
    };
    read().with_context(|| format!("reading {}", path.display()))
}

pub const AUTO_LEVEL: i32 = 9;
pub const MIN_PACK_SIZE: u64 = 4096;
const FAST_LEVEL: i32 = 3;
pub const BIG_OBJECT: u64 = 128 << 20;
const BIG_WINDOW_LOG: u32 = 26;
const BIG_JOB_SIZE: u32 = 32 << 20;
const BIG_OVERLAP_LOG: u32 = 6;
const MAX_WINDOW_LOG: u32 = 27;
const PACKED_SUFFIX: &str = ".z";

fn copy_hashing(input: &mut impl Read, output: &mut impl Write) -> std::io::Result<Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    Ok(*hasher.finalize().as_bytes())
}

pub const SETTLED: u8 = 0;
const SLOW_LEVEL: i32 = 5;
const SAMPLES: u64 = 8;
const SAMPLE_LEN: usize = 128 << 10;
const MIN_SAVING: f64 = 0.10;

fn pack_hashing(
    input: &mut impl Read,
    size: u64,
    dst: &Path,
    level: i32,
    header: u8,
) -> Result<Hash> {
    let mut out = std::io::BufWriter::new(File::create(dst)?);
    out.write_all(&[header])?;
    let mut encoder = zstd::stream::write::Encoder::new(out, level)?;
    encoder.set_pledged_src_size(Some(size))?;
    if size >= BIG_OBJECT && level >= SLOW_LEVEL {
        use zstd::stream::raw::CParameter;
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        encoder.multithread(threads as u32)?;
        encoder.long_distance_matching(true)?;
        encoder.window_log(BIG_WINDOW_LOG)?;
        encoder.set_parameter(CParameter::JobSize(BIG_JOB_SIZE))?;
        encoder.set_parameter(CParameter::OverlapSizeLog(BIG_OVERLAP_LOG))?;
    }
    let hash = copy_hashing(input, &mut encoder)?;
    encoder.finish()?.flush()?;
    Ok(hash)
}

trait Skip: Read {
    fn skip(&mut self, n: u64) -> std::io::Result<()> {
        std::io::copy(&mut self.take(n), &mut std::io::sink()).map(|_| ())
    }
}

impl Skip for File {
    fn skip(&mut self, n: u64) -> std::io::Result<()> {
        self.seek(SeekFrom::Current(n as i64)).map(|_| ())
    }
}

fn worth_compressing(input: &mut impl Skip, size: u64) -> Result<bool> {
    let mut pos = 0u64;
    let mut savings = Vec::new();
    let mut buf = vec![0; SAMPLE_LEN];
    for i in 0..SAMPLES {
        let at = size * i / SAMPLES;
        if at > pos {
            input.skip(at - pos)?;
            pos = at;
        }
        let mut n = 0;
        while n < buf.len() {
            match input.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            break;
        }
        pos += n as u64;
        let packed = zstd::bulk::compress(&buf[..n], 1)?.len();
        savings.push(1.0 - packed as f64 / n as f64);
    }
    if savings.is_empty() {
        return Ok(false);
    }
    savings.sort_by(f64::total_cmp);
    Ok(savings[savings.len() / 2] >= MIN_SAVING)
}

pub struct Packed {
    pub before: u64,
    pub after: u64,
    pub settled: bool,
}

fn clones_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("SOULGEM_NO_CLONE").is_some())
}

const CLONES_ARE_ATOMIC: bool = cfg!(target_os = "macos");

fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    if clones_disabled() {
        bail!("clones disabled");
    }
    reflink_copy::reflink(src, dst)?;
    Ok(())
}

pub enum Object {
    Raw(PathBuf),
    Packed(PathBuf),
}

pub fn io_pool() -> Result<rayon::ThreadPool> {
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(6);
    Ok(rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?)
}

pub fn system_time_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

pub fn now_ns() -> i64 {
    system_time_ns(SystemTime::now())
}

pub fn set_mtime(path: &Path, mtime_ns: i64) -> Result<()> {
    let time = filetime::FileTime::from_unix_time(
        mtime_ns.div_euclid(1_000_000_000),
        mtime_ns.rem_euclid(1_000_000_000) as u32,
    );
    filetime::set_file_mtime(path, time)
        .with_context(|| format!("setting mtime of {}", path.display()))
}

pub fn tmp_path(dir: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        "{TMP_PREFIX}{}-{}-{n}",
        std::process::id(),
        now_ns()
    ))
}

fn with_tmp(dir: &Path, f: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = tmp_path(dir);
    let result = f(&tmp);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    with_tmp(path.parent().unwrap(), |tmp| {
        let mut file = File::create(tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(tmp, path).with_context(|| format!("writing {}", path.display()))
    })
}

pub struct Repo {
    pub root: PathBuf,
    pub dir: PathBuf,
    _lock: File,
}

impl Repo {
    pub fn init(path: &Path) -> Result<PathBuf> {
        fs::create_dir_all(path)?;
        let dir = std::path::absolute(path)?.join(SOULGEM_DIR);
        if dir.exists() {
            bail!("already a soulgem repository: {}", path.display());
        }
        for sub in ["objects", "trees", "commits"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        fs::write(dir.join("HEAD"), "")?;
        fs::write(dir.join("format"), FORMAT)?;
        Ok(dir)
    }

    pub fn open() -> Result<Repo> {
        let cwd = std::env::current_dir()?;
        let root = cwd
            .ancestors()
            .find(|p| p.join(SOULGEM_DIR).is_dir())
            .ok_or_else(|| {
                anyhow!(
                    "not a soulgem repository (or any parent directory); run 'soulgem init' first"
                )
            })?
            .to_path_buf();
        let dir = root.join(SOULGEM_DIR);
        match fs::read_to_string(dir.join("format")) {
            Ok(f) if f.trim() == FORMAT => {}
            _ => bail!(
                "{} was created by an older, incompatible soulgem; remove it and run 'soulgem init' again",
                dir.display()
            ),
        }
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                bail!("another soulgem command is running in this repository")
            }
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        Ok(Repo {
            root,
            dir,
            _lock: lock,
        })
    }

    pub fn head(&self) -> Result<Option<String>> {
        let head = fs::read_to_string(self.dir.join("HEAD"))?;
        let head = head.trim();
        Ok((!head.is_empty()).then(|| head.to_string()))
    }

    pub fn set_head(&self, id: &str) -> Result<()> {
        write_atomic(&self.dir.join("HEAD"), id.as_bytes())
    }

    pub fn resolve(&self, rev: &str) -> Result<String> {
        let (name, steps) = match rev.split_once('~') {
            None => (rev, 0),
            Some((name, "")) => (name, 1),
            Some((name, n)) => (name, n.parse().map_err(|_| anyhow!("bad revision: {rev}"))?),
        };
        let mut id = if name.is_empty() || name.eq_ignore_ascii_case("HEAD") || name == "@" {
            self.head()?.ok_or_else(|| anyhow!("no commits yet"))?
        } else {
            self.find_commit(name)?
        };
        for _ in 0..steps {
            id = self
                .read_commit(&id)?
                .parent
                .ok_or_else(|| anyhow!("{rev}: history is not that long"))?;
        }
        Ok(id)
    }

    fn find_commit(&self, prefix: &str) -> Result<String> {
        if prefix.len() < 4 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("unknown revision: {prefix}");
        }
        let prefix = prefix.to_ascii_lowercase();
        let mut matches = self
            .commit_ids()?
            .into_iter()
            .filter(|id| id.starts_with(&prefix));
        match (matches.next(), matches.count()) {
            (None, _) => bail!("unknown revision: {prefix}"),
            (Some(id), 0) => Ok(id),
            (Some(_), n) => bail!("ambiguous commit prefix '{prefix}' ({} matches)", n + 1),
        }
    }

    pub fn object_path(&self, hash: &Hash) -> PathBuf {
        let h = hex(hash);
        self.dir.join("objects").join(&h[..2]).join(&h[2..])
    }

    pub fn packed_path(&self, hash: &Hash) -> PathBuf {
        let mut path = self.object_path(hash).into_os_string();
        path.push(PACKED_SUFFIX);
        path.into()
    }

    pub fn parse_object_name(prefix: &str, name: &str) -> Option<(String, bool)> {
        if name.starts_with(TMP_PREFIX) {
            return None;
        }
        match name.strip_suffix(PACKED_SUFFIX) {
            Some(rest) => Some((format!("{prefix}{rest}"), true)),
            None => Some((format!("{prefix}{name}"), false)),
        }
    }

    pub fn find_object(&self, hash: &Hash) -> Option<Object> {
        let raw = self.object_path(hash);
        if raw.is_file() {
            return Some(Object::Raw(raw));
        }
        let packed = self.packed_path(hash);
        packed.is_file().then_some(Object::Packed(packed))
    }

    fn open_packed(path: &Path) -> Result<impl Read + use<>> {
        let mut file = std::io::BufReader::new(File::open(path)?);
        file.read_exact(&mut [0u8])?;
        let mut decoder = zstd::stream::read::Decoder::with_buffer(file)?;
        decoder.window_log_max(MAX_WINDOW_LOG)?;
        Ok(decoder)
    }

    pub fn hash_object(&self, hash: &Hash) -> Result<Hash> {
        match self.find_object(hash) {
            None => bail!("missing"),
            Some(Object::Raw(path)) => hash_file(&path),
            Some(Object::Packed(path)) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update_reader(Self::open_packed(&path)?)?;
                Ok(*hasher.finalize().as_bytes())
            }
        }
    }

    pub fn store_object(&self, src: &Path, entry: &FileEntry) -> Result<bool> {
        if self.find_object(&entry.hash).is_some() {
            return Ok(false);
        }
        let dst = self.object_path(&entry.hash);
        let shard = dst.parent().unwrap();
        fs::create_dir_all(shard)?;
        let changed = || {
            anyhow!(
                "{} changed while being committed; run commit again",
                src.display()
            )
        };
        let unchanged = || -> Result<()> {
            let meta = fs::metadata(src)?;
            if meta.len() != entry.size || system_time_ns(meta.modified()?) != entry.mtime_ns {
                return Err(changed());
            }
            Ok(())
        };

        if CLONES_ARE_ATOMIC && clone_file(src, &dst).is_ok() {
            if let Err(e) = unchanged() {
                let _ = fs::remove_file(&dst);
                return Err(e);
            }
            return Ok(true);
        }
        with_tmp(shard, |tmp| {
            if clone_file(src, tmp).is_ok() {
                unchanged()?;
                fs::rename(tmp, &dst)?;
                return Ok(());
            }
            if entry.size < MIN_PACK_SIZE {
                let mut input = File::open(src)?;
                if copy_hashing(&mut input, &mut File::create(tmp)?)? != entry.hash {
                    return Err(changed());
                }
                fs::rename(tmp, &dst)?;
                return Ok(());
            }
            let mut input =
                File::open(src).with_context(|| format!("reading {}", src.display()))?;
            let (level, header) = if worth_compressing(&mut input, entry.size)? {
                (AUTO_LEVEL, AUTO_LEVEL as u8)
            } else {
                (FAST_LEVEL, SETTLED)
            };
            input.seek(SeekFrom::Start(0))?;
            let hash = pack_hashing(&mut input, entry.size, tmp, level, header)
                .with_context(|| format!("storing {}", src.display()))?;
            if hash != entry.hash {
                return Err(changed());
            }
            fs::rename(tmp, self.packed_path(&entry.hash))?;
            Ok(())
        })?;
        Ok(true)
    }

    pub fn restore_object(&self, entry: &FileEntry, dest: &Path) -> Result<bool> {
        let object = self.find_object(&entry.hash).ok_or_else(|| {
            anyhow!(
                "stored data for {} is missing (object {}); run 'soulgem verify'",
                dest.display(),
                short(&hex(&entry.hash))
            )
        })?;
        let parent = dest.parent().unwrap();
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        if dest.is_dir() {
            bail!(
                "cannot restore {}: a directory is in the way",
                dest.display()
            );
        }
        with_tmp(parent, |tmp| {
            match &object {
                Object::Raw(src) => {
                    if clone_file(src, tmp).is_err() {
                        fs::copy(src, tmp)?;
                    }
                }
                Object::Packed(src) => {
                    let mut out = File::create(tmp)?;
                    if copy_hashing(&mut Self::open_packed(src)?, &mut out)? != entry.hash {
                        bail!(
                            "stored data for {} is corrupted; run 'soulgem verify'",
                            dest.display()
                        );
                    }
                }
            }
            set_mtime(tmp, entry.mtime_ns)?;
            fs::rename(tmp, dest).with_context(|| format!("writing {}", dest.display()))
        })
        .with_context(|| format!("restoring {}", dest.display()))?;
        Ok(matches!(object, Object::Packed(_)))
    }

    pub fn clones_supported(&self) -> bool {
        let tmp = tmp_path(&self.dir);
        let ok = clone_file(&self.dir.join("format"), &tmp).is_ok();
        let _ = fs::remove_file(&tmp);
        ok
    }

    pub fn pack_object(&self, hash: &Hash, size: u64) -> Result<Packed> {
        let raw = self.object_path(hash);
        let before = fs::metadata(&raw)?.len();
        let mut input = File::open(&raw)?;
        let worth = worth_compressing(&mut input, size)?;
        input.seek(SeekFrom::Start(0))?;
        let (level, header) = if worth {
            (AUTO_LEVEL, AUTO_LEVEL as u8)
        } else {
            (FAST_LEVEL, SETTLED)
        };
        let packed = self.packed_path(hash);
        let mut after = 0;
        with_tmp(packed.parent().unwrap(), |tmp| {
            if pack_hashing(&mut input, size, tmp, level, header)? != *hash {
                bail!(
                    "object {} is corrupted; run 'soulgem verify'",
                    short(&hex(hash))
                );
            }
            after = fs::metadata(tmp)?.len();
            fs::rename(tmp, &packed)?;
            Ok(())
        })?;
        fs::remove_file(&raw)?;
        Ok(Packed {
            before,
            after,
            settled: !worth,
        })
    }

    pub fn share_object(&self, src: &Path, entry: &FileEntry) -> Result<u64> {
        let packed = self.packed_path(&entry.hash);
        let raw = self.object_path(&entry.hash);
        with_tmp(raw.parent().unwrap(), |tmp| {
            clone_file(src, tmp)?;
            let meta = fs::metadata(src)?;
            if meta.len() != entry.size || system_time_ns(meta.modified()?) != entry.mtime_ns {
                bail!("{} changed meanwhile", src.display());
            }
            fs::rename(tmp, &raw)?;
            Ok(())
        })?;
        let freed = fs::metadata(&packed)?.len();
        fs::remove_file(&packed)?;
        Ok(freed)
    }

    pub fn write_tree(&self, tree: &Tree) -> Result<String> {
        let raw = postcard::to_allocvec(tree)?;
        let id = blake3::hash(&raw).to_hex().to_string();
        let path = self.dir.join("trees").join(&id);
        if !path.exists() {
            write_atomic(&path, &zstd::encode_all(&raw[..], 3)?)?;
        }
        Ok(id)
    }

    pub fn read_tree(&self, id: &str) -> Result<Tree> {
        let bytes = fs::read(self.dir.join("trees").join(id))
            .with_context(|| format!("tree {} is missing; run 'soulgem verify'", short(id)))?;
        let raw = zstd::decode_all(&bytes[..])
            .with_context(|| format!("tree {} is corrupted", short(id)))?;
        postcard::from_bytes(&raw).with_context(|| format!("tree {} is corrupted", short(id)))
    }

    pub fn tree_of(&self, commit: Option<&str>) -> Result<Tree> {
        match commit {
            Some(id) => self.read_tree(&self.read_commit(id)?.tree),
            None => Ok(Tree::new()),
        }
    }

    pub fn write_commit(&self, commit: &Commit) -> Result<String> {
        let data = serde_json::to_vec_pretty(commit)?;
        let id = blake3::hash(&data).to_hex().to_string();
        write_atomic(&self.dir.join("commits").join(&id), &data)?;
        Ok(id)
    }

    pub fn read_commit(&self, id: &str) -> Result<Commit> {
        let data = fs::read(self.dir.join("commits").join(id))
            .with_context(|| format!("commit {} is missing", short(id)))?;
        let mut commit: Commit = serde_json::from_slice(&data)
            .with_context(|| format!("commit {} is corrupted", short(id)))?;
        commit.id = id.to_string();
        Ok(commit)
    }

    pub fn commit_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.dir.join("commits"))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if !name.starts_with(TMP_PREFIX) {
                ids.push(name);
            }
        }
        Ok(ids)
    }

    pub fn all_commits(&self) -> Result<Vec<Commit>> {
        let mut commits = self
            .commit_ids()?
            .iter()
            .map(|id| self.read_commit(id))
            .collect::<Result<Vec<_>>>()?;
        commits.sort_by_key(|c| std::cmp::Reverse(c.time_ms));
        Ok(commits)
    }

    pub fn rel_path(&self, user_path: &Path) -> Result<String> {
        let mut abs = PathBuf::new();
        for comp in std::path::absolute(user_path)?.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    abs.pop();
                }
                other => abs.push(other),
            }
        }
        let rel = abs.strip_prefix(&self.root).map_err(|_| {
            anyhow!(
                "{} is outside the repository at {}",
                user_path.display(),
                self.root.display()
            )
        })?;
        rel_string(rel)
    }
}

pub fn rel_string(rel: &Path) -> Result<String> {
    let parts = rel
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", rel.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReadThrough<R>(R);

    impl<R: Read> Read for ReadThrough<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl<R: Read> Skip for ReadThrough<R> {}

    fn packed_level(path: &Path) -> i32 {
        let mut level = [0u8];
        File::open(path).unwrap().read_exact(&mut level).unwrap();
        level[0] as i32
    }

    #[test]
    fn packed_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packed");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        for level in [FAST_LEVEL, AUTO_LEVEL] {
            let hash =
                pack_hashing(&mut &data[..], data.len() as u64, &path, level, level as u8).unwrap();
            assert_eq!(hash, *blake3::hash(&data).as_bytes());
            assert_eq!(packed_level(&path), level);
            assert!(fs::metadata(&path).unwrap().len() < data.len() as u64 / 10);
            let mut back = Vec::new();
            Repo::open_packed(&path)
                .unwrap()
                .read_to_end(&mut back)
                .unwrap();
            assert_eq!(back, data);
        }
    }

    #[test]
    fn pack_rejects_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![1u8; 1000];
        assert!(pack_hashing(&mut &data[..], 999, &dir.path().join("p"), FAST_LEVEL, 3).is_err());
    }

    #[test]
    fn probe_separates_compressible_from_random() {
        let text: Vec<u8> = (0..5_000_000u32)
            .map(|i| b"skyrim mod "[i as usize % 11])
            .collect();
        let mut random = vec![0u8; 5_000_000];
        let mut x = 0x9e3779b97f4a7c15u64;
        for b in &mut random {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        assert!(worth_compressing(&mut ReadThrough(&text[..]), text.len() as u64).unwrap());
        assert!(!worth_compressing(&mut ReadThrough(&random[..]), random.len() as u64).unwrap());
        let mut archive = text[..200_000].to_vec();
        archive.extend_from_slice(&random);
        assert!(!worth_compressing(&mut ReadThrough(&archive[..]), archive.len() as u64).unwrap());
    }
}
