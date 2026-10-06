//! One time move of a database from before the shards (everything in
//! `atlas.db`) into the sharded compact layout of store.rs.
//!
//! 1. a new main database (`atlas.new.db`) with the group cursors
//! 2. every release into its group's shard, keeping its order: the new id is
//!    `old id * SHARDS + shard`
//! 3. every article, read in the order it is stored (one sequential pass),
//!    into its release's shard: one `files` row per file, one `segments` row
//!    per article
//! 4. a sample of releases checked: their NZBs, sizes, part counts and
//!    completeness have to come out the same as from the old database
//! 5. only then the swap: `atlas.db` becomes `atlas.old.db` (delete it once
//!    happy), `atlas.new.db` becomes `atlas.db`
//!
//! Shards hold their own writer thread each, soo they fill in parallel. If
//! anything fails the old database is left as it was and the next start
//! begins again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::compact::{checkpoint, rename_db};
use crate::db;
use crate::parser::Article;
use crate::search::{ArticleRow, ReleaseRow};
use crate::store::{self, SHARDS};

/// rows per transaction, and per buffer: 8 readers x 8 shards of buffers
/// plus the queues is what the conversion holds in memory
const BATCH: usize = 25_000;
/// releases whose NZBs are compared before the swap
const CHECK_SAMPLE: i64 = 2_000;
/// and the newest releases on top
const CHECK_TAIL: i64 = 200;
/// threads reading the old articles at once
const READERS: usize = 8;

/// The database at `main` is from before the shards.
pub fn needed(main: &Path) -> bool {
    main.exists() && db::open_at(main).and_then(|c| db::has_old_layout(&c)).unwrap_or(false)
}

/// Whether `--convert` has anything to do: a conversion `needed`, or a swap
/// cut short (atlas.db missing next to atlas.new.db or atlas.old.db) for
/// `run_alone` to finish or undo first.
pub fn to_do(main: &Path) -> bool {
    needed(main)
        || ((!main.exists() || is_empty_interloper(main))
            && (sibling(main, "atlas.new.db").exists() || sibling(main, "atlas.old.db").exists()))
}

/// An atlas.db with nothing in it: no bytes, or not a single table. What a
/// read made (a build that still made one on opening) in the moment a
/// conversion's swap had no main database. Looked at read only, and closed
/// again before this returns (it may be removed next, which Windows refuses
/// for a file still open).
pub fn is_empty_interloper(main: &Path) -> bool {
    match std::fs::metadata(main) {
        Ok(m) if m.len() == 0 => return true,
        Ok(m) if m.is_file() => {}
        _ => return false,
    }
    Connection::open_with_flags(main, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .and_then(|c| c.query_row("select not exists(select 1 from sqlite_master)", [], |r| r.get::<_, bool>(0)))
        .unwrap_or(false)
}

/// Removes an empty interloper (see `is_empty_interloper`) at `main`, with
/// its sidecars. Whoever made it may still have it open: then it stays, and
/// that is the error.
fn remove_interloper(main: &Path) -> std::io::Result<()> {
    std::fs::remove_file(main)?;
    remove_db(main);
    Ok(())
}

/// `rename_db(from, main)`, through an empty interloper at `main` (made in
/// the moment there was no main database): taken out, and tried again.
fn rename_into_place(from: &Path, main: &Path) -> std::io::Result<()> {
    match rename_db(from, main) {
        Err(e) if is_empty_interloper(main) => {
            remove_interloper(main).map_err(|_| e)?;
            rename_db(from, main)
        }
        done => done,
    }
}

fn sibling(main: &Path, name: &str) -> PathBuf {
    main.with_file_name(name)
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// A connection for filling a new shard fast: no journal (pages are written
/// once, straight into the file, instead of to the WAL and then again into
/// the file) and no syncing. A crash only means converting again from the
/// old database; the shards go back to WAL before the swap.
fn open_for_conversion(path: &Path) -> rusqlite::Result<Connection> {
    let conn = db::create_at(path)?;
    conn.query_row("pragma journal_mode = off", [], |_| Ok(()))?;
    conn.execute_batch(
        "pragma synchronous = off;
         pragma cache_size = -262144;
         pragma temp_store = memory;",
    )?;
    Ok(conn)
}

/// Mark a shard filled by `open_for_conversion` as completely copied (a
/// stopped conversion resumes after it, trusting the mark). Its pages were
/// written unsynced and unjournaled, soo the shard goes durable first (WAL,
/// full sync, every page folded into the file and synced) and the mark is
/// written after: it can never be on disk ahead of the pages it vouches for.
fn mark_copied(conn: &Connection) -> Result<()> {
    conn.execute_batch("pragma synchronous = full")?;
    conn.query_row("pragma journal_mode = wal", [], |_| Ok(()))?;
    conn.execute("insert or replace into meta (key, value) values ('copied', 0)", [])?;
    checkpoint(conn).context("making a copied shard durable")?;
    conn.execute("insert or replace into meta (key, value) values ('copied', 1)", [])?;
    Ok(())
}

/// `--convert`: `run` under the exclusive database lock for its whole run,
/// refused while the indexer, a write, a compaction or another conversion
/// holds it (two at once would write the same shards and `atlas.new.db`).
/// None when there was nothing to convert.
pub fn run_alone(main: &Path, progress: &dyn Fn(&str)) -> Result<Option<(i64, i64)>> {
    let _alone = crate::compact::Lock::take(main).map_err(|e| match e.downcast_ref::<crate::compact::Busy>() {
        Some(_) => anyhow!("the database is in use (indexing, a compaction or another conversion); try again later"),
        None => e,
    })?;
    if let Some(said) = recover_cut_swap(main)? {
        progress(&said);
    }
    if !needed(main) {
        return Ok(None);
    }
    crate::compact::refuse_unresolved_backups(main)?;
    run(main, progress).map(Some)
}

/// Convert, reporting progress through `progress`. Returns how many releases
/// and articles moved.
pub fn run(main: &Path, progress: &dyn Fn(&str)) -> Result<(i64, i64)> {
    let started = Instant::now();
    let old = Connection::open_with_flags(main, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("opening the old database")?;
    old.busy_timeout(std::time::Duration::from_secs(30))?;

    let new_main = sibling(main, "atlas.new.db");

    // a run that got through the copy and was stopped during the check
    // picks up there instead of copying everything again
    let source = Source::of(&old, main)?;
    let (max_id, shard_of_old, moved_releases, moved_articles, orphans) = if copy_finished(main, &source) {
        progress("converting the database: the copy finished earlier, checking it");
        let max_id: i64 = old.query_row("select coalesce(max(id), 0) from releases", [], |r| r.get(0))?;
        let mut shard_of_old = ShardOf::default();
        let mut stmt = old.prepare("select id, group_name from releases order by id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (id, group): (i64, Option<String>) = (r.get(0)?, r.get(1)?);
            shard_of_old.push(id, store::shard_of(group.as_deref().unwrap_or("")));
        }
        drop(rows);
        drop(stmt);
        let conn = Connection::open_in_memory()?;
        store::attach(&conn, main)?;
        let (releases, articles) = store::totals(&conn)?;
        (max_id, shard_of_old, releases, articles, 0)
    } else {
        copy(main, &old, &source, started, progress)?
    };

    // 4. the check
    progress("converting the database: checking NZBs against the old database");
    check(main, &old, max_id, &shard_of_old)?;

    // 5. the swap. every checkpoint has to fold its whole WAL in: one held
    // back (a reader, a writer) leaves committed pages only in the WAL
    {
        let conn = db::open_at(&new_main)?;
        store::set_next_seq(&conn, max_id + 1)?;
        checkpoint(&conn).context("folding the new main database's WAL in")?;
    }
    for path in store::shard_paths(main) {
        let conn = db::open_at(&path)?;
        conn.query_row("pragma journal_mode = wal", [], |_| Ok(()))?;
        checkpoint(&conn).with_context(|| format!("folding {}'s WAL in", path.display()))?;
    }
    {
        // from here atlas.new.db is whole: a swap cut short finishes at the next start
        let conn = db::open_at(&new_main)?;
        conn.execute("insert or replace into meta (key, value) values ('swap_ready', 1)", [])?;
        checkpoint(&conn).context("folding the new main database's WAL in")?;
    }
    drop(old);
    let backup = sibling(main, "atlas.old.db");
    remove_db(&backup);
    {
        // fold the old wal in soo atlas.old.db is whole on its own. nothing
        // else should have it open (the conversion runs alone), soo a short
        // wait: whoever does keeps the old database in place
        let conn = db::open_at(main)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        checkpoint(&conn).context("folding the old database's WAL in")?;
    }
    // its -wal and -shm go aside with it, whatever they still hold
    rename_db(main, &backup).context("moving the old database aside")?;
    if let Err(e) = rename_into_place(&new_main, main) {
        // the old one back, soo there is a main database to start from
        let _ = rename_into_place(&backup, main);
        return Err(e).context("putting the new main database in place");
    }

    progress(&format!(
        "converted {moved_releases} releases and {moved_articles} articles in {} ({orphans} articles without a release left out). \
         the old database is {} until you delete it",
        crate::dashboard::human_time(started.elapsed().as_secs() as i64),
        backup.display()
    ));
    Ok((moved_releases, moved_articles))
}

/// A swap that was cut short between moving the old database aside and
/// putting the new one in place leaves no main database. Finishes it when
/// `atlas.new.db` got through everything before the swap, else puts
/// `atlas.old.db` back (and the next start converts again). Run under the
/// exclusive lock. What it did, if anything.
pub fn recover_cut_swap(main: &Path) -> Result<Option<String>> {
    let (new_main, backup) = (sibling(main, "atlas.new.db"), sibling(main, "atlas.old.db"));
    if main.try_exists()? {
        // an empty one made in the swap's moment is in the way: out with it
        let cut = (new_main.try_exists()? && swap_ready(&new_main)) || backup.try_exists()?;
        if !(cut && is_empty_interloper(main)) {
            return Ok(None);
        }
        remove_interloper(main).with_context(|| format!("removing the empty {} in the way", main.display()))?;
    }
    if new_main.try_exists()? && swap_ready(&new_main) {
        rename_db(&new_main, main).context("putting the converted main database in place")?;
        return Ok(Some(format!(
            "a conversion was cut short putting {} in place; it's in place now (the old database is {})",
            main.display(),
            backup.display()
        )));
    }
    if backup.try_exists()? {
        // no conversion under way (it would have left atlas.new.db) yet shards
        // with releases: a conversion finished long ago and atlas.db went
        // missing since. the old database back would be converted again, and
        // that fresh copy would wipe the shards and all indexed since
        if !new_main.try_exists()? && shards_hold_releases(main) {
            bail!(
                "{} is missing, but the shards next to it hold releases from a finished conversion; \
                 not putting {} back (converting it again would wipe the shards). \
                 restore {} from a backup, or move the shards (atlas.s*.db) aside to start over from {}",
                main.display(),
                backup.display(),
                main.display(),
                backup.display()
            );
        }
        rename_db(&backup, main).context("putting the old database back")?;
        return Ok(Some(format!(
            "a conversion was cut short after moving {} aside; it's back in place",
            main.display()
        )));
    }
    Ok(None)
}

/// Any shard next to `main` has a release (read only: nothing is created)
fn shards_hold_releases(main: &Path) -> bool {
    store::shard_paths(main).iter().any(|p| {
        p.exists()
            && Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .and_then(|c| c.query_row("select exists(select 1 from releases)", [], |r| r.get::<_, bool>(0)))
                .unwrap_or(false)
    })
}

fn swap_ready(new_main: &Path) -> bool {
    db::open_at(new_main)
        .and_then(|c| c.query_row("select value from meta where key = 'swap_ready'", [], |r| r.get(0)).optional())
        .is_ok_and(|v: Option<i64>| v == Some(1))
}

/// Old release id -> the shard it went to, one entry per release (ids can
/// have gaps of any size, soo not a slot per id up to the highest). Filled
/// in id order, looked up by binary search.
#[derive(Default)]
struct ShardOf(Vec<(i64, u8)>);

impl ShardOf {
    fn push(&mut self, id: i64, shard: usize) {
        debug_assert!(self.0.last().is_none_or(|&(last, _)| last < id), "releases come in id order");
        self.0.push((id, shard as u8));
    }

    fn get(&self, id: i64) -> Option<usize> {
        self.0.binary_search_by_key(&id, |&(id, _)| id).ok().map(|i| self.0[i].1 as usize)
    }
}

type OldRelease = (i64, ReleaseFields);
type ReleaseFields = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
);

fn move_releases(
    main: &Path,
    old: &Connection,
    total: i64,
    shard_of_old: &mut ShardOf,
    progress: &dyn Fn(&str),
) -> Result<i64> {
    std::thread::scope(|s| -> Result<i64> {
        let mut senders: Vec<SyncSender<Vec<OldRelease>>> = Vec::new();
        let mut writers = Vec::new();
        for shard in 0..SHARDS {
            let (tx, rx): (SyncSender<Vec<OldRelease>>, Receiver<Vec<OldRelease>>) = sync_channel(2);
            senders.push(tx);
            let path = store::shard_path(main, shard);
            writers.push(s.spawn(move || -> Result<i64> {
                let mut conn = open_for_conversion(&path)?;
                let mut moved = 0i64;
                for batch in rx {
                    let tx = conn.transaction()?;
                    {
                        let mut insert = tx.prepare_cached(
                            "insert into releases (id, name, group_name, poster, posted_date, size, complete, parts,
                                file_total, display_name, is_obfuscated) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        )?;
                        for (old_id, (name, group, poster, date, size, complete, parts, file_total, display, obf)) in
                            &batch
                        {
                            insert.execute(params![
                                store::global_id(*old_id, shard),
                                name,
                                group,
                                poster,
                                date,
                                size,
                                complete,
                                parts,
                                file_total,
                                display,
                                obf.unwrap_or(0)
                            ])?;
                        }
                        store::add_totals(&tx, batch.len() as i64, 0)?;
                    }
                    tx.commit()?;
                    moved += batch.len() as i64;
                }
                Ok(moved)
            }));
        }

        let mut stmt = old.prepare(
            "select id, name, group_name, poster, posted_date, size, complete, parts, file_total, display_name,
                is_obfuscated from releases order by id",
        )?;
        let mut rows = stmt.query([])?;
        let mut buffers: Vec<Vec<OldRelease>> = vec![Vec::new(); SHARDS];
        let mut read = 0i64;
        let t = Instant::now();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let group: Option<String> = r.get(2)?;
            let shard = store::shard_of(group.as_deref().unwrap_or(""));
            shard_of_old.push(id, shard);
            buffers[shard].push((
                id,
                (
                    r.get(1)?,
                    group,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                ),
            ));
            if buffers[shard].len() >= BATCH {
                senders[shard]
                    .send(std::mem::take(&mut buffers[shard]))
                    .map_err(|_| anyhow!("a shard writer stopped"))?;
            }
            read += 1;
            if read % 2_000_000 == 0 {
                progress(&format!(
                    "converting the database: releases {:.0}% ({read} of {total}, {:.0}/s)",
                    read as f64 * 100.0 / total.max(1) as f64,
                    read as f64 / t.elapsed().as_secs_f64()
                ));
            }
        }
        for (shard, rest) in buffers.into_iter().enumerate() {
            if !rest.is_empty() {
                senders[shard].send(rest).map_err(|_| anyhow!("a shard writer stopped"))?;
            }
        }
        drop(senders);
        let mut moved = 0;
        for w in writers {
            moved += w.join().map_err(|_| anyhow!("a shard writer panicked"))??;
        }
        if moved != read {
            bail!("moved {moved} releases of {read}");
        }
        Ok(moved)
    })
}

/// (new release id, article)
type OldArticle = (i64, Article);

fn move_articles(
    main: &Path,
    total: i64,
    shard_of_old: &ShardOf,
    started: Instant,
    progress: &dyn Fn(&str),
) -> Result<(i64, i64)> {
    let (read, orphans, sent) = (AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0));
    std::thread::scope(|s| -> Result<(i64, i64)> {
        let mut senders: Vec<SyncSender<Vec<OldArticle>>> = Vec::new();
        let mut writers = Vec::new();
        for shard in 0..SHARDS {
            let (tx, rx): (SyncSender<Vec<OldArticle>>, Receiver<Vec<OldArticle>>) = sync_channel(2);
            senders.push(tx);
            let path = store::shard_path(main, shard);
            writers.push(s.spawn(move || -> Result<(i64, i64)> {
                let mut conn = open_for_conversion(&path)?;
                let mut domains = store::Domains::default();
                // articles arrive in the order the old database stored them: 90
                // groups interleaved, a file's parts spread over hours. inserted
                // straight into `segments` (keyed by file) they land all over the
                // table, every one a page rewrite. appended to a plain table first
                // and copied over sorted at the end, every write is sequential
                conn.execute_batch(
                    "create table segments_load (file_id INTEGER, local BLOB, domain INTEGER, part INTEGER, bytes INTEGER)",
                )?;
                let read_total = &mut 0i64;
                for batch in rx {
                    *read_total += batch.len() as i64;
                    let tx = conn.transaction()?;
                    // by (release, file), keeping the order they came in
                    let mut groups: Vec<(i64, &str, Vec<&Article>)> = Vec::new();
                    let mut index: std::collections::HashMap<(i64, &str), usize> = Default::default();
                    for (release_id, a) in &batch {
                        let key = (*release_id, a.filename.as_deref().unwrap_or(""));
                        match index.get(&key) {
                            Some(&i) => groups[i].2.push(a),
                            None => {
                                index.insert(key, groups.len());
                                groups.push((key.0, key.1, vec![a]));
                            }
                        }
                    }
                    {
                        let mut load = tx.prepare_cached(
                            "insert into segments_load (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)",
                        )?;
                        for (release_id, name, articles) in &groups {
                            let mut f = store::file(&tx, *release_id, name)?;
                            for a in articles {
                                let (local, domain) = domains.encode(&tx, &a.message_id)?;
                                load.execute(params![f.id, local, domain, a.part, a.bytes])?;
                                f.add(a);
                            }
                            store::put_file(&tx, &f)?;
                        }
                    }
                    tx.commit()?;
                    domains.committed();
                }

                // into place in key order: an append, sorted on disk
                conn.execute_batch("pragma temp_store = file")?;
                let added = conn.execute(
                    "insert or ignore into segments (file_id, local, domain, part, bytes)
                     select file_id, local, domain, part, bytes from segments_load order by file_id, local, domain",
                    [],
                )? as i64;
                conn.execute_batch("drop table segments_load")?;
                store::add_totals(&conn, 0, added)?;
                mark_copied(&conn)?;
                Ok((*read_total, added))
            }));
        }

        // several readers, each over its own range of article ids: the old table
        // isnt laid out in order on disk, soo one reader waits on one small read
        // at a time while several keep the disk busy
        let t = Instant::now();
        let per_reader = total / READERS as i64 + 1;
        let readers: Vec<_> = (0..READERS as i64)
            .map(|n| {
                let (senders, read, orphans, sent) = (senders.clone(), &read, &orphans, &sent);
                s.spawn(move || -> Result<()> {
                    let old = Connection::open_with_flags(
                        main,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?;
                    let mut stmt = old.prepare(
                        "select release_id, message_id, subject, filename, part, total_parts, bytes, file_total
                         from articles where id >= ? and id < ?",
                    )?;
                    let mut rows = stmt.query([n * per_reader, (n + 1) * per_reader])?;
                    let mut buffers: Vec<Vec<OldArticle>> = vec![Vec::new(); SHARDS];
                    while let Some(r) = rows.next()? {
                        read.fetch_add(1, Ordering::Relaxed);
                        let release_id: Option<i64> = r.get(0)?;
                        let message_id: Option<String> = r.get(1)?;
                        let shard = release_id.and_then(|id| shard_of_old.get(id));
                        let (Some(old_id), Some(message_id)) = (release_id, message_id.filter(|m| !m.is_empty()))
                        else {
                            orphans.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        let Some(shard) = shard else {
                            orphans.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        let article = Article {
                            message_id,
                            subject: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            filename: r.get(3)?,
                            part: r.get(4)?,
                            total_parts: r.get(5)?,
                            bytes: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                            file_total: r.get(7)?,
                            ..Article::default()
                        };
                        buffers[shard].push((store::global_id(old_id, shard), article));
                        if buffers[shard].len() >= BATCH {
                            sent.fetch_add(BATCH as i64, Ordering::Relaxed);
                            senders[shard]
                                .send(std::mem::take(&mut buffers[shard]))
                                .map_err(|_| anyhow!("a shard writer stopped"))?;
                        }
                    }
                    for (shard, rest) in buffers.into_iter().enumerate() {
                        if !rest.is_empty() {
                            sent.fetch_add(rest.len() as i64, Ordering::Relaxed);
                            senders[shard].send(rest).map_err(|_| anyhow!("a shard writer stopped"))?;
                        }
                    }
                    Ok(())
                })
            })
            .collect();

        // progress every 10s until the readers are done
        while readers.iter().any(|r| !r.is_finished()) {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if !t.elapsed().as_secs().is_multiple_of(10) {
                continue;
            }
            let done = read.load(Ordering::Relaxed);
            let rate = done as f64 / t.elapsed().as_secs_f64().max(1.0);
            progress(&format!(
                "converting the database: articles {:.1}% ({done} of about {total}, {:.0}/s, about {} left, {} so far)",
                done as f64 * 100.0 / total.max(1) as f64,
                rate,
                crate::dashboard::human_time(((total - done).max(0) as f64 / rate.max(1.0)) as i64),
                crate::dashboard::human_time(started.elapsed().as_secs() as i64)
            ));
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
        for r in readers {
            r.join().map_err(|_| anyhow!("an article reader panicked"))??;
        }
        progress(&format!(
            "converting the database: sorting the articles into place ({} so far)",
            crate::dashboard::human_time(started.elapsed().as_secs() as i64)
        ));
        let (orphans, sent) = (orphans.load(Ordering::Relaxed), sent.load(Ordering::Relaxed));
        drop(senders);
        let (mut got, mut added) = (0, 0);
        for w in writers {
            let (r, a) = w.join().map_err(|_| anyhow!("a shard writer panicked"))??;
            got += r;
            added += a;
        }
        if got != sent {
            bail!("the shard writers got {got} articles of {sent}");
        }
        // the old database had no duplicates per release, soo every one is new
        if added != sent {
            bail!("{} of {sent} articles came out as duplicates", sent - added);
        }
        Ok((added, orphans))
    })
}

/// Compare `CHECK_SAMPLE` releases, spread over the whole database, and the
/// newest `CHECK_TAIL` between the old database and the shards.
fn check(main: &Path, old: &Connection, max_id: i64, shard_of_old: &ShardOf) -> Result<()> {
    let new = Connection::open_in_memory()?;
    store::attach(&new, main)?;

    let step = (max_id / CHECK_SAMPLE).max(1);
    let mut old_release = old.prepare(
        "select id, name, group_name, poster, posted_date, size, complete, parts from releases where id >= ? limit 1",
    )?;
    let mut old_articles = old.prepare(
        "select articles.message_id, articles.filename, articles.part, articles.total_parts, articles.bytes,
            articles.subject, releases.poster, releases.posted_date
         from articles join releases on articles.release_id = releases.id
         where articles.release_id = ? order by articles.filename, articles.part",
    )?;

    let row = |r: &rusqlite::Row| -> rusqlite::Result<ReleaseRow> {
        Ok(ReleaseRow {
            id: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            group_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            poster: r.get(3)?,
            posted_date: r.get(4)?,
            size: r.get(5)?,
            complete: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
            parts: r.get(7)?,
        })
    };

    // spread over the ids, and the newest ones: the last written are the
    // likeliest to be missing from a copy
    let tail: Vec<i64> = old
        .prepare("select id from releases order by id desc limit ?")?
        .query_map([CHECK_TAIL], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let starts = (0..CHECK_SAMPLE).map(|i| i * step).chain(tail);

    let (mut checked, mut bad) = (0, Vec::new());
    let mut seen = std::collections::HashSet::new();
    for start in starts {
        let Ok(was) = old_release.query_row([start], row) else { continue };
        if !seen.insert(was.id) {
            continue;
        }
        let shard = shard_of_old.get(was.id).ok_or_else(|| anyhow!("release {} has no shard", was.id))?;
        let id = store::global_id(was.id, shard);
        let now = new.query_row(
            &format!(
                "select id, name, group_name, poster, posted_date, size, complete, parts from s{shard}.releases where id = ?"
            ),
            [id],
            row,
        )?;

        let articles_before: Vec<ArticleRow> = old_articles
            .query_map([was.id], |r| {
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
            })?
            .collect::<rusqlite::Result<_>>()?;
        // the old query leaves ties in part number in no set order; the shards
        // order them by message-id. sort the same way to compare
        let mut articles_before = articles_before;
        articles_before.sort_by(|a, b| (&a.filename, a.part, &a.message_id).cmp(&(&b.filename, b.part, &b.message_id)));
        let articles_after = store::articles(&new, id)?;
        if articles_before.is_empty() && articles_after.is_empty() {
            checked += 1;
            continue;
        }

        let nzb_before = crate::nzb::render_nzb(&was, &articles_before);
        let nzb_after = crate::nzb::render_nzb(&now, &articles_after);
        let stats_before = (was.size, was.complete, was.parts);
        let stats_after = (now.size, now.complete, now.parts);
        if nzb_before != nzb_after || stats_before != stats_after {
            bad.push(was.id);
        }
        checked += 1;
    }

    if !bad.is_empty() {
        bail!(
            "{} of {checked} sampled releases came out different (old ids {:?}), nothing was changed",
            bad.len(),
            &bad[..bad.len().min(10)]
        );
    }
    Ok(())
}

/// Steps 1 to 3. Returns (max old release id, old id -> shard, releases moved,
/// articles moved, articles without a release).
fn copy(
    main: &Path,
    old: &Connection,
    source: &Source,
    started: Instant,
    progress: &dyn Fn(&str),
) -> Result<(i64, ShardOf, i64, i64, i64)> {
    let new_main = sibling(main, "atlas.new.db");
    // 1. the new main database with the cursors
    remove_db(&new_main);
    {
        let conn = db::create_at(&new_main)?;
        conn.query_row("pragma journal_mode = wal", [], |_| Ok(()))?;
        conn.execute_batch("create table groups(name TEXT PRIMARY KEY, live_cursor INTEGER, backfill_cursor INTEGER)")?;
        let cols: Vec<String> = old
            .prepare("select name from pragma_table_info('groups')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for col in cols.iter().filter(|c| !["name", "live_cursor", "backfill_cursor"].contains(&c.as_str())) {
            conn.execute(&format!("alter table groups add column {col} INTEGER"), [])?;
        }
        store::create_main(&conn)?;
        conn.execute("attach database ? as old", [main.to_string_lossy()])?;
        let list = cols.join(", ");
        conn.execute(&format!("insert into groups ({list}) select {list} from old.groups"), [])?;
        conn.execute("detach database old", [])?;
        source.save(&conn)?;
    }

    // fresh shards (a failed run leaves its own behind). releases go in without
    // their indexes and search index: building those once afterwards, sorted,
    // is far faster than keeping them up to date row by row
    for path in store::shard_paths(main) {
        remove_db(&path);
        store::create_shard(&path)?;
        db::open_at(&path)?.execute_batch(
            "drop trigger releases_ai; drop trigger releases_ad; drop trigger releases_au;
             drop index idx_release_unique; drop index idx_release_group;",
        )?;
    }

    // 2. releases
    let max_id: i64 = old.query_row("select coalesce(max(id), 0) from releases", [], |r| r.get(0))?;
    let total_releases: i64 = old.query_row("select count(*) from releases", [], |r| r.get(0))?;
    progress(&format!("converting the database: {} releases", total_releases));
    let mut shard_of_old = ShardOf::default();
    let moved_releases = move_releases(main, old, total_releases, &mut shard_of_old, progress)?;

    progress("converting the database: building the release indexes and search index");
    std::thread::scope(|s| -> Result<()> {
        let builders: Vec<_> = store::shard_paths(main)
            .into_iter()
            .map(|path| {
                s.spawn(move || -> Result<()> {
                    let conn = open_for_conversion(&path)?;
                    conn.execute_batch("insert into releases_fts(releases_fts) values('rebuild')")?;
                    // the indexes and triggers again
                    drop(conn);
                    store::create_shard(&path)?;
                    Ok(())
                })
            })
            .collect();
        for b in builders {
            b.join().map_err(|_| anyhow!("an index builder panicked"))??;
        }
        Ok(())
    })?;

    // 3. articles
    let total_articles: i64 = old.query_row("select coalesce(max(id), 0) from articles", [], |r| r.get(0))?;
    let (moved_articles, orphans) = move_articles(main, total_articles, &shard_of_old, started, progress)?;

    // the copy's own connections to the old database have closed (the last one
    // folds its -wal in), soo its files are as a stopped run will find them
    let conn = db::open_at(&new_main)?;
    Source::of(old, main)?.save(&conn)?;

    Ok((max_id, shard_of_old, moved_releases, moved_articles, orphans))
}

/// What the old database was when a copy started: its release and article
/// counts and highest ids, a digest of its `groups` rows (the cursors: scans
/// of empty ranges move them without adding anything), an identity written
/// into its own meta on the first attempt, and the size and modification time
/// of its file and its -wal (a change that keeps every count, an edit to one
/// release, still moves those). A copy is only picked up again from the same
/// old database; one written to or put back from another backup since differs.
#[derive(Debug, PartialEq)]
struct Source(Vec<(&'static str, String)>);

impl Source {
    fn of(old: &Connection, main: &Path) -> Result<Source> {
        let (releases, max_release): (i64, i64) =
            old.query_row("select count(*), coalesce(max(id), 0) from releases", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let (articles, max_article): (i64, i64) =
            old.query_row("select count(*), coalesce(max(id), 0) from articles", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        // the identity first: writing it is a change to the files stat'd below
        let uuid = source_identity(main)?;
        let stat = |path: PathBuf| -> String {
            // an empty -wal is as good as none: opening a database touches it
            let Ok(m) = std::fs::metadata(&path) else { return "none".into() };
            if m.len() == 0 && path.to_string_lossy().ends_with("-wal") {
                return "none".into();
            }
            let nanos =
                m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos());
            format!("{}:{}", m.len(), nanos.unwrap_or(0))
        };
        let wal = PathBuf::from(format!("{}-wal", main.display()));
        Ok(Source(vec![
            ("source_releases", releases.to_string()),
            ("source_max_release", max_release.to_string()),
            ("source_articles", articles.to_string()),
            ("source_max_article", max_article.to_string()),
            ("source_groups", groups_digest(old)?.to_string()),
            ("source_uuid", uuid),
            ("source_db_file", stat(main.to_path_buf())),
            ("source_wal_file", stat(wal)),
        ]))
    }

    /// into the new main database's meta, as the copy starts
    fn save(&self, conn: &Connection) -> Result<()> {
        for (key, value) in &self.0 {
            conn.execute("insert or replace into meta (key, value) values (?, ?)", params![key, value])?;
        }
        Ok(())
    }

    fn saved(conn: &Connection, like: &Source) -> Result<Option<Source>> {
        let mut values = Vec::new();
        for (key, _) in &like.0 {
            let Some(v) =
                conn.query_row("select cast(value as text) from meta where key = ?", [key], |r| r.get(0)).optional()?
            else {
                return Ok(None);
            };
            values.push((*key, v));
        }
        Ok(Some(Source(values)))
    }
}

/// The old database's random identity, written into its meta the first time
/// a conversion is tried on it and read back after that. Its -wal is folded
/// in here too: whichever connection closes last does that anyway, and it
/// would change the files a stopped run is checked against, soo it's done
/// first, before they are looked at.
fn source_identity(main: &Path) -> Result<String> {
    let conn = Connection::open(main).context("opening the old database to identify it")?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    let read = |conn: &Connection| -> Option<String> {
        conn.query_row("select cast(value as text) from meta where key = 'source_uuid'", [], |r| r.get(0)).ok()
    };
    let uuid = match read(&conn) {
        Some(uuid) => uuid,
        None => {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|e| anyhow!("no random bytes for the source identity: {e}"))?;
            let uuid: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            conn.execute_batch("create table if not exists meta (key text primary key, value)")?;
            conn.execute("insert or ignore into meta (key, value) values ('source_uuid', ?)", [&uuid])?;
            read(&conn).ok_or_else(|| anyhow!("the source identity was not written"))?
        }
    };
    // best effort, never waited for: a reader holding it back only means a
    // stopped run is copied again
    conn.busy_timeout(std::time::Duration::ZERO)?;
    let _ = conn.query_row("pragma wal_checkpoint(truncate)", [], |_| Ok(()));
    Ok(uuid)
}

/// FNV-1a over every column of every `groups` row in name order, stable
/// across builds (it's kept in the new database's meta between runs)
fn groups_digest(old: &Connection) -> Result<i64> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash = (hash ^ *b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    };
    let mut stmt = old.prepare("select * from groups order by name")?;
    let columns = stmt.column_count();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        for i in 0..columns {
            // the type too, soo null and '' and 0 differ
            match r.get_ref(i)? {
                ValueRef::Null => eat(b"n"),
                ValueRef::Integer(v) => {
                    eat(b"i");
                    eat(&v.to_le_bytes());
                }
                ValueRef::Real(v) => {
                    eat(b"r");
                    eat(&v.to_le_bytes());
                }
                ValueRef::Text(v) | ValueRef::Blob(v) => {
                    eat(b"t");
                    eat(&(v.len() as u64).to_le_bytes());
                    eat(v);
                }
            }
        }
    }
    Ok(hash as i64)
}

/// The shards are complete from an earlier run, copied from the old database
/// as it is now (`source`): every shard writer marks its shard once its
/// articles are sorted into place.
fn copy_finished(main: &Path, source: &Source) -> bool {
    let new_main = sibling(main, "atlas.new.db");
    new_main.exists()
        && db::open_at(&new_main).ok().and_then(|c| Source::saved(&c, source).ok().flatten()).as_ref() == Some(source)
        && store::shard_paths(main).iter().all(|p| {
            p.exists()
                && db::open_at(p)
                    .and_then(|c| {
                        let copied: Option<i64> =
                            c.query_row("select value from meta where key = 'copied'", [], |r| r.get(0)).optional()?;
                        Ok(copied == Some(1))
                    })
                    .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copied_shard_is_durable_before_it_is_marked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shard.db");
        let conn = open_for_conversion(&path).unwrap();
        conn.execute_batch("create table meta(key text primary key, value); create table t(x)").unwrap();
        conn.execute("insert into t values (1)", []).unwrap();
        let mode = |c: &Connection| c.query_row("pragma journal_mode", [], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!(mode(&conn), "off");
        mark_copied(&conn).unwrap();
        // fully synced, journaled, and the data is in the file itself
        assert_eq!(mode(&conn), "wal");
        assert_eq!(conn.query_row("pragma synchronous", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
        let mark: i64 = conn.query_row("select value from meta where key = 'copied'", [], |r| r.get(0)).unwrap();
        assert_eq!(mark, 1);
        // the file alone (without the WAL, as after a crash that lost the
        // unsynced tail) has the rows, and not yet the mark written last
        let alone = dir.path().join("alone.db");
        std::fs::copy(&path, &alone).unwrap();
        let alone = Connection::open(&alone).unwrap();
        assert_eq!(alone.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let marked: Option<i64> =
            alone.query_row("select value from meta where key = 'copied'", [], |r| r.get(0)).optional().unwrap();
        assert_ne!(marked, Some(1));
    }
}
