//! How fast slices of headers get saved into a database, for tuning the
//! writer. Point it at a copy of a real (big) database: on macOS an APFS clone
//! is instant and leaves the original alone:
//!
//!     for f in atlas.db atlas.s*.db; do cp -c $f /tmp/bench/$f; done
//!     cargo run --release --example bench_writes -- --db /tmp/bench/atlas.db --slices 300 --batch 8
//!
//! Options: `--slices N` slices to save, `--batch K` slices per transaction (per shard),
//! `--slice-size N` headers per slice, `--groups N` groups the slices rotate
//! over, `--per-release N` articles per release, `--checkpoints` checkpoint the
//! way the indexer does (with ATLAS_HOME set to the copy's folder, the copy named
//! atlas.db). Prints headers saved per second.

use std::time::Instant;

use atlas::nntp::{Overview, headers_to_articles};
use atlas::parser::{Release, group_articles};
use atlas::{db, store};

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::args().skip_while(|a| a != name).nth(1).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One slice of `size` headers in `group`, from article `start`: releases of
/// 2 files x `per_release / 2` parts with random looking names, like
/// obfuscated posts (a real database averages about 20 articles per release).
fn slice(group: &str, start: u64, size: u64, per_release: u64, seed: u64) -> Vec<Release> {
    let parts = (per_release / 2).max(1);
    let headers = (start..start + size)
        .map(|n| {
            let rel = n / (parts * 2);
            let (file, part) = ((n / parts) % 2 + 1, n % parts + 1);
            let name = format!("{:016x}", (rel ^ seed).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            Overview {
                number: n,
                subject: format!(r#"[{file:02}/02] - "{name}.part{file:02}.rar" yEnc ({part}/{parts})"#),
                from: "poster <p@x>".into(),
                date: "Sat, 03 Oct 2026 10:00:00 +0000".into(),
                message_id: format!("<{n}.{seed}@bench>"),
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

fn main() {
    let path: String = arg("--db", String::new());
    assert!(!path.is_empty(), "--db PATH (a copy, it gets written to)");
    let (slices, batch, size, groups) = (
        arg("--slices", 200usize),
        arg("--batch", 1usize).max(1),
        arg("--slice-size", 2000u64),
        arg("--groups", 90usize),
    );
    let per_release = arg("--per-release", 20u64);

    let main = std::path::Path::new(&path);
    db::create_db_at(main).unwrap();
    let ids = store::Ids::new(main);
    let mut shards: Vec<(rusqlite::Connection, store::ShardWriter)> = (0..store::SHARDS)
        .map(|i| {
            let conn = db::open_at(&store::shard_path(main, i)).unwrap();
            db::tune_for_writing(&conn).unwrap();
            (conn, store::ShardWriter::new(i))
        })
        .collect();

    // a different seed every run soo every run writes new releases
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let work: Vec<Vec<Release>> = (0..slices)
        .map(|i| {
            let group = format!("alt.binaries.bench{}", i % groups);
            slice(&group, (i / groups) as u64 * size + 1, size, per_release, seed)
        })
        .collect();

    // `--checkpoints`: checkpoint like the indexer does (needs ATLAS_HOME set to
    // the folder holding the copy, named atlas.db)
    let checkpoints = std::env::args().any(|a| a == "--checkpoints");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checkpointer = checkpoints.then(|| db::checkpointer(stop.clone()));

    // one writer, going shard by shard; the indexer runs one per shard in parallel
    let started = Instant::now();
    for chunk in work.chunks(batch) {
        for (shard, (conn, writer)) in shards.iter_mut().enumerate() {
            let mine: Vec<&[Release]> =
                chunk.iter().filter(|s| store::shard_of(&s[0].group) == shard).map(Vec::as_slice).collect();
            if !mine.is_empty() {
                writer.save(conn, &ids, mine).unwrap();
                if checkpoints {
                    db::finish_checkpoint(conn).unwrap();
                }
            }
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(c) = checkpointer {
        c.join().unwrap();
    }
    let secs = started.elapsed().as_secs_f64();
    let headers = slices as f64 * size as f64;
    println!("{slices} slices x {size} headers, {batch} per transaction: {secs:.1}s, {:.0} headers/s", headers / secs);
}
