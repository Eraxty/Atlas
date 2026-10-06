//! Proof of concept: store less per article, and split the database.
//!
//! `storage`: copies a sample of real releases (with their articles) out of
//! atlas.db (read only) into today's layout and into the compact layouts, and
//! compares the sizes. Then builds every sampled release's NZB from today's
//! layout and from the compact ones: they have to be byte for byte the same.
//!
//!     cargo run --release -- storage --db ../atlas.db --sample 50000
//!
//! `speed`: fills databases in each layout with the same number of synthetic
//! articles, then times saving new slices into clones of them:
//! - `current`: today's layout and save code (atlas::db::save_release_batches)
//! - `compact1` / `compact2`: the compact layouts (see compact.rs)
//! - `compact2+shards8`: 8 databases, groups split between them, one writer each
//! - `memseg`: compact2 written into an in-memory database; every `--seg-mb`
//!   it is written out to its own segment file in one go (`vacuum into`, a
//!   sequential write) and a fresh one starts. Never touches the big database.
//!
//!     cargo run --release -- speed --work target/work --prefill 100000000

mod compact;

use std::path::{Path, PathBuf};
use std::time::Instant;

use atlas::db;
use atlas::nntp::{Overview, headers_to_articles};
use atlas::parser::{Article, Release, group_articles};
use atlas::search::{ArticleRow, ReleaseRow};
use compact::{Domains, Flavor, Writer};
use rusqlite::{Connection, OpenFlags, params};

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::args().skip_while(|a| a != name).nth(1).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("storage") => storage(),
        Some("speed") => speed(),
        _ => eprintln!("usage: poc_sqlite_2 storage|speed [options], see src/main.rs"),
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn fresh(path: &Path, flavor: Option<Flavor>) -> Connection {
    remove_db(path);
    db::create_db_at(path).unwrap();
    let conn = db::open_at(path).unwrap();
    if let Some(f) = flavor {
        compact::create(&conn, f).unwrap();
    }
    conn
}

// ---------------------------------------------------------------- storage

/// The newest `n` releases of the real database, articles included, as the
/// indexer would hand them to the save code.
fn sample(src: &Path, n: i64) -> Vec<Release> {
    let conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut releases_stmt = conn
        .prepare(
            "select * from (select id, name, group_name, poster, posted_date, display_name, is_obfuscated
             from releases order by id desc limit ?) order by id",
        )
        .unwrap();
    let mut articles_stmt = conn
        .prepare(
            "select message_id, subject, filename, part, total_parts, bytes, file_total from articles
             where release_id = ? order by id",
        )
        .unwrap();

    let rows: Vec<(i64, String, String, Option<String>, Option<String>, Option<String>, Option<i64>)> = releases_stmt
        .query_map([n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    rows.into_iter()
        .map(|(id, name, group, poster, date, display_name, obfuscated)| {
            let (poster, date) = (poster.unwrap_or_default(), date.unwrap_or_default());
            let articles: Vec<Article> = articles_stmt
                .query_map([id], |r| {
                    Ok(Article {
                        number: 0,
                        subject: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        author: poster.clone(),
                        date: date.clone(),
                        message_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        references: String::new(),
                        bytes: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        lines: 0,
                        filename: r.get(2)?,
                        release_name: None,
                        part: r.get(3)?,
                        total_parts: r.get(4)?,
                        file_index: None,
                        file_total: r.get(6)?,
                    })
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            Release {
                name,
                articles,
                size: 0,
                is_obfuscated: obfuscated.unwrap_or(0) != 0,
                display_name,
                complete: false,
                group,
                poster,
                date,
            }
        })
        .collect()
}

/// A release's articles from today's layout, the way atlas::search::get_articles reads them.
fn current_articles(conn: &Connection, release_id: i64) -> Vec<ArticleRow> {
    let mut stmt = conn
        .prepare_cached(
            "select articles.message_id, articles.filename,
                articles.part, articles.total_parts, articles.bytes,
                articles.subject, releases.poster, releases.posted_date
             from articles join releases on articles.release_id = releases.id
             where articles.release_id = ?
             order by articles.filename, articles.part",
        )
        .unwrap();
    stmt.query_map([release_id], |r| {
        Ok(ArticleRow {
            message_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            filename: r.get(1)?,
            part: r.get(2)?,
            total_parts: r.get(3)?,
            bytes: r.get(4)?,
            subject: r.get(5)?,
            poster: r.get(6)?,
            posted_date: r.get(7)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn release_row(conn: &Connection, name: &str, group: &str) -> Option<ReleaseRow> {
    conn.query_row(
        "select id, name, group_name, poster, posted_date, size, complete, parts from releases where name = ? and group_name = ?",
        params![name, group],
        |r| {
            Ok(ReleaseRow {
                id: r.get(0)?,
                name: r.get(1)?,
                group_name: r.get(2)?,
                poster: r.get(3)?,
                posted_date: r.get(4)?,
                size: r.get(5)?,
                complete: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                parts: r.get(7)?,
            })
        },
    )
    .ok()
}

fn storage() {
    let src = PathBuf::from(arg("--db", String::from("../atlas.db")));
    let work = PathBuf::from(arg("--work", String::from("target/work")));
    let n = arg("--sample", 50_000i64);
    std::fs::create_dir_all(&work).unwrap();

    let t = Instant::now();
    let releases = sample(&src, n);
    let articles: usize = releases.iter().map(|r| r.articles.len()).sum();
    println!("sampled {} releases, {articles} articles in {:.0}s", releases.len(), t.elapsed().as_secs_f64());

    // the same releases into each layout, in slice sized batches
    let layouts: [(&str, Option<Flavor>); 3] =
        [("current", None), ("compact1", Some(Flavor::Plain)), ("compact2", Some(Flavor::Packed))];
    let mut sizes = Vec::new();
    for (name, flavor) in layouts {
        let path = work.join(format!("storage_{name}.db"));
        let mut conn = fresh(&path, flavor);
        let mut writer = flavor.map(Writer::new);
        for chunk in releases.chunks(200) {
            match writer.as_mut() {
                None => db::save_release_batches(&mut conn, [chunk]).unwrap(),
                Some(w) => w.save(&mut conn, [chunk]).unwrap(),
            }
        }
        conn.execute_batch("pragma wal_checkpoint(truncate); vacuum;").unwrap();
        drop(conn);
        let bytes = file_size(&path);
        sizes.push(bytes);
        println!(
            "{name:9} {:>8.1}MB  {:>6.1} bytes per article  ({:.2}x of current)",
            bytes as f64 / 1e6,
            bytes as f64 / articles as f64,
            bytes as f64 / sizes[0] as f64
        );
    }

    // every release's nzb, today's layout against each compact one
    let current = db::open_at(&work.join("storage_current.db")).unwrap();
    for (name, flavor) in [("compact1", Flavor::Plain), ("compact2", Flavor::Packed)] {
        let other = db::open_at(&work.join(format!("storage_{name}.db"))).unwrap();
        let mut domains = Domains::default();
        let (mut same, mut differ, mut first_diff) = (0, 0, None);
        for r in &releases {
            let (Some(a), Some(b)) = (release_row(&current, &r.name, &r.group), release_row(&other, &r.name, &r.group))
            else {
                continue;
            };
            let want = atlas::nzb::render_nzb(&a, &current_articles(&current, a.id));
            let got = atlas::nzb::render_nzb(&b, &compact::articles(&other, flavor, &mut domains, b.id).unwrap());
            let stats_match = (a.size, a.complete, a.parts) == (b.size, b.complete, b.parts);
            if want == got && stats_match {
                same += 1;
            } else {
                differ += 1;
                first_diff.get_or_insert((r.name.clone(), want, got, stats_match));
            }
        }
        println!("{name}: {same} NZBs (and release size/parts/complete) identical, {differ} different");
        if let Some((release, want, got, stats_match)) = first_diff {
            println!("  first difference: {release} (stats match: {stats_match})");
            for (a, b) in want.lines().zip(got.lines()).filter(|(a, b)| a != b).take(3) {
                println!("  - {a}\n  + {b}");
            }
        }
    }
    for (name, _) in layouts {
        remove_db(&work.join(format!("storage_{name}.db")));
    }
}

// ---------------------------------------------------------------- speed

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// One slice of synthetic headers, like poc_sqlite_1: obfuscated looking
/// releases of 2 files, `per_release` articles each, with message-ids shaped
/// like real ones (hex locals at a few posting tools' domains).
fn slice(group: &str, start: u64, size: u64, per_release: u64, seed: u64) -> Vec<Release> {
    let parts = (per_release / 2).max(1);
    let domains = ["@ngPost", "@nyuu", "@camelsystem-powerpost.local", "@JBinUp.local"];
    let headers = (start..start + size)
        .map(|n| {
            let rel = n / (parts * 2);
            let (file, part) = ((n / parts) % 2 + 1, n % parts + 1);
            let name = format!("{:016x}", (rel ^ seed ^ hash(group)).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let id = (n ^ seed ^ hash(group)).wrapping_mul(0x2545_F491_4F6C_DD1D);
            Overview {
                number: n,
                subject: format!(r#"[{file:02}/02] - "{name}.part{file:02}.rar" yEnc ({part}/{parts})"#),
                from: "poster <p@x>".into(),
                date: "Sat, 03 Oct 2026 10:00:00 +0000".into(),
                message_id: format!("<{id:016x}{:016x}{}>", id.rotate_left(17), domains[(n % 4) as usize]),
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

/// `slices` slices rotating over `groups` groups, groups `group_offset..`
fn workload(slices: usize, size: u64, groups: usize, seed: u64, first_number: u64) -> Vec<(String, Vec<Release>)> {
    (0..slices)
        .map(|i| {
            let group = format!("alt.binaries.poc{}", i % groups);
            let start = first_number + (i / groups) as u64 * size;
            let releases = slice(&group, start, size, 20, seed);
            (group, releases)
        })
        .collect()
}

/// Save `work` into `conn` in batches of 8 slices, the way the indexer does.
fn write(conn: &mut Connection, writer: &mut Option<Writer>, work: &[&Vec<Release>]) {
    for chunk in work.chunks(8) {
        match writer.as_mut() {
            None => db::save_release_batches(conn, chunk.iter().map(|r| r.as_slice())).unwrap(),
            Some(w) => w.save(conn, chunk.iter().map(|r| r.as_slice())).unwrap(),
        }
        let _ = db::finish_checkpoint(conn);
    }
}

fn clone_db(src: &Path, dst: &Path) {
    remove_db(dst);
    let ok = std::process::Command::new("cp").arg("-c").arg(src).arg(dst).status().unwrap().success();
    assert!(ok, "cp -c failed (needs APFS)");
}

fn speed() {
    let work_dir = PathBuf::from(arg("--work", String::from("target/work")));
    let prefill = arg("--prefill", 100_000_000u64);
    let (slices, size, groups, rounds) =
        (arg("--slices", 3000usize), arg("--slice-size", 1000u64), arg("--groups", 90usize), arg("--rounds", 2usize));
    let seg_mb = arg("--seg-mb", 2048u64);
    let shards = 8usize;
    std::fs::create_dir_all(&work_dir).unwrap();

    // templates: each layout filled with `prefill` articles once, cloned per run
    let fill_slices = (prefill / size) as usize;
    let template = |name: &str| work_dir.join(format!("template_{name}.db"));
    let layouts: [(&str, Option<Flavor>); 3] =
        [("current", None), ("compact1", Some(Flavor::Plain)), ("compact2", Some(Flavor::Packed))];
    for (name, flavor) in layouts {
        if template(name).exists() {
            continue;
        }
        let t = Instant::now();
        let mut conn = fresh(&template(name), flavor);
        db::tune_for_writing(&conn).unwrap();
        let mut writer = flavor.map(Writer::new);
        // filled in chunks soo the workload never sits in memory whole
        for start in (0..fill_slices).step_by(groups * 50) {
            let n = (groups * 50).min(fill_slices - start);
            let work = workload(n, size, groups, 1, 1 + (start / groups) as u64 * size);
            let refs: Vec<&Vec<Release>> = work.iter().map(|(_, r)| r).collect();
            write(&mut conn, &mut writer, &refs);
        }
        conn.execute_batch("pragma wal_checkpoint(truncate)").unwrap();
        println!(
            "filled {name} with {prefill} articles: {:.1}GB in {:.0}s",
            file_size(&template(name)) as f64 / 1e9,
            t.elapsed().as_secs_f64()
        );
    }
    // shards: the same data split by group
    let shard_template = |i: usize| work_dir.join(format!("template_shard{i}.db"));
    if !shard_template(0).exists() {
        let t = Instant::now();
        std::thread::scope(|s| {
            for i in 0..shards {
                s.spawn(move || {
                    let mut conn = fresh(&shard_template(i), Some(Flavor::Packed));
                    db::tune_for_writing(&conn).unwrap();
                    let mut writer = Some(Writer::new(Flavor::Packed));
                    for start in (0..fill_slices).step_by(groups * 50) {
                        let n = (groups * 50).min(fill_slices - start);
                        let work = workload(n, size, groups, 1, 1 + (start / groups) as u64 * size);
                        let mine: Vec<&Vec<Release>> = work
                            .iter()
                            .filter(|(g, _)| hash(g) % shards as u64 == i as u64)
                            .map(|(_, r)| r)
                            .collect();
                        write(&mut conn, &mut writer, &mine);
                    }
                    conn.execute_batch("pragma wal_checkpoint(truncate)").unwrap();
                });
            }
        });
        let total: u64 = (0..shards).map(|i| file_size(&shard_template(i))).sum();
        println!("filled 8 shards: {:.1}GB in total in {:.0}s", total as f64 / 1e9, t.elapsed().as_secs_f64());
    }

    let headers = slices as f64 * size as f64;
    for round in 1..=rounds {
        // new releases, numbered after the prefill
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
        let work = workload(slices, size, groups, seed, 1 + prefill);
        let all: Vec<&Vec<Release>> = work.iter().map(|(_, r)| r).collect();

        for (name, flavor) in layouts {
            let path = work_dir.join(format!("run_{name}.db"));
            clone_db(&template(name), &path);
            let mut conn = db::open_at(&path).unwrap();
            db::tune_for_writing(&conn).unwrap();
            let mut writer = flavor.map(Writer::new);
            let t = Instant::now();
            write(&mut conn, &mut writer, &all);
            let secs = t.elapsed().as_secs_f64();
            drop(conn);
            remove_db(&path);
            println!("round {round}  {name:18} {:>9.0} headers/s", headers / secs);
        }

        // 8 shards, one writer each
        let paths: Vec<PathBuf> = (0..shards).map(|i| work_dir.join(format!("run_shard{i}.db"))).collect();
        for (i, p) in paths.iter().enumerate() {
            clone_db(&shard_template(i), p);
        }
        let t = Instant::now();
        std::thread::scope(|s| {
            for (i, p) in paths.iter().enumerate() {
                let mine: Vec<&Vec<Release>> =
                    work.iter().filter(|(g, _)| hash(g) % shards as u64 == i as u64).map(|(_, r)| r).collect();
                s.spawn(move || {
                    let mut conn = db::open_at(p).unwrap();
                    db::tune_for_writing(&conn).unwrap();
                    write(&mut conn, &mut Some(Writer::new(Flavor::Packed)), &mine);
                });
            }
        });
        println!("round {round}  {:18} {:>9.0} headers/s", "compact2+shards8", headers / t.elapsed().as_secs_f64());
        for p in &paths {
            remove_db(p);
        }

        // in-memory segments written out sequentially
        let t = Instant::now();
        let mut segments = 0;
        let mut flush_secs = 0.0;
        let new_segment = || {
            let conn = Connection::open_in_memory().unwrap();
            copy_schema(&template("compact2"), &conn);
            conn
        };
        let mut mem = new_segment();
        let mut writer = Some(Writer::new(Flavor::Packed));
        for chunk in all.chunks(8) {
            write(&mut mem, &mut writer, chunk);
            let used: i64 = mem.query_row("select page_count * page_size from pragma_page_count, pragma_page_size", [], |r| r.get(0)).unwrap();
            if used as u64 >= seg_mb << 20 {
                let f = Instant::now();
                let seg = work_dir.join(format!("segment{segments}.db"));
                remove_db(&seg);
                mem.execute("vacuum into ?", [seg.to_str().unwrap()]).unwrap();
                flush_secs += f.elapsed().as_secs_f64();
                segments += 1;
                mem = new_segment();
                writer = Some(Writer::new(Flavor::Packed));
            }
        }
        // the last, partial segment counts too
        let f = Instant::now();
        let seg = work_dir.join(format!("segment{segments}.db"));
        remove_db(&seg);
        mem.execute("vacuum into ?", [seg.to_str().unwrap()]).unwrap();
        flush_secs += f.elapsed().as_secs_f64();
        let secs = t.elapsed().as_secs_f64();
        println!(
            "round {round}  {:18} {:>9.0} headers/s  ({} segment(s), {flush_secs:.1}s of it writing them out)",
            "memseg",
            headers / secs,
            segments + 1
        );
        for i in 0..=segments {
            remove_db(&work_dir.join(format!("segment{i}.db")));
        }
    }
}

/// The schema of `template` (tables, indexes, the search index and its
/// triggers) recreated in `conn`. The search index makes its own shadow tables.
fn copy_schema(template: &Path, conn: &Connection) {
    let src = Connection::open_with_flags(template, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = src.prepare("select name, sql from sqlite_master where sql is not null order by rowid").unwrap();
    let rows: Vec<(String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    for (name, sql) in rows {
        if name.starts_with("releases_fts_") || name.starts_with("sqlite_") {
            continue;
        }
        conn.execute_batch(&sql).unwrap();
    }
}
