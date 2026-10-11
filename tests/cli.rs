use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Repo {
    dir: tempfile::TempDir,
    no_clone: bool,
}

impl Repo {
    fn new() -> Repo {
        Self::create(false)
    }

    fn without_clones() -> Repo {
        Self::create(true)
    }

    fn create(no_clone: bool) -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
            no_clone,
        };
        repo.ok(&["init"]);
        repo
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_soulgem"));
        if self.no_clone {
            cmd.env("SOULGEM_NO_CLONE", "1");
        }
        let out = cmd
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (success, out) = self.run(args);
        assert!(success, "soulgem {args:?} failed:\n{out}");
        out
    }

    fn fail(&self, args: &[&str]) -> String {
        let (success, out) = self.run(args);
        assert!(!success, "soulgem {args:?} unexpectedly succeeded:\n{out}");
        out
    }

    fn write(&self, rel: &str, data: &[u8]) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.path(rel)).unwrap()
    }

    fn objects(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for shard in fs::read_dir(self.path(".soulgem/objects")).unwrap() {
            for obj in fs::read_dir(shard.unwrap().path()).unwrap() {
                out.push(obj.unwrap().path());
            }
        }
        out
    }

    fn object_forms(&self) -> (usize, Vec<u8>) {
        let (packed, raw): (Vec<_>, Vec<_>) = self
            .objects()
            .into_iter()
            .partition(|p| p.extension().is_some_and(|e| e == "z"));
        (
            raw.len(),
            packed.iter().map(|p| fs::read(p).unwrap()[0]).collect(),
        )
    }

    fn head(&self) -> String {
        fs::read_to_string(self.path(".soulgem/HEAD")).unwrap()
    }
}

fn exists(p: &Path) -> bool {
    p.exists()
}

#[test]
fn checkout_restores_content_and_removes_added_files() {
    let r = Repo::new();
    r.write("Data/Skyrim.esm", b"base game");
    r.write("Data/textures/sky.dds", &[0u8; 5000]);
    r.ok(&["commit", "Vanilla"]);

    r.write("Data/SkyUI.esp", b"skyui");
    r.write("Data/textures/sky.dds", &[1u8; 6000]);
    r.write("Data/meshes/new/thing.nif", b"mesh");
    r.ok(&["commit", "Installed", "SkyUI"]);

    let out = r.ok(&["checkout", "HEAD~1"]);
    assert!(out.contains("1 reverted, 2 removed"), "{out}");
    assert_eq!(r.read("Data/textures/sky.dds"), vec![0u8; 5000]);
    assert!(!exists(&r.path("Data/SkyUI.esp")));
    assert!(
        !exists(&r.path("Data/meshes")),
        "empty directories should be pruned"
    );
    assert!(r.ok(&["diff"]).contains("No differences"));

    let lc = r.ok(&["lc"]);
    assert!(
        lc.contains("Installed SkyUI") && lc.contains("(HEAD)"),
        "{lc}"
    );
    let newer = lc
        .lines()
        .find(|l| l.contains("Installed SkyUI"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    r.ok(&["checkout", newer]);
    assert_eq!(r.read("Data/SkyUI.esp"), b"skyui");
}

#[test]
fn commit_message_is_optional() {
    let r = Repo::new();
    r.write("a.esp", b"a");
    assert!(r.ok(&["commit"]).contains("Snapshot "));
    assert!(r.ok(&["commit"]).contains("Nothing to commit"));
    r.write("b.esp", b"b");
    assert!(
        r.ok(&["commit", "-5 GB", "+11 GB"])
            .contains("-5 GB +11 GB")
    );
}

#[test]
fn identical_content_is_stored_once() {
    let r = Repo::new();
    r.write("a.bsa", b"same bytes");
    r.write("copy/a.bsa", b"same bytes");
    r.ok(&["commit", "one"]);
    assert_eq!(r.objects().len(), 1);
}

#[test]
fn dirty_tree_blocks_checkout_unless_forced() {
    let r = Repo::new();
    r.write("a.esp", b"1");
    r.ok(&["commit", "one"]);
    r.write("a.esp", b"2");
    r.ok(&["commit", "two"]);
    r.write("a.esp", b"uncommitted");

    assert!(
        r.fail(&["checkout", "HEAD~1"])
            .contains("uncommitted change")
    );
    assert_eq!(r.read("a.esp"), b"uncommitted");
    r.ok(&["checkout", "--force", "HEAD~1"]);
    assert_eq!(r.read("a.esp"), b"1");
}

#[test]
fn checkout_of_paths_leaves_head_and_other_files_alone() {
    let r = Repo::new();
    r.write("mods/a.esp", b"a1");
    r.write("mods-extra/b.esp", b"b1");
    r.write("other.esp", b"o1");
    r.ok(&["commit", "one"]);
    let head = r.head();
    r.write("mods/a.esp", b"a2");
    r.write("mods-extra/b.esp", b"b2");
    r.write("other.esp", b"o2");

    r.ok(&["checkout", "HEAD", "mods"]);
    assert_eq!(r.read("mods/a.esp"), b"a1");
    assert_eq!(
        r.read("mods-extra/b.esp"),
        b"b2",
        "prefix must match whole path components"
    );
    assert_eq!(r.read("other.esp"), b"o2");
    assert_eq!(r.head(), head);
}

#[test]
fn diff_between_revisions_and_working_tree() {
    let r = Repo::new();
    r.write("keep.esp", b"k");
    r.write("change.esp", b"1");
    r.write("gone.esp", b"g");
    r.ok(&["commit", "one"]);
    r.write("change.esp", b"22");
    r.write("new.esp", b"n");
    fs::remove_file(r.path("gone.esp")).unwrap();

    let out = r.ok(&["diff"]);
    assert!(out.contains("A  new.esp"), "{out}");
    assert!(out.contains("M  change.esp"), "{out}");
    assert!(out.contains("D  gone.esp"), "{out}");
    assert!(!out.contains("keep.esp"), "{out}");

    r.ok(&["commit", "two"]);
    assert_eq!(r.ok(&["diff", "HEAD~1", "HEAD"]), out);
}

#[test]
fn ignore_file_is_respected() {
    let r = Repo::new();
    r.write(".soulgemignore", b"*.log\nOverwrite/\n");
    r.write("game.LOG", b"noise");
    r.write("Overwrite/x.ini", b"noise");
    r.write("Data/keep.esp", b"keep");
    let out = r.ok(&["diff"]);
    assert!(
        out.contains("Data/keep.esp") && out.contains(".soulgemignore"),
        "{out}"
    );
    assert!(
        !out.contains("game.LOG") && !out.contains("Overwrite"),
        "{out}"
    );
}

#[test]
fn checkout_restores_modification_times() {
    let r = Repo::new();
    r.write("a.esp", b"a");
    let old = filetime::FileTime::from_unix_time(1_500_000_000, 0);
    filetime::set_file_mtime(r.path("a.esp"), old).unwrap();
    r.ok(&["commit", "one"]);
    r.write("a.esp", b"b");
    r.ok(&["checkout", "-f", "HEAD"]);
    let meta = fs::metadata(r.path("a.esp")).unwrap();
    assert_eq!(filetime::FileTime::from_last_modification_time(&meta), old);
}

#[test]
fn verify_detects_corruption() {
    let r = Repo::new();
    r.write("a.esp", b"content");
    r.ok(&["commit", "one"]);
    assert!(r.ok(&["verify"]).contains("OK"));
    fs::write(&r.objects()[0], b"rotten").unwrap();
    assert!(r.fail(&["verify"]).contains("corrupted: a.esp"));
}

#[test]
fn commit_removes_leftovers_of_interrupted_commands() {
    let r = Repo::new();
    r.write("a.esp", b"content");
    r.ok(&["commit", "one"]);
    let leftovers = [
        r.path(".soulgem/objects/00/.soulgem-tmp-1-2-3"),
        r.path(".soulgem/trees/.soulgem-tmp-1-2-4"),
        r.path(".soulgem/.soulgem-tmp-1-2-5"),
    ];
    for path in &leftovers {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"half-written").unwrap();
    }
    r.write("b.esp", b"more");
    r.ok(&["commit", "two"]);
    for path in &leftovers {
        assert!(!exists(path), "{} should be gone", path.display());
    }
    assert!(r.ok(&["verify"]).contains("OK: 2 objects"));
}

#[test]
fn errors_are_reported() {
    let r = Repo::new();
    assert!(r.fail(&["checkout", "HEAD"]).contains("no commits yet"));
    assert!(r.fail(&["checkout", "zzzz"]).contains("unknown revision"));
    assert!(r.fail(&["init"]).contains("already a soulgem repository"));
    let outside = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_soulgem"))
        .arg("lc")
        .current_dir(outside.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
}

fn mod_data(seed: u8) -> Vec<u8> {
    (0..2_000_000u32)
        .map(|i| ((i / 1000) as u8).wrapping_add(seed))
        .collect()
}

#[test]
fn history_is_compressed_automatically() {
    let r = Repo::new();
    r.write("Data/mod.bsa", &mod_data(1));
    r.ok(&["commit", "v1"]);
    assert_eq!(
        r.object_forms(),
        (1, vec![]),
        "installed data stays a free clone"
    );

    r.write("Data/mod.bsa", &mod_data(2));
    let out = r.ok(&["commit", "v2"]);
    assert!(out.contains("history compressed"), "{out}");
    assert_eq!(r.object_forms(), (1, vec![9]), "v1 packed, v2 a clone");
    let packed = r
        .objects()
        .into_iter()
        .find(|p| p.extension().is_some())
        .unwrap();
    assert!(fs::metadata(packed).unwrap().len() < 100_000);

    let out = r.ok(&["checkout", "HEAD~1"]);
    assert!(out.contains("share space with the store again"), "{out}");
    assert!(
        out.contains("will be compressed by the next commit"),
        "{out}"
    );
    assert_eq!(r.read("Data/mod.bsa"), mod_data(1));
    assert_eq!(
        r.object_forms(),
        (2, vec![]),
        "v1 is a clone again; v2 waits for the next commit"
    );
    assert!(r.ok(&["verify"]).contains("OK: 2 objects"));

    r.write("Data/extra.esp", b"new plugin");
    assert!(
        r.ok(&["commit", "v1 + plugin"])
            .contains("history compressed")
    );
    assert_eq!(
        r.object_forms(),
        (2, vec![9]),
        "v2 compressed by the commit"
    );

    let lc = r.ok(&["lc"]);
    let v2 = lc
        .lines()
        .find(|l| l.contains("v2"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    r.ok(&["checkout", v2]);
    assert_eq!(r.read("Data/mod.bsa"), mod_data(2));
    assert!(!exists(&r.path("Data/extra.esp")));
}

#[test]
fn without_clones_everything_is_compressed() {
    let r = Repo::without_clones();
    r.write("Data/a.bsa", &mod_data(1));
    r.write("Data/b.esp", &mod_data(2));
    r.ok(&["commit", "one"]);
    assert_eq!(
        r.object_forms(),
        (0, vec![9, 9]),
        "commit compresses at the auto level"
    );

    r.write("Data/a.bsa", b"broken");
    r.ok(&["checkout", "-f", "HEAD"]);
    assert_eq!(r.read("Data/a.bsa"), mod_data(1));
    assert!(r.ok(&["verify"]).contains("OK: 2 objects"));
}

#[test]
fn verify_detects_corrupted_packed_object() {
    let r = Repo::without_clones();
    r.write("a.esp", &mod_data(1));
    r.ok(&["commit", "one"]);
    let obj = &r.objects()[0];
    let mut data = fs::read(obj).unwrap();
    let mid = data.len() / 2;
    data[mid] ^= 0xff;
    fs::write(obj, data).unwrap();
    assert!(r.fail(&["verify"]).contains("a.esp"));
    fs::remove_file(r.path("a.esp")).unwrap();
    assert!(r.fail(&["checkout", "-f", "HEAD"]).contains("a.esp"));
}

#[test]
fn incompressible_data_is_marked_and_not_retried() {
    let r = Repo::without_clones();
    let mut x = 0x2545f4914f6cdd1du64;
    let noise: Vec<u8> = (0..3_000_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    r.write("Data/packed.bsa", &noise);
    r.ok(&["commit", "one"]);
    assert_eq!(
        r.object_forms(),
        (0, vec![0]),
        "stored at a fast level and marked done"
    );
    r.write("Data/other.esp", &mod_data(3));
    r.ok(&["commit", "two"]);
    assert_eq!(
        r.object_forms(),
        (0, vec![0, 9]),
        "marked objects are left alone"
    );
    r.ok(&["checkout", "-f", "HEAD"]);
    assert!(r.ok(&["verify"]).contains("OK"));
}

#[test]
fn tiny_files_are_never_compressed() {
    for r in [Repo::new(), Repo::without_clones()] {
        r.write("a.ini", b"x=1");
        r.ok(&["commit", "one"]);
        r.write("a.ini", b"x=2");
        let out = r.ok(&["commit", "two"]);
        assert!(!out.contains("compressed"), "{out}");
        assert_eq!(r.object_forms(), (2, vec![]));
        r.ok(&["checkout", "HEAD~1"]);
        assert_eq!(r.read("a.ini"), b"x=1");
        assert!(r.ok(&["verify"]).contains("OK: 2 objects"));
    }
}
