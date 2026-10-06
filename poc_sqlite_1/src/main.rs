//! Proof of concept: which changes make the Atlas database writer faster on a
//! real, big database (bigger than RAM, on NVMe)?
//!
//! Every variant saves the same synthetic slices with Atlas's own save code
//! (`atlas::db::save_release_batches`) into fresh APFS clones of the real
//! database (`cp -c`: instant, the original is never touched), and reports
//! headers saved per second. Variants:
//!
//! - `baseline`: what the indexer does now. 8 slices per transaction, the
//!   writer finishes a checkpoint after every transaction, a background thread
//!   checkpoints passively twice a second.
//! - `sorted`: the releases of a transaction saved in (name, group) order, soo
//!   index pages are visited in order instead of at random.
//! - `wal256`: the writer only finishes a checkpoint once the WAL reaches
//!   256MB, soo a page changed by many transactions is copied once.
//! - `b32`: 32 slices per transaction instead of 8.
//! - `shardsN`: N writers in parallel, each on its own database (a clone of
//!   the whole database, soo each shard is as big as today's: the worst case;
//!   real shards would be 1/N the size). Groups are split between them.
//!
//! Variants combine with `+`, e.g. `sorted+wal256+shards4`.
//!
//!     cargo run --release -- --db ../atlas.db --work /path/on/same/volume
//!         [--variants baseline,sorted,...] [--rounds 2] [--slices 3000]
//!         [--slice-size 1000] [--per-release 20] [--groups 90]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use atlas::db;
use atlas::nntp::{Overview, headers_to_articles};
use atlas::parser::{Release, group_articles};
use rusqlite::Connection;

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::args().skip_while(|a| a != name).nth(1).and_then(|v| v.parse().ok()).unwrap_or(default)
}

const DEFAULT_VARIANTS: &str =
    "baseline,sorted,wal256,sorted+wal256,sorted+wal256+b32,sorted+wal256+shards4,sorted+wal256+shards8";

/// One variant, parsed from e.g. `sorted+wal256+shards4`.
#[derive(Clone, Debug)]
struct Variant {
    name: String,
    sorted: bool,
    /// finish a checkpoint once the WAL is this big (0 = after every transaction)
    wal_limit: u64,
    batch: usize,
    shards: usize,
}

impl Variant {
    fn parse(name: &str) -> Variant {
        let mut v = Variant { name: name.to_string(), sorted: false, wal_limit: 0, batch: 8, shards: 1 };
        for part in name.split('+') {
            match part {
                "baseline" => {}
                "sorted" => v.sorted = true,
                "wal256" => v.wal_limit = 256 << 20,
                p if p.starts_with('b') && p[1..].parse::<usize>().is_ok() => v.batch = p[1..].parse().unwrap(),
                p if p.starts_with("shards") => v.shards = p[6..].parse().expect("shardsN"),
                other => panic!("unknown variant part {other}"),
            }
        }
        v
    }
}

/// One slice of `size` headers in `group` from article `start`: releases of
/// 2 files x `per_release / 2` parts with random looking names, like
/// obfuscated posts (the real database averages about 20-30 articles per release).
fn slice(group: &str, start: u64, size: u64, per_release: u64, seed: u64) -> Vec<Release> {
    let parts = (per_release / 2).max(1);
    let headers = (start..start + size)
        .map(|n| {
            let rel = n / (parts * 2);
            let (file, part) = ((n / parts) % 2 + 1, n % parts + 1);
            let name = format!("{:016x}", (rel ^ seed ^ hash(group)).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            Overview {
                number: n,
                subject: format!(r#"[{file:02}/02] - "{name}.part{file:02}.rar" yEnc ({part}/{parts})"#),
                from: "poster <p@x>".into(),
                date: "Sat, 03 Oct 2026 10:00:00 +0000".into(),
                message_id: format!("<{n}.{seed}.{}@poc>", hash(group)),
                references: String::new(),
                bytes: 750_000,
                lines: 5000,
            }
        })
        .collect();

    let mut releases: Vec<Release> = group_articles(headers_to_articles(headers)).into_values().collect();
    for r in &mut releases {
        r.group = group.to_string();
        r.poster = r.articles[0].author.clone();
        r.date = r.articles[0].date.clone();
        r.complete = true;
    }
    releases
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn wal_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    PathBuf::from(p)
}

/// APFS clone of the database and its WAL (instant, copy on write).
fn clone_db(src: &Path, dst: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", dst.display()));
    }
    let wal = wal_path(src);
    if wal.exists() {
        let ok = std::process::Command::new("cp").arg("-c").arg(&wal).arg(wal_path(dst)).status().unwrap().success();
        assert!(ok, "cp -c of the wal failed (needs APFS, same volume)");
    }
    let ok = std::process::Command::new("cp").arg("-c").arg(src).arg(dst).status().unwrap().success();
    assert!(ok, "cp -c failed (needs APFS, and --work on the same volume as --db)");
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Passive checkpoints twice a second, like `atlas::db::checkpointer`, for any path.
fn checkpointer(path: PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let conn = db::open_at(&path).unwrap();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            let _ = conn.query_row("pragma wal_checkpoint(passive)", [], |_| Ok(()));
        }
    })
}

#[derive(Default)]
struct Totals {
    checkpoint_ns: AtomicU64,
    wal_max: AtomicU64,
}

/// One writer saving `work` into the database at `path` the way `v` says.
fn writer(path: &Path, work: &[Vec<Release>], v: &Variant, totals: &Totals) {
    let mut conn: Connection = db::open_at(path).unwrap();
    db::tune_for_writing(&conn).unwrap();
    let wal = wal_path(path);

    for chunk in work.chunks(v.batch) {
        if v.sorted {
            let mut all: Vec<&Release> = chunk.iter().flatten().collect();
            all.sort_by(|a, b| (&a.name, &a.group).cmp(&(&b.name, &b.group)));
            let owned: Vec<Release> = all.into_iter().cloned().collect();
            db::save_release_batches(&mut conn, [owned.as_slice()]).unwrap();
        } else {
            db::save_release_batches(&mut conn, chunk.iter().map(Vec::as_slice)).unwrap();
        }

        let size = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        totals.wal_max.fetch_max(size, Ordering::Relaxed);
        if size >= v.wal_limit {
            let t = Instant::now();
            db::finish_checkpoint(&conn).unwrap();
            totals.checkpoint_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
    // what is still in the WAL is part of the cost
    let t = Instant::now();
    db::finish_checkpoint(&conn).unwrap();
    totals.checkpoint_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

fn main() {
    let src = PathBuf::from(arg("--db", String::from("../atlas.db")));
    let work_dir = PathBuf::from(arg("--work", String::from("target/poc-work")));
    let variants: Vec<Variant> = arg("--variants", DEFAULT_VARIANTS.to_string()).split(',').map(Variant::parse).collect();
    let rounds = arg("--rounds", 2usize);
    let (slices, size, per_release, groups) =
        (arg("--slices", 3000usize), arg("--slice-size", 1000u64), arg("--per-release", 20u64), arg("--groups", 90usize));

    assert!(src.exists(), "{} not found", src.display());
    std::fs::create_dir_all(&work_dir).unwrap();
    println!(
        "{slices} slices x {size} headers over {groups} groups, {rounds} round(s), database {}",
        std::fs::metadata(&src).map(|m| format!("{:.1}GB", m.len() as f64 / 1e9)).unwrap_or_default()
    );

    for round in 1..=rounds {
        for v in &variants {
            // new releases every run
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
            let shard_paths: Vec<PathBuf> = (0..v.shards).map(|i| work_dir.join(format!("shard{i}.db"))).collect();
            for p in &shard_paths {
                clone_db(&src, p);
            }

            // slices in arrival order; each shard gets the groups that hash to it
            let mut per_shard: Vec<Vec<Vec<Release>>> = vec![Vec::new(); v.shards];
            for i in 0..slices {
                let group = format!("alt.binaries.poc{}", i % groups);
                let start = (i / groups) as u64 * size + 1;
                per_shard[(hash(&group) % v.shards as u64) as usize].push(slice(&group, start, size, per_release, seed));
            }

            // open once untimed: recovers the cloned WAL
            for p in &shard_paths {
                drop(db::open_at(p).unwrap().query_row("select count(*) from sqlite_master", [], |r| r.get::<_, i64>(0)));
            }

            let totals = Totals::default();
            let stop = Arc::new(AtomicBool::new(false));
            let checkpointers: Vec<_> = shard_paths.iter().map(|p| checkpointer(p.clone(), stop.clone())).collect();

            let started = Instant::now();
            std::thread::scope(|s| {
                for (path, work) in shard_paths.iter().zip(&per_shard) {
                    let (v, totals) = (v, &totals);
                    s.spawn(move || writer(path, work, v, totals));
                }
            });
            let secs = started.elapsed().as_secs_f64();

            stop.store(true, Ordering::Relaxed);
            for c in checkpointers {
                c.join().unwrap();
            }
            for p in &shard_paths {
                remove_db(p);
            }

            let headers = slices as f64 * size as f64;
            println!(
                "round {round}  {:<28} {:>8.0} headers/s  ({secs:5.1}s, writer checkpointing {:4.1}s, wal max {:5.0}MB)",
                v.name,
                headers / secs,
                totals.checkpoint_ns.load(Ordering::Relaxed) as f64 / 1e9,
                totals.wal_max.load(Ordering::Relaxed) as f64 / 1048576.0
            );
        }
    }
}
