mod maintain;
mod repo;
mod worktree;

use anyhow::{Result, bail};
use chrono::{Local, TimeZone};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use repo::{Commit, Hash, Repo, Tree, diff, hex, now_ns, short};
use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use worktree::{apply, progress, scan};

#[derive(Parser)]
#[command(
    name = "soulgem",
    version,
    about = "Version control for directories of big binary files (game mods and the like)",
    long_about = "Version control for directories of big binary files (game mods and the like).\n\n\
                  Revisions are HEAD, HEAD~N, or a commit id prefix (at least 4 characters)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Create a repository in the current (or given) directory")]
    Init { path: Option<PathBuf> },
    #[command(about = "Snapshot the working tree; the message is optional")]
    Commit {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
    },
    #[command(about = "List commits, newest first")]
    Lc,
    #[command(about = "Show changes: HEAD vs working tree, REV vs working tree, or REV vs REV")]
    Diff {
        #[arg(num_args = 0..=2)]
        revs: Vec<String>,
    },
    #[command(about = "Restore the working tree, or only the given paths, to a commit")]
    Checkout {
        rev: String,
        #[arg(help = "Only restore these files or directories (HEAD is not moved)")]
        paths: Vec<PathBuf>,
        #[arg(short, long, help = "Discard uncommitted changes")]
        force: bool,
    },
    #[command(about = "Re-hash every stored object to detect corruption")]
    Verify,
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_time(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(|| "?".into(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}

fn print_changes(old: &Tree, new: &Tree) -> Result<usize> {
    let changes = diff(old, new);
    let mut out = BufWriter::new(std::io::stdout().lock());
    for rel in &changes.added {
        writeln!(out, "A  {rel}  ({})", human_size(new[rel].size))?;
    }
    for rel in &changes.modified {
        writeln!(
            out,
            "M  {rel}  ({} -> {})",
            human_size(old[rel].size),
            human_size(new[rel].size)
        )?;
    }
    for rel in &changes.deleted {
        writeln!(out, "D  {rel}")?;
    }
    Ok(changes.len())
}

fn cmd_init(path: Option<PathBuf>) -> Result<()> {
    let dir = Repo::init(&path.unwrap_or_else(|| ".".into()))?;
    println!("Initialized empty soulgem repository in {}", dir.display());
    Ok(())
}

fn cmd_commit(message: Vec<String>) -> Result<()> {
    let repo = Repo::open()?;
    let tree = scan(&repo)?;
    let head = repo.head()?;
    let old = repo.tree_of(head.as_deref())?;
    let changes = diff(&old, &tree);
    if head.is_some() && changes.is_empty() {
        println!("Nothing to commit, the working tree matches HEAD.");
        return Ok(());
    }

    let known: HashSet<&Hash> = old.values().map(|e| &e.hash).collect();
    let mut seen = HashSet::new();
    let candidates: Vec<_> = tree
        .iter()
        .filter(|(_, e)| !known.contains(&e.hash) && seen.insert(e.hash))
        .collect();
    let bar = progress("storing", candidates.iter().map(|(_, e)| e.size).sum());
    let stored = AtomicU64::new(0);
    let store_pool = if repo.clones_supported() {
        repo::io_pool()?
    } else {
        rayon::ThreadPoolBuilder::new().build()?
    };
    let result = store_pool.install(|| {
        candidates.par_iter().try_for_each(|(rel, e)| {
            if repo.store_object(&repo.root.join(rel), e)? {
                stored.fetch_add(e.size, Ordering::Relaxed);
            }
            bar.inc(e.size);
            Ok::<_, anyhow::Error>(())
        })
    });
    bar.finish_and_clear();
    result?;

    let time_ms = now_ns() / 1_000_000;
    let message = match message.join(" ").trim() {
        "" => format!("Snapshot {}", format_time(time_ms)),
        m => m.to_string(),
    };
    let commit = Commit {
        id: String::new(),
        tree: repo.write_tree(&tree)?,
        parent: head,
        message,
        time_ms,
        files: tree.len() as u64,
        size: tree.values().map(|e| e.size).sum(),
        added: changes.added.len() as u64,
        modified: changes.modified.len() as u64,
        deleted: changes.deleted.len() as u64,
    };
    let id = repo.write_commit(&commit)?;
    repo.set_head(&id)?;
    println!("[{}] {}", short(&id), commit.message);
    println!(
        "  {} added, {} modified, {} deleted; {} of new data stored",
        commit.added,
        commit.modified,
        commit.deleted,
        human_size(stored.into_inner())
    );

    print_compressed("history", &maintain::compress_history(&repo, &tree)?);
    Ok(())
}

fn cmd_lc() -> Result<()> {
    let repo = Repo::open()?;
    let head = repo.head()?;
    let commits = repo.all_commits()?;
    if commits.is_empty() {
        println!("No commits yet.");
    }
    let mut out = BufWriter::new(std::io::stdout().lock());
    for c in commits {
        let is_head = head.as_deref() == Some(c.id.as_str());
        writeln!(
            out,
            "{} {}  {}  {:>18}  {:>7} files  {:>9}  {}{}",
            if is_head { '*' } else { ' ' },
            short(&c.id),
            format_time(c.time_ms),
            format!("+{} ~{} -{}", c.added, c.modified, c.deleted),
            c.files,
            human_size(c.size),
            c.message,
            if is_head { "  (HEAD)" } else { "" },
        )?;
    }
    Ok(())
}

fn cmd_diff(revs: Vec<String>) -> Result<()> {
    let repo = Repo::open()?;
    let (old, new) = match revs.as_slice() {
        [] => (repo.tree_of(repo.head()?.as_deref())?, scan(&repo)?),
        [a] => (repo.tree_of(Some(&repo.resolve(a)?))?, scan(&repo)?),
        [a, b] => (
            repo.tree_of(Some(&repo.resolve(a)?))?,
            repo.tree_of(Some(&repo.resolve(b)?))?,
        ),
        _ => bail!("diff takes at most two revisions"),
    };
    if print_changes(&old, &new)? == 0 {
        println!("No differences.");
    }
    Ok(())
}

fn cmd_checkout(rev: String, paths: Vec<PathBuf>, force: bool) -> Result<()> {
    let repo = Repo::open()?;
    let id = repo.resolve(&rev)?;
    let mut target = repo.tree_of(Some(&id))?;
    let worktree = scan(&repo)?;
    let mut current = worktree.clone();

    if !paths.is_empty() {
        let prefixes = paths
            .iter()
            .map(|p| repo.rel_path(p))
            .collect::<Result<Vec<_>>>()?;
        let selected = |rel: &String| {
            prefixes.iter().any(|p| {
                p.is_empty()
                    || rel == p
                    || (rel.starts_with(p.as_str()) && rel.as_bytes()[p.len()] == b'/')
            })
        };
        target.retain(|rel, _| selected(rel));
        current.retain(|rel, _| selected(rel));
        if target.is_empty() && current.is_empty() {
            bail!(
                "the given paths match nothing in the working tree or in {}",
                short(&id)
            );
        }
    } else if !force {
        let dirty = diff(&repo.tree_of(repo.head()?.as_deref())?, &current);
        if !dirty.is_empty() {
            bail!(
                "you have {} uncommitted change(s) (see 'soulgem diff').\n\
                 Commit them first, or use --force to throw them away.",
                dirty.len()
            );
        }
    }

    let (changes, decompressed) = apply(&repo, &current, &target)?;
    if paths.is_empty() {
        repo.set_head(&id)?;
        println!(
            "HEAD is now at {}: {}",
            short(&id),
            repo.read_commit(&id)?.message
        );
    }
    println!(
        "  {} restored, {} reverted, {} removed",
        changes.added.len(),
        changes.modified.len(),
        changes.deleted.len()
    );

    let (reshared, freed) = maintain::reshare(&repo, &decompressed)?;
    if reshared > 0 {
        println!(
            "  {reshared} restored file(s) share space with the store again ({} freed)",
            human_size(freed)
        );
    }
    let after: HashSet<&Hash> = worktree
        .iter()
        .filter(|(rel, _)| !current.contains_key(*rel))
        .chain(&target)
        .map(|(_, e)| &e.hash)
        .collect();
    let left = current
        .values()
        .filter(|e| !after.contains(&e.hash))
        .map(|e| (e.hash, e.size));
    let pending = maintain::uncompressed_bytes(&repo, left);
    if pending > 0 {
        println!(
            "  {} you rolled away from will be compressed by the next commit",
            human_size(pending)
        );
    }
    Ok(())
}

fn print_compressed(what: &str, stats: &maintain::Stats) {
    if stats.objects == 0 {
        return;
    }
    print!(
        "  {what} compressed: {} -> {} ({:.0}% saved)",
        human_size(stats.before),
        human_size(stats.after),
        stats.saved_percent()
    );
    if stats.settled > 0 {
        print!(
            "; {} of it already compressed, kept as is",
            human_size(stats.settled_bytes)
        );
    }
    println!();
}

fn cmd_verify() -> Result<()> {
    let repo = Repo::open()?;
    let commits = repo.all_commits()?;
    let mut problems = Vec::new();
    let mut first_use_by_hash: HashMap<Hash, (String, u64)> = HashMap::new();
    for c in &commits {
        match repo.read_tree(&c.tree) {
            Ok(tree) => {
                for (rel, e) in tree {
                    first_use_by_hash.entry(e.hash).or_insert((rel, e.size));
                }
            }
            Err(e) => problems.push(format!("commit {}: {e:#}", short(&c.id))),
        }
    }

    let bar = progress(
        "verifying",
        first_use_by_hash.values().map(|(_, size)| size).sum(),
    );
    let mut bad: Vec<String> = first_use_by_hash
        .par_iter()
        .filter_map(|(hash, (rel, size))| {
            let status = match repo.hash_object(hash) {
                Ok(h) if h == *hash => None,
                Ok(_) => Some("corrupted"),
                Err(_) if repo.find_object(hash).is_none() => Some("missing"),
                Err(_) => Some("unreadable"),
            };
            bar.inc(*size);
            status.map(|s| format!("{s}: {rel} (object {})", short(&hex(hash))))
        })
        .collect();
    bar.finish_and_clear();
    bad.sort();
    problems.extend(bad);

    for p in &problems {
        println!("{p}");
    }
    if !problems.is_empty() {
        bail!("{} problem(s) found", problems.len());
    }
    println!(
        "OK: {} objects in {} commits verified",
        first_use_by_hash.len(),
        commits.len()
    );
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init { path } => cmd_init(path),
        Command::Commit { message } => cmd_commit(message),
        Command::Lc => cmd_lc(),
        Command::Diff { revs } => cmd_diff(revs),
        Command::Checkout { rev, paths, force } => cmd_checkout(rev, paths, force),
        Command::Verify => cmd_verify(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("soulgem: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
