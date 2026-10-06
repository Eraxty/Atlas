//! `atlas --compact`, with the indexer stopped: rewrite every shard into a
//! fresh file.
//!
//! - message-ids whose domain fewer than `SHARED_AFTER` articles share are
//!   stored whole, and those domains dropped: some posting tools make up a
//!   new domain per article, which left tens of millions of single use rows
//! - the other message-ids packed the current way (locals saved before the
//!   packing by shape are re-encoded), in blobs too
//! - every file that is due sealed into a blob as it's copied
//! - the copy has no free space in it (the conversion left the space its
//!   staging table used)
//!
//! Each shard's copy is checked (row counts, and a sample of NZBs against the
//! original) before it replaces the original. The originals stay next to it
//! as `atlas.sN.precompact.db` until every shard has had its turn, then they
//! go. A shard that fails keeps its original in place and the others carry
//! on; `run` then returns an error naming each failed shard. A swap cut short
//! between moving the original aside and the copy in (a crash) is undone by
//! the next start or compaction, see `recover_cut_swaps`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::blob::{self, Seg};
use crate::db;
use crate::store::{self, SHARDS, SHARED_AFTER};

/// shards rewritten at once
const PARALLEL: usize = 4;
/// releases whose NZBs are compared per shard
const CHECK_SAMPLE: i64 = 300;
/// rows per transaction
const BATCH: usize = 500_000;

/// `atlas.s3.db` -> `atlas.s3.{suffix}.db`
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{stem}.{suffix}.db"))
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Fold the WAL of `conn`'s database into it, all of it: a checkpoint held
/// back (a reader, a writer) leaves committed pages only in the WAL, soo it
/// is an error rather than taken for done.
pub(crate) fn checkpoint(conn: &Connection) -> Result<()> {
    let (busy, log, done): (i64, i64, i64) =
        conn.query_row("pragma wal_checkpoint(truncate)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if busy != 0 || log != done {
        bail!("the WAL checkpoint didnt complete ({done} of {log} pages, busy {busy})");
    }
    Ok(())
}

/// Rename the database `from` to `to`, its -wal and -shm with it (those
/// there are): the WAL can hold committed pages of it.
pub(crate) fn rename_db(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)?;
    for suffix in ["-wal", "-shm"] {
        let (a, b) = (format!("{}{suffix}", from.display()), format!("{}{suffix}", to.display()));
        if Path::new(&a).exists() {
            std::fs::rename(a, b)?;
        }
    }
    Ok(())
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `atlas.compacting` next to `main`: a compaction holds it locked
/// exclusively for its whole run, the indexer holds it shared while it
/// indexes, and a write from outside the indexer for its own. The OS lets go
/// of a lock when its process ends, soo a crash leaves nothing to clean up.
/// The file itself stays (removing a file someone may be locking races them).
pub fn lock_path(main: &Path) -> PathBuf {
    main.with_extension("compacting")
}

fn open_lock(main: &Path) -> Result<std::fs::File> {
    let path = lock_path(main);
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// A compaction couldnt start: another one, the indexer or a write from
/// outside it holds the lock.
#[derive(Debug)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the indexer, another compaction or a database write holds the lock")
    }
}

impl std::error::Error for Busy {}

/// The indexer while it indexes, and writes from outside it (AI search saves,
/// purging) for their whole run, hold this: they would land in the original
/// after a compaction took its copy, and be lost when the copy replaces it.
/// Refused while a compaction runs; a compaction cant start while one is held.
#[must_use = "the write is only safe while the guard is held"]
pub struct WriteGuard(std::fs::File);

/// Hold off compaction while writing the shards, see `WriteGuard`. Also
/// refused while a shard and its `.precompact.db` are both there and which
/// one is whole isnt known (`unresolved_backups`): a write would go into
/// whichever is wrong. Whoever holds this is about to write; setting up the
/// database and reading go through `try_hold_off_compaction` and aren't held
/// back.
pub fn hold_off_compaction(main: &Path) -> Result<WriteGuard> {
    let held =
        try_hold_off_compaction(main)?.ok_or_else(|| anyhow!("the database is being compacted; try again later"))?;
    refuse_unresolved_backups(main)?;
    Ok(held)
}

/// `hold_off_compaction`, None while a compaction runs.
pub fn try_hold_off_compaction(main: &Path) -> Result<Option<WriteGuard>> {
    let file = open_lock(main)?;
    match file.try_lock_shared() {
        Ok(()) => Ok(Some(WriteGuard(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("locking the database for a write"),
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        // closing the file lets go too, but Windows may take a while to
        let _ = self.0.unlock();
    }
}

/// The lock of a running compaction, let go when dropped (done, failed or stopped).
pub(crate) struct Lock(std::fs::File);

impl Lock {
    pub(crate) fn take(main: &Path) -> Result<Lock> {
        let file = open_lock(main)?;
        match file.try_lock() {
            Ok(()) => Ok(Lock(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(Busy.into()),
            Err(std::fs::TryLockError::Error(e)) => Err(e).context("locking the database for compacting"),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Shards with a `.precompact.db` next to them, as (shard, backup). With the
/// lock held a backup next to a shard is not a compaction at work: it was
/// cut short, or an older atlas made an empty shard beside it.
pub fn unresolved_backups(main: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut found = Vec::new();
    for path in store::shard_paths(main) {
        let backup = with_suffix(&path, "precompact");
        if backup.try_exists()? && path.try_exists()? {
            found.push((path, backup));
        }
    }
    Ok(found)
}

/// What to tell the user about one `unresolved_backups` pair: both files, and
/// how to go on with either.
fn unresolved_message(shard: &Path, backup: &Path) -> String {
    let bytes = |p: &Path| crate::ui::fmt_size(Some(size(p) as i64));
    format!(
        "{shard} ({}) and {backup} ({}) both exist, and which one is whole isnt known: a compaction was cut short, \
         or an older atlas made an empty shard next to the backup. Not writing the shards till this is resolved. \
         Stop atlas, then either keep {shard} and delete {backup}, or keep {backup}: delete {shard} (and its -wal and \
         -shm files) and rename {backup} to {shard}. The bigger one usually has the releases",
        bytes(shard),
        bytes(backup),
        shard = shard.display(),
        backup = backup.display(),
    )
}

/// Err for the first of `unresolved_backups`, see `unresolved_message`.
pub fn refuse_unresolved_backups(main: &Path) -> Result<()> {
    match unresolved_backups(main)?.first() {
        Some((shard, backup)) => bail!("{}", unresolved_message(shard, backup)),
        None => Ok(()),
    }
}

/// Put back the original of every shard whose swap was cut short (a crash,
/// or its copy failing to move in): the original moved aside as
/// `.precompact.db` and nothing in its place. Nothing writes a shard while
/// it's compacted, soo the original is whole: it goes back, and the copy
/// goes. A backup next to a shard that is in place is kept (the copy may have
/// gone in, or a shard made empty in its place by an older atlas; which one
/// is whole isnt known) and said so: the indexer and saves refuse till the
/// user removes one (`hold_off_compaction`). Returns a line for each, to show.
///
/// Only with the lock held (either kind): a running compaction has a shard
/// moved aside on purpose for a moment.
pub fn recover_cut_swaps(main: &Path) -> Result<Vec<String>> {
    let mut said = Vec::new();
    for path in store::shard_paths(main) {
        let original = with_suffix(&path, "precompact");
        if !original.try_exists()? {
            continue;
        }
        if path.try_exists()? {
            said.push(unresolved_message(&path, &original));
            continue;
        }
        rename_db(&original, &path)
            .with_context(|| format!("putting {} back as {}", original.display(), path.display()))?;
        remove_db(&with_suffix(&path, "compact"));
        said.push(format!("{} was moved aside by a compaction that was cut short; it's back in place", path.display()));
    }
    Ok(said)
}

/// What compacting one shard did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Shrunk {
    pub before: u64,
    pub after: u64,
    pub domains_kept: i64,
    pub domains_dropped: i64,
}

/// the rows of a copying loop between looks at the stop flag
const STOP_EVERY: usize = 65_536;

/// Stop with an error once `stop` is set. Compacting only ever leaves the
/// original shard alone until the swap, soo stopping anywhere before it loses
/// nothing: the half made copy goes and the shard stays as it was.
fn halt(stop: &AtomicBool, shard: usize) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        bail!("shard {shard}: stopped, the original was kept");
    }
    Ok(())
}

/// test hook: the stop flags (by address) to set the first time the held
/// back stream looks at them, with how many times it has
#[cfg(test)]
static HELD_CHECKS: std::sync::Mutex<Vec<(usize, usize, usize)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
struct HeldCheck(usize);

/// registers a stop flag with the hook above until the guard drops, soo a
/// flag's address freed and reused by another test never finds it still there
#[cfg(test)]
impl HeldCheck {
    fn on(stop: &Arc<AtomicBool>) -> HeldCheck {
        let at = Arc::as_ptr(stop) as usize;
        HELD_CHECKS.lock().unwrap().push((at, 0, 0));
        HeldCheck(at)
    }

    fn looks(&self) -> usize {
        HELD_CHECKS.lock().unwrap().iter().find(|e| e.0 == self.0).unwrap().1
    }

    /// the rows the loop had looked at when it last looked
    fn examined(&self) -> usize {
        HELD_CHECKS.lock().unwrap().iter().find(|e| e.0 == self.0).unwrap().2
    }
}

#[cfg(test)]
impl Drop for HeldCheck {
    fn drop(&mut self) {
        HELD_CHECKS.lock().unwrap().retain(|e| e.0 != self.0);
    }
}

#[cfg(test)]
fn held_check(stop: &AtomicBool, examined: usize) {
    let at = stop as *const AtomicBool as usize;
    if let Some(e) = HELD_CHECKS.lock().unwrap().iter_mut().find(|e| e.0 == at) {
        e.1 += 1;
        e.2 = examined;
        stop.store(true, Ordering::Relaxed);
    }
}

/// Have SQLite give up long statements (a bulk insert, an index build) once
/// `stop` is set: they fail with an interrupt, like any other error.
fn interruptible(conn: &Connection, stop: &Arc<AtomicBool>) -> Result<()> {
    let stop = stop.clone();
    conn.progress_handler(1000, Some(move || stop.load(Ordering::Relaxed)))?;
    Ok(())
}

/// Compact every shard of `main`. Returns the total before and after. Once
/// `stop` is set, shards not done are left as they were (and an error says so).
pub fn run(main: &Path, progress: &(dyn Fn(&str) + Sync), stop: &Arc<AtomicBool>) -> Result<Shrunk> {
    let _lock = Lock::take(main)?;
    // a conversion's swap cut short is finished (or undone) like at a start;
    // shards with no main database at all are refused: the main database
    // the next start made would hand out their ids again
    if let Some(said) = crate::convert::recover_cut_swap(main)? {
        progress(&said);
    }
    if !main.try_exists()? && store::exists(main) {
        bail!("{} is missing but its shards are there; put it back from a backup first", main.display());
    }
    for line in recover_cut_swaps(main)? {
        progress(&line);
    }
    if !store::exists(main) {
        bail!("{} has no shards (convert it first)", main.display());
    }
    let started = Instant::now();
    let mut total = Shrunk::default();
    let mut failed: Vec<(usize, String)> = Vec::new();
    // every shard gets its turn: one that fails keeps its original and doesnt
    // stop the others
    for chunk in (0..SHARDS).collect::<Vec<_>>().chunks(PARALLEL) {
        if stop.load(Ordering::Relaxed) {
            failed.extend(chunk.iter().map(|&shard| (shard, format!("shard {shard}: stopped"))));
            continue;
        }
        let results: Vec<Result<Shrunk>> = std::thread::scope(|s| {
            let jobs: Vec<_> =
                chunk.iter().map(|&shard| s.spawn(move || compact_shard(main, shard, progress, stop))).collect();
            jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err(anyhow!("compacting a shard panicked")))).collect()
        });
        for (&shard, r) in chunk.iter().zip(results) {
            match r {
                Ok(r) => {
                    total.before += r.before;
                    total.after += r.after;
                    total.domains_kept += r.domains_kept;
                    total.domains_dropped += r.domains_dropped;
                }
                Err(e) => failed.push((shard, format!("shard {shard}: {e:#}"))),
            }
        }
    }
    // the originals of the shards that were swapped can go. a failed shard
    // keeps its original in place, and loses its half made copy; one whose
    // copy didnt move in gets its original back
    for shard in 0..SHARDS {
        let path = store::shard_path(main, shard);
        if !failed.iter().any(|(f, _)| *f == shard) {
            remove_db(&with_suffix(&path, "precompact"));
        } else if path.exists() {
            remove_db(&with_suffix(&path, "compact"));
        }
    }
    if failed.iter().any(|(shard, _)| !store::shard_path(main, *shard).exists()) {
        for line in recover_cut_swaps(main)? {
            progress(&line);
        }
    }
    if stop.load(Ordering::Relaxed) {
        bail!("stopped, {} of {SHARDS} shards were left as they were", failed.len());
    }
    if !failed.is_empty() {
        let each: Vec<String> = failed.into_iter().map(|(_, e)| e).collect();
        bail!("{} of {SHARDS} shards were not compacted, their originals were kept: {}", each.len(), each.join("; "));
    }
    progress(&format!(
        "compacted {:.1}GB into {:.1}GB in {}: kept {} shared domains, dropped {} single use ones",
        total.before as f64 / 1e9,
        total.after as f64 / 1e9,
        crate::dashboard::human_time(started.elapsed().as_secs() as i64),
        total.domains_kept,
        total.domains_dropped
    ));
    Ok(total)
}

fn compact_shard(
    main: &Path,
    shard: usize,
    progress: &(dyn Fn(&str) + Sync),
    stop: &Arc<AtomicBool>,
) -> Result<Shrunk> {
    let path = store::shard_path(main, shard);
    let copy = with_suffix(&path, "compact");
    let say = |msg: &str| progress(&format!("compacting shard {shard}: {msg}"));
    halt(stop, shard)?;
    // a backup left by a compaction cut short isnt known to be redundant, and
    // the swap would need its name
    let backup = with_suffix(&path, "precompact");
    if backup.try_exists()? {
        bail!("{} is left from an earlier compaction; the original was kept", backup.display());
    }

    // fold the WAL in soo the original is whole on its own
    {
        let conn = db::open_at(&path)?;
        checkpoint(&conn).context("folding the original's WAL in")?;
        // a shard from before sealing gets its (empty) seal columns and tables
        store::migrate_shard(&conn)?;
    }
    let before = size(&path);

    // a fresh shard, filled without its indexes and triggers (built at the end)
    remove_db(&copy);
    store::create_shard(&copy)?;
    let mut conn = db::open_at(&copy)?;
    interruptible(&conn, stop)?;
    conn.query_row("pragma journal_mode = off", [], |_| Ok(()))?;
    conn.execute_batch(
        "pragma synchronous = off;
         pragma cache_size = -262144;
         pragma temp_store = memory;
         drop trigger releases_ai; drop trigger releases_ad; drop trigger releases_au;
         drop index idx_release_unique; drop index idx_release_group; drop index files_key;",
    )?;
    conn.execute("attach database ? as old", [path.to_string_lossy()])?;

    // how many articles use each domain, as rows and inside sealed blobs
    say("counting domains");
    let uses = DomainUses::count(&path, shard, stop)?;
    let shared = |d: i64| -> Result<bool> { Ok(d != 0 && uses.shared(d)?) };

    halt(stop, shard)?;
    say("copying releases");
    conn.execute_batch(
        "insert into releases select * from old.releases order by id;
         insert or replace into meta select * from old.meta;",
    )?;

    // the shared domains, keeping their ids
    let (mut kept, mut dropped) = (0i64, 0i64);
    {
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare("insert into domains (id, suffix) values (?, ?)")?;
            let mut stmt = tx.prepare("select id, suffix from old.domains order by id")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let (id, suffix): (i64, String) = (r.get(0)?, r.get(1)?);
                if shared(id)? {
                    insert.execute(params![id, suffix])?;
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        tx.commit()?;
    }

    // the files and their articles, in id order: those of dropped domains
    // stored whole, the rest packed the current way, and files that are due
    // sealed into one blob on the way (sealing afterwards would leave the
    // copy full of the deleted rows' free pages)
    halt(stop, shard)?;
    say(&format!("copying files and articles ({kept} shared domains, dropping {dropped}), sealing those due"));
    let now = chrono::Utc::now().timestamp();
    // rows dropped as copies of what their file's blob already has
    let (mut copied, mut sealed, mut copies) = (0i64, 0i64, 0i64);
    // files that lost copies: their parts are worked out again once copied
    let mut recount = Vec::new();
    {
        let open = || Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY);
        let (files_read, rows_read) = (open()?, open()?);
        let mut suffix_of = files_read.prepare("select suffix from domains where id = ?")?;
        let mut held_of = files_read
            .prepare("select message_id, subject, part, total_parts, file_total from held_back where file_id = ?")?;
        let mut files_stmt = files_read.prepare(&format!("select {FILE_COLUMNS} from files order by id"))?;
        let mut files = files_stmt.query([])?;
        let mut rows_stmt = rows_read.prepare(
            "select s.file_id, s.local, s.domain, s.part, s.bytes, d.suffix
             from segments s left join domains d on d.id = s.domain order by s.file_id",
        )?;
        let mut rows = rows_stmt.query([])?;
        let mut pending = next_row(shard, &mut rows, &shared)?;

        let mut tx = conn.transaction()?;
        let mut in_tx = 0;
        loop {
            halt(stop, shard)?;
            let file = files.next()?;
            let id: Option<i64> = file.map(|f| f.get(0)).transpose()?;
            // rows whose file isnt there (purged, or past the last file) stay rows
            while let Some(row) = pending.take_if(|r| id.is_none_or(|id| r.file_id < id)) {
                copied += insert_row(&tx, &row)?;
                in_tx += 1;
                if (copied as usize).is_multiple_of(STOP_EVERY) {
                    halt(stop, shard)?;
                }
                pending = next_row(shard, &mut rows, &shared)?;
            }
            let (Some(f), Some(id)) = (file, id) else { break };
            // at most one past the most a blob takes; the rest of a bigger file
            // is streamed below, never held
            let mut file_rows = Vec::new();
            while file_rows.len() as i64 <= store::SEAL_MAX_SEGMENTS
                && let Some(row) = pending.take_if(|r| r.file_id == id)
            {
                file_rows.push(row);
                pending = next_row(shard, &mut rows, &shared)?;
            }

            // its blob, with every message-id stored the way its rows are
            let old_blob: Option<Vec<u8>> = f.get(10)?;
            let mut segs = Vec::new();
            // old blob segments whose rewritten message-id is past what a blob
            // takes (a dropped domain's suffix made whole again) become rows
            let mut spilled = Vec::new();
            for seg in old_blob.as_deref().map(|b| unseal(shard, id, b)).transpose()?.unwrap_or_default() {
                let suffix: Option<String> = if seg.domain != 0 && !shared(seg.domain)? {
                    suffix_of.query_row([seg.domain], |r| r.get(0)).optional()?
                } else {
                    None
                };
                let (local, domain) =
                    rewrite(shard, id, seg.local, seg.domain, shared(seg.domain)?, suffix.as_deref())?;
                if local.len() > blob::MAX_LOCAL {
                    spilled.push(Row { file_id: id, local, domain, part: seg.part, bytes: Some(seg.bytes) });
                } else {
                    segs.push(Seg { local, domain, ..seg });
                }
            }
            // what rows a save couldnt tell from copies said (see `store::seal_rows`):
            // kept for the rows that are new, dropped for copies of the blob's
            // streamed, never held: only the blob's ids (bounded) are
            let mut held_back = 0usize;
            let mut in_blob: Option<HashSet<String>> = None;
            let mut held = held_of.query([id])?;
            let mut streamed = 0usize;
            while let Some(h) = held.next()? {
                // copies and kept entries alike: a huge file has millions
                streamed += 1;
                if streamed.is_multiple_of(STOP_EVERY) {
                    #[cfg(test)]
                    held_check(stop, streamed);
                    halt(stop, shard)?;
                }
                let in_blob = match &mut in_blob {
                    Some(set) => set,
                    None => {
                        let mut key = |local: &[u8], domain: i64| dedup_key(&mut suffix_of, local, domain);
                        let mut set = HashSet::new();
                        for s in &segs {
                            set.insert(key(&s.local, s.domain)?);
                        }
                        for r in &spilled {
                            set.insert(key(&r.local, r.domain)?);
                        }
                        in_blob.insert(set)
                    }
                };
                let message_id: String = h.get(0)?;
                if in_blob.contains(&message_id) {
                    continue;
                }
                tx.prepare_cached(
                    "insert into held_back (file_id, message_id, subject, part, total_parts, file_total)
                     values (?, ?, ?, ?, ?, ?)",
                )?
                .execute(params![
                    id,
                    message_id,
                    h.get::<_, Value>(1)?,
                    h.get::<_, Value>(2)?,
                    h.get::<_, Value>(3)?,
                    h.get::<_, Value>(4)?
                ])?;
                held_back += 1;
                in_tx += 1;
                if in_tx >= BATCH {
                    tx.commit()?;
                    tx = conn.transaction()?;
                    in_tx = 0;
                }
            }
            drop(held);
            file_rows.extend(spilled);

            // a row the blob already has (the same message-id) isnt copied, nor
            // is a second row of one (a spilled segment and its loose copy):
            // they go before the blob is counted, the way `seal_rows` drops them.
            // `held` stays for the streamed tail below (there is none without rows here)
            let mut held: HashSet<String> = HashSet::new();
            if !file_rows.is_empty() {
                let mut key = |local: &[u8], domain: i64| dedup_key(&mut suffix_of, local, domain);
                held = segs.iter().map(|s| key(&s.local, s.domain)).collect::<Result<_>>()?;
                let (mut dropped, mut kept) = (Vec::new(), Vec::new());
                for r in file_rows {
                    if held.insert(key(&r.local, r.domain)?) { kept.push(r) } else { dropped.push(r) }
                }
                file_rows = kept;
                if !dropped.is_empty() {
                    // out of the release's and the shard's totals too, copied as they were
                    let bytes = dropped.iter().map(|r| r.bytes.unwrap_or(0)).sum();
                    store::uncount_copies(&tx, f.get(1)?, bytes, dropped.len() as i64)?;
                    copies += dropped.len() as i64;
                    recount.push(id);
                }
            }

            // rows a blob cant hold exactly (a negative part, no size, a long message-id) stay rows
            let fits = file_rows
                .iter()
                .all(|r| r.part.is_none_or(|p| p >= 0) && r.bytes.is_some() && r.local.len() <= blob::MAX_LOCAL);
            let seen: Vec<u8> = f.get(8)?;
            // and files with more segments than a blob takes stay rows
            let small = (segs.len() + file_rows.len()) as i64 <= store::SEAL_MAX_SEGMENTS;
            // nor files with something held back: sealing them folds it in
            let seal = !file_rows.is_empty()
                && held_back == 0
                && fits
                && small
                && store::due(f.get(6)?, &seen, f.get(9)?, old_blob.is_some(), now);
            if seal {
                segs.extend(file_rows.drain(..).map(|r| Seg {
                    part: r.part,
                    bytes: r.bytes.unwrap_or(0),
                    domain: r.domain,
                    local: r.local,
                }));
                sealed += 1;
            }
            let blob = (old_blob.is_some() || seal).then(|| blob::encode(&segs));

            let mut values: Vec<Value> = (0..10).map(|i| f.get(i)).collect::<rusqlite::Result<_>>()?;
            values.push(blob.map_or(Value::Null, Value::Blob));
            tx.prepare_cached(&format!("insert into files ({FILE_COLUMNS}) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"))?
                .execute(params_from_iter(values))?;
            copied += segs.len() as i64;
            for row in &file_rows {
                copied += insert_row(&tx, row)?;
            }
            in_tx += 1 + segs.len() + file_rows.len();
            // the rest of a file too big to buffer, a row at a time, a copy of
            // a row before it dropped the same way
            let (mut tail_bytes, mut tail_copies) = (0i64, 0i64);
            #[cfg(test)]
            let held_before_tail = held.len();
            // `held` (the blob and the buffered rows, bounded) isnt added to:
            // a tail copy of a tail row is found in the copy database instead
            let mut examined = 0usize;
            while let Some(row) = pending.take_if(|r| r.file_id == id) {
                let key = dedup_key(&mut suffix_of, &row.local, row.domain)?;
                if !held.contains(&key) && !has_other_form(&tx, &row, &key)? && insert_row(&tx, &row)? == 1 {
                    copied += 1;
                } else {
                    (tail_bytes, tail_copies) = (tail_bytes + row.bytes.unwrap_or(0), tail_copies + 1);
                }
                in_tx += 1;
                // rows looked at, not copied: a tail of copies doesnt advance `copied`
                examined += 1;
                if examined.is_multiple_of(STOP_EVERY) {
                    #[cfg(test)]
                    held_check(stop, examined);
                    halt(stop, shard)?;
                }
                if in_tx >= BATCH {
                    tx.commit()?;
                    tx = conn.transaction()?;
                    in_tx = 0;
                }
                pending = next_row(shard, &mut rows, &shared)?;
            }
            #[cfg(test)]
            TAIL_HELD_GROWTH.fetch_max(held.len() - held_before_tail, Ordering::Relaxed);
            if tail_copies > 0 {
                store::uncount_copies(&tx, f.get(1)?, tail_bytes, tail_copies)?;
                copies += tail_copies;
                recount.push(id);
            }
            if in_tx >= BATCH {
                tx.commit()?;
                tx = conn.transaction()?;
                in_tx = 0;
            }
        }
        // the parts the dropped copies were counted in are what is left, and
        // so is their release's completeness (once all its files are in)
        recount.dedup();
        for id in recount {
            store::recount_parts(&tx, id)?;
        }
        // the releases' message-id index: its files and their ids are the same
        tx.execute_batch(
            "insert into release_ids select * from old.release_ids
             where file_id = 0 or file_id in (select id from files)",
        )?;
        tx.commit()?;
    }
    say(&format!("sealed {sealed} files"));

    halt(stop, shard)?;
    say("building indexes and the search index");
    conn.execute_batch(
        "detach database old;
         insert into releases_fts(releases_fts) values('rebuild');",
    )?;
    drop(conn);
    // nothing holds the original open past here: the swap renames it, and
    // Windows wont rename an open file
    drop(uses);
    {
        let conn = db::open_at(&copy)?;
        interruptible(&conn, stop)?;
        store::build_shard(&conn)?;
    }

    // the check: same rows, same NZBs
    halt(stop, shard)?;
    say("checking");
    check(&path, &copy, shard, copied, copies, stop)?;

    // swap: the last point to stop at
    halt(stop, shard)?;
    let conn = db::open_at(&copy)?;
    conn.query_row("pragma journal_mode = wal", [], |_| Ok(()))?;
    checkpoint(&conn).context("folding the copy's WAL in")?;
    drop(conn);
    // the original's -wal and -shm go aside with it: whatever they hold
    // comes back with it if the swap is cut short (`recover_cut_swaps`)
    let original = with_suffix(&path, "precompact");
    rename_db(&path, &original).context("moving the original shard aside")?;
    std::fs::rename(&copy, &path).context("putting the compacted shard in place")?;
    let after = size(&path);
    say(&format!("{:.1}GB -> {:.1}GB", before as f64 / 1e9, after as f64 / 1e9));
    Ok(Shrunk { before, after, domains_kept: kept, domains_dropped: dropped })
}

/// How many articles of a shard use each domain, as rows and inside sealed
/// blobs, counted in a database file of its own next to the shard: a shard
/// can have as many domains as a poster cares to make up, three articles
/// each, and four shards are compacted at once, soo they arent held in
/// memory. The file goes when this does.
struct DomainUses {
    conn: Option<Connection>,
    path: PathBuf,
}

impl DomainUses {
    fn count(shard_path: &Path, shard: usize, stop: &Arc<AtomicBool>) -> Result<DomainUses> {
        let path = with_suffix(shard_path, "domains");
        remove_db(&path);
        let uses = DomainUses { conn: Some(Connection::open(&path)?), path };
        let conn = uses.conn.as_ref().expect("open till dropped");
        interruptible(conn, stop)?;
        conn.query_row("pragma journal_mode = off", [], |_| Ok(()))?;
        conn.execute_batch(
            "pragma synchronous = off;
             create table uses (id integer primary key, n integer not null);",
        )?;
        conn.execute("attach database ? as old", [shard_path.to_string_lossy()])?;
        conn.execute("insert into uses select domain, count(*) from old.segments group by domain", [])?;
        // the shard isnt kept open by this file's connection: the swap renames
        // it, and Windows wont rename an open file
        conn.execute_batch("detach database old")?;
        debug_assert_eq!(attached(conn), ["main"]);
        halt(stop, shard)?;
        let read = Connection::open_with_flags(shard_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = read.prepare("select id, blob from files where blob is not null")?;
        let mut rows = stmt.query([])?;
        let mut used = conn.prepare("insert into uses values (?, 1) on conflict (id) do update set n = n + 1")?;
        conn.execute_batch("begin")?;
        while let Some(r) = rows.next()? {
            halt(stop, shard)?;
            for seg in unseal(shard, r.get(0)?, &r.get::<_, Vec<u8>>(1)?)? {
                used.execute([seg.domain])?;
            }
        }
        conn.execute_batch("commit")?;
        drop(used);
        Ok(uses)
    }

    /// whether enough articles use `domain` for it to stay
    fn shared(&self, domain: i64) -> Result<bool> {
        let n: Option<i64> = self
            .conn
            .as_ref()
            .expect("open till dropped")
            .prepare_cached("select n from uses where id = ?")?
            .query_row([domain], |r| r.get(0))
            .optional()?;
        Ok(n.is_some_and(|n| n >= SHARED_AFTER as i64))
    }
}

impl Drop for DomainUses {
    fn drop(&mut self) {
        // closed first: an open file cant be removed everywhere
        drop(self.conn.take());
        remove_db(&self.path);
    }
}

/// the schemas attached to `conn`, `main` first
fn attached(conn: &Connection) -> Vec<String> {
    conn.prepare("pragma database_list").and_then(|mut s| s.query_map([], |r| r.get(1))?.collect()).unwrap_or_default()
}

/// the files table's columns, blob last
const FILE_COLUMNS: &str =
    "id, release_id, filename, subject, subject_part, subject_mid, expected, file_total, seen, touched_at, blob";

/// A sealed file's articles. A blob that wont decode stops the shard: copying
/// on without it would lose the file's articles.
fn unseal(shard: usize, file_id: i64, blob: &[u8]) -> Result<Vec<Seg>> {
    blob::decode(blob)
        .with_context(|| format!("shard {shard}: file {file_id} has a corrupt blob; the original was kept"))
}

/// A message-id as the copy stores it, (local, domain): kept whole, packed
/// the current way under a shared domain, or made whole when its domain is
/// dropped. `suffix` is the domain's, needed for a dropped one. A rewrite
/// that doesnt give back the same message-id stops the shard.
fn rewrite(
    shard: usize,
    file_id: i64,
    local: Vec<u8>,
    domain: i64,
    shared: bool,
    suffix: Option<&str>,
) -> Result<(Vec<u8>, i64)> {
    let (new_local, new_domain) = if domain == 0 {
        return Ok((local, 0));
    } else if shared {
        (store::repack(&local), domain)
    } else {
        (store::whole(&store::decode(&local, suffix)), 0)
    };
    if new_local != local {
        let new_suffix = if new_domain == 0 { None } else { suffix };
        check_same_id(shard, file_id, (&local, suffix), (&new_local, new_suffix))?;
    }
    Ok((new_local, new_domain))
}

/// The message-id a rewritten (local, suffix) stands for is the original's:
/// a packing bug would quietly change NZBs otherwise.
fn check_same_id(
    shard: usize,
    file_id: i64,
    (old_local, old_suffix): (&[u8], Option<&str>),
    (new_local, new_suffix): (&[u8], Option<&str>),
) -> Result<()> {
    let (was, now) = (store::decode(old_local, old_suffix), store::decode(new_local, new_suffix));
    if was != now {
        bail!("shard {shard}: file {file_id}: message-id {was} came out as {now}; the original was kept");
    }
    Ok(())
}

/// one article row of the copy
struct Row {
    file_id: i64,
    local: Vec<u8>,
    domain: i64,
    part: Option<i64>,
    bytes: Option<i64>,
}

/// the next row of `select file_id, local, domain, part, bytes, suffix`, rewritten
fn next_row(shard: usize, rows: &mut rusqlite::Rows, shared: &dyn Fn(i64) -> Result<bool>) -> Result<Option<Row>> {
    let Some(r) = rows.next()? else { return Ok(None) };
    let (file_id, domain, suffix): (i64, i64, Option<String>) = (r.get(0)?, r.get(2)?, r.get(5)?);
    let (local, domain) = rewrite(shard, file_id, r.get(1)?, domain, shared(domain)?, suffix.as_deref())?;
    Ok(Some(Row { file_id, local, domain, part: r.get(3)?, bytes: r.get(4)? }))
}

/// the message-id a copied (local, domain) stands for, whichever way it is stored now
fn dedup_key(suffix_of: &mut rusqlite::Statement, local: &[u8], domain: i64) -> Result<String> {
    let suffix: Option<String> =
        if domain == 0 { None } else { suffix_of.query_row([domain], |r| r.get(0)).optional()? };
    Ok(store::dedup_key(local, domain, suffix.as_deref()))
}

/// The copy already has `r`'s file's article `key` stored the other way: whole
/// when `r` is packed under a domain, or packed under its domain (one the copy
/// kept) when `r` is whole. The same way is the segments key's to catch.
fn has_other_form(conn: &Connection, r: &Row, key: &str) -> Result<bool> {
    let other = if r.domain != 0 {
        Some((store::whole(key), 0))
    } else if let Some((local, suffix)) = store::split_message_id(key) {
        conn.prepare_cached("select id from domains where suffix = ?")?
            .query_row([suffix], |x| x.get::<_, i64>(0))
            .optional()?
            .map(|d| (store::pack_local(local), d))
    } else {
        None
    };
    let Some((local, domain)) = other else { return Ok(false) };
    Ok(conn
        .prepare_cached("select 1 from segments where file_id = ? and local = ? and domain = ?")?
        .query_row(params![r.file_id, local, domain], |_| Ok(()))
        .optional()?
        .is_some())
}

/// the most ids `compact_shard` added to `held` while streaming a file's tail
#[cfg(test)]
static TAIL_HELD_GROWTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 1 when the row went in, 0 when the copy already had it
fn insert_row(conn: &Connection, r: &Row) -> Result<i64> {
    let n = conn
        .prepare_cached("insert or ignore into segments (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)")?
        .execute(params![r.file_id, r.local, r.domain, r.part, r.bytes])?;
    Ok(n as i64)
}

/// The copy holds the same releases, files and articles (rows and inside
/// blobs) as the original, less the `copies` rows that were copies of what
/// their blob already had, and a sample of releases build the same NZBs from
/// both.
fn check(original: &Path, copy: &Path, shard: usize, copied: i64, copies: i64, stop: &Arc<AtomicBool>) -> Result<()> {
    let attach = |path: &Path| -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        interruptible(&conn, stop)?;
        conn.execute("attach database ? as ?", params![path.to_string_lossy(), format!("s{shard}")])?;
        Ok(conn)
    };
    let (old, new) = (attach(original)?, attach(copy)?);
    let count = |conn: &Connection, table: &str| -> Result<i64> {
        Ok(conn.query_row(&format!("select count(*) from s{shard}.{table}"), [], |r| r.get(0))?)
    };
    for table in ["releases", "files"] {
        let (a, b) = (count(&old, table)?, count(&new, table)?);
        if a != b {
            bail!("shard {shard}: {table} has {b} rows in the copy, {a} in the original; the original was kept");
        }
    }
    let schema = format!("s{shard}");
    let (a, b) = (store::article_count(&old, &schema)?, store::article_count(&new, &schema)?);
    if a - copies != b {
        bail!(
            "shard {shard}: {b} articles in the copy, {a} in the original ({copies} copies dropped); the original was kept"
        );
    }
    if copied != b {
        bail!("shard {shard}: some articles came out as duplicates; the original was kept");
    }

    let (low, high): (Option<i64>, Option<i64>) =
        old.query_row(&format!("select min(id), max(id) from s{shard}.releases"), [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let (Some(low), Some(high)) = (low, high) else { return Ok(()) };
    let step = ((high - low) / CHECK_SAMPLE).max(1);
    for n in 0..CHECK_SAMPLE {
        halt(stop, shard)?;
        let found: Option<i64> = old
            .query_row(
                &format!("select id from s{shard}.releases where id >= ? order by id limit 1"),
                [low + n * step],
                |r| r.get(0),
            )
            .ok();
        let Some(id) = found else { continue };
        if digest(&old, id)? != digest(&new, id)? && !(copies > 0 && same_but_copies(&old, &new, id, copy)?) {
            bail!("shard {shard}: release {id} reads back differently from the copy; the original was kept");
        }
    }
    Ok(())
}

/// A 128 bit hash of an article
fn row_hash(row: &crate::search::ArticleRow) -> u128 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let half = |salt: u8| {
        let mut h = DefaultHasher::new();
        (salt, row).hash(&mut h);
        h.finish() as u128
    };
    half(0) << 64 | half(1)
}

/// Release `id`'s articles as how many and the sum of their hashes: the
/// same whatever order they come in, and none held (a loose file can have
/// any number)
fn digest(conn: &Connection, id: i64) -> Result<(i64, u128)> {
    let (mut n, mut sum) = (0i64, 0u128);
    store::each_article(conn, id, &mut |row| {
        n += 1;
        sum = sum.wrapping_add(row_hash(&row));
        Ok(())
    })?;
    Ok((n, sum))
}

/// Release `id` reads back from `copy` the way it does from `original` with
/// only rows dropped that were a second copy of a message-id (the one kept is
/// one of the original's). Worked out in a database file next to `copy_path`
/// rather than in memory, removed after.
fn same_but_copies(original: &Connection, copy: &Connection, id: i64, copy_path: &Path) -> Result<bool> {
    let path = with_suffix(copy_path, "check");
    remove_db(&path);
    let same = (|| -> Result<bool> {
        let scratch = Connection::open(&path)?;
        scratch.query_row("pragma journal_mode = off", [], |_| Ok(()))?;
        scratch.execute_batch(
            "pragma synchronous = off;
             create table a (id text not null, h blob not null);
             create table b (id text not null, h blob not null);
             begin;",
        )?;
        for (conn, table) in [(original, "a"), (copy, "b")] {
            let mut insert = scratch.prepare(&format!("insert into {table} values (?, ?)"))?;
            store::each_article(conn, id, &mut |row| {
                insert.execute(params![row.message_id, row_hash(&row).to_be_bytes()])?;
                Ok(())
            })?;
        }
        scratch.execute_batch("commit")?;
        let none = |sql: &str| -> Result<bool> { Ok(!scratch.prepare(sql)?.exists([])?) };
        Ok(none("select 1 from b group by id having count(*) > 1")?
            && none("select id from a except select id from b")?
            && none("select id from b except select id from a")?
            && none("select h from b except select h from a")?)
    })();
    remove_db(&path);
    same
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Article, Release};

    fn no_stop() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn a_rewrite_that_changes_a_message_id_stops_the_shard() {
        let local = store::pack_local("Nyu1Q2z");
        // packed again the same way, kept whole: the same message-id
        assert_eq!(rewrite(3, 9, local.clone(), 5, true, Some("@ngPost>")).unwrap(), (local.clone(), 5));
        let (whole, domain) = rewrite(3, 9, local.clone(), 5, false, Some("@ngPost>")).unwrap();
        assert_eq!((store::decode(&whole, None), domain), ("<Nyu1Q2z@ngPost>".to_string(), 0));
        // a rewrite that came out different
        let err = check_same_id(3, 9, (&local, Some("@ngPost>")), (&store::pack_local("Nyu1Q2y"), Some("@ngPost>")))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("shard 3") && err.contains("<Nyu1Q2z@ngPost>") && err.contains("original was kept"),
            "{err}"
        );
    }

    fn busy(r: Result<Shrunk>) -> bool {
        r.is_err_and(|e| e.downcast_ref::<Busy>().is_some())
    }

    #[test]
    fn writes_from_outside_are_refused_while_compacting() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        db::create_db_at(&main).unwrap();
        drop(hold_off_compaction(&main).unwrap());

        // seen from the progress messages, while the run is going
        let refused = std::sync::Mutex::new(Vec::new());
        let progress = |_: &str| refused.lock().unwrap().push(hold_off_compaction(&main).err().map(|e| e.to_string()));
        run(&main, &progress, &no_stop()).unwrap();
        let refused = refused.into_inner().unwrap();
        assert!(
            !refused.is_empty()
                && refused.iter().all(|r| r.as_deref() == Some("the database is being compacted; try again later")),
            "refused all through the run: {refused:?}"
        );
        assert!(hold_off_compaction(&main).is_ok(), "and not after it");

        // stopped or failing: the lock goes too
        let stop = Arc::new(AtomicBool::new(true));
        assert!(run(&main, &|_| {}, &stop).is_err());
        assert!(hold_off_compaction(&main).is_ok());
    }

    #[test]
    fn a_second_compaction_at_once_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        db::create_db_at(&main).unwrap();

        let second = std::sync::Mutex::new(Vec::new());
        let progress = |_: &str| {
            let mut second = second.lock().unwrap();
            if second.is_empty() {
                second.push(run(&main, &|_| {}, &no_stop()));
            }
        };
        run(&main, &progress, &no_stop()).unwrap();
        let second = second.into_inner().unwrap().pop().expect("the second ran during the first");
        assert!(busy(second), "the second was refused, the first finished");
    }

    #[test]
    fn a_write_from_outside_holds_off_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        db::create_db_at(&main).unwrap();

        let write = hold_off_compaction(&main).unwrap();
        // writes dont hold each other off
        let other = hold_off_compaction(&main).unwrap();
        assert!(busy(run(&main, &|_| {}, &no_stop())));
        assert!(!store::shard_paths(&main).iter().any(|p| with_suffix(p, "compact").exists()), "nothing was started");
        drop((write, other));
        run(&main, &|_| {}, &no_stop()).unwrap();
        assert!(lock_path(&main).exists(), "the lock file stays, unlocked");
        assert!(hold_off_compaction(&main).is_ok());
    }

    /// every release's NZB rows, by id
    fn all_articles(main: &Path) -> Vec<(i64, Vec<crate::search::ArticleRow>)> {
        let conn = db::open_with_shards(main).unwrap();
        let ids: Vec<i64> = conn
            .prepare("select id from releases order by id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        ids.into_iter().map(|id| (id, store::articles(&conn, id).unwrap())).collect()
    }

    /// articles over every shard, loose rows and blobs
    fn article_total(main: &Path) -> i64 {
        store::shard_paths(main).iter().map(|p| store::article_count(&db::open_at(p).unwrap(), "main").unwrap()).sum()
    }

    /// segment rows over every shard (articles not sealed into a blob)
    fn loose(main: &Path) -> i64 {
        store::shard_paths(main)
            .iter()
            .map(|p| {
                db::open_at(p).unwrap().query_row("select count(*) from segments", [], |r| r.get::<_, i64>(0)).unwrap()
            })
            .sum()
    }

    fn domains(main: &Path) -> i64 {
        let conn = db::open_with_shards(main).unwrap();
        let sql = format!(
            "select sum(c) from ({})",
            store::each_shard(|i| format!("select count(*) as c from s{i}.domains"))
        );
        conn.query_row(&sql, [], |r| r.get(0)).unwrap()
    }

    /// every blob's segments, checked against the domains of their shard
    fn blob_segs_with_known_domains(main: &Path) -> Vec<Seg> {
        let mut all = Vec::new();
        for path in store::shard_paths(main) {
            let conn = db::open_at(&path).unwrap();
            let ids: std::collections::HashSet<i64> = conn
                .prepare("select id from domains")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let blobs: Vec<Vec<u8>> = conn
                .prepare("select blob from files where blob is not null")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            for b in blobs {
                for seg in crate::blob::decode(&b).unwrap() {
                    assert!(seg.domain == 0 || ids.contains(&seg.domain), "{seg:?} points at a missing domain");
                    all.push(seg);
                }
            }
        }
        all
    }

    /// what the conversion left, from before locals were packed by shape: 40
    /// releases over several groups, 6 articles each, three with the shared
    /// ngPost domain (their locals stored as text) and three with a made up
    /// domain each, every domain in the table
    fn legacy_fixture(main: &Path) {
        db::create_db_at(main).unwrap();
        let releases: Vec<Release> = (0..40)
            .map(|r| Release {
                name: format!("Rel.{r}"),
                group: format!("alt.binaries.g{}", r % 5),
                articles: (1..=6)
                    .map(|p| Article {
                        message_id: if p % 2 == 0 {
                            format!("<Nyu{r}Q{p}z@ngPost>")
                        } else {
                            format!("<x{r}y{p}@Made{r}Up{p}>")
                        },
                        subject: format!("\"f.rar\" yEnc ({p}/6)"),
                        filename: Some("f.rar".into()),
                        part: Some(p),
                        total_parts: Some(6),
                        bytes: 10,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
        store::save(main, &releases).unwrap();

        for path in store::shard_paths(main) {
            let conn = db::open_at(&path).unwrap();
            let whole: Vec<(i64, Vec<u8>)> = conn
                .prepare("select file_id, local from segments where domain = 0")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            for (file_id, local) in whole {
                let id = String::from_utf8(local[1..].to_vec()).unwrap();
                let (part, suffix) = store::split_message_id(&id).unwrap();
                conn.execute("insert or ignore into domains (suffix) values (?)", [suffix]).unwrap();
                let d: i64 = conn.query_row("select id from domains where suffix = ?", [suffix], |r| r.get(0)).unwrap();
                // the old packing: anything but hex as text
                let mut text = vec![0u8];
                text.extend_from_slice(part.as_bytes());
                conn.execute(
                    "update segments set local = ?, domain = ? where file_id = ? and local = ? and domain = 0",
                    params![text, d, file_id, local],
                )
                .unwrap();
            }
            let packed: Vec<(i64, Vec<u8>, i64)> = conn
                .prepare("select file_id, local, domain from segments where local >= x'01'")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            for (file_id, local, domain) in packed {
                let mut text = vec![0u8];
                text.extend_from_slice(store::unpack_local(&local).as_bytes());
                conn.execute(
                    "update segments set local = ? where file_id = ? and local = ? and domain = ?",
                    params![text, file_id, local, domain],
                )
                .unwrap();
            }
        }
        assert_eq!(domains(main), 120 + 5, "every made up domain has a row, ngPost one per shard used");
    }

    /// The sample is compared by digest, order aside; a copy that lost a
    /// second copy of a message-id is told apart from one that reads back
    /// differently in a file of its own, removed after.
    #[test]
    fn the_check_compares_digests_and_works_copies_out_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let (shard, path) = store::shard_paths(&main)
            .into_iter()
            .enumerate()
            .find(|(_, p)| store::article_count(&db::open_at(p).unwrap(), "main").unwrap() > 0)
            .unwrap();
        checkpoint(&db::open_at(&path).unwrap()).unwrap();
        let copy = with_suffix(&path, "compact");
        std::fs::copy(&path, &copy).unwrap();
        let n = store::article_count(&db::open_at(&path).unwrap(), "main").unwrap();
        let checks = |copies: i64| {
            let r = check(&path, &copy, shard, n, copies, &no_stop());
            assert!(!with_suffix(&copy, "check").exists(), "no scratch file left");
            r
        };
        checks(0).unwrap();

        // the original with a second copy of a message-id, stored whole next to its packed one
        let conn = db::open_at(&path).unwrap();
        let (file_id, local, domain, part, bytes, suffix): (i64, Vec<u8>, i64, Option<i64>, i64, String) = conn
            .query_row(
                "select s.file_id, s.local, s.domain, s.part, s.bytes, d.suffix
                 from segments s join domains d on d.id = s.domain limit 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        let (whole, none) = rewrite(shard, file_id, local, domain, false, Some(&suffix)).unwrap();
        assert_eq!(none, 0);
        conn.execute(
            "insert into segments (file_id, local, domain, part, bytes) values (?, ?, 0, ?, ?)",
            params![file_id, whole, part, bytes],
        )
        .unwrap();
        drop(conn);
        checks(1).unwrap();

        // a copy whose article reads back differently
        let conn = db::open_at(&copy).unwrap();
        conn.execute(
            "update segments set bytes = bytes + 1
             where (file_id, local, domain) = (select file_id, local, domain from segments limit 1)",
            [],
        )
        .unwrap();
        drop(conn);
        let e = checks(1).unwrap_err().to_string();
        assert!(e.contains("reads back differently"), "{e}");
    }

    #[test]
    fn domain_uses_are_counted_on_disk_and_the_file_goes_with_them() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        for path in store::shard_paths(&main) {
            let conn = db::open_at(&path).unwrap();
            checkpoint(&conn).unwrap();
            let rows: Vec<(i64, i64)> = conn
                .prepare("select domain, count(*) from segments group by domain")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            drop(conn);
            let file = with_suffix(&path, "domains");
            let uses = DomainUses::count(&path, 0, &no_stop()).unwrap();
            assert!(file.exists(), "counted in a file, not in memory");
            for (d, n) in rows {
                assert_eq!(uses.shared(d).unwrap(), n >= SHARED_AFTER as i64, "domain {d} used {n} times");
            }
            assert!(!uses.shared(i64::MAX).unwrap(), "an unused domain isnt shared");
            drop(uses);
            assert!(!file.exists(), "removed once done with");
        }
    }

    #[test]
    fn compacting_drops_single_use_domains_repacks_seals_and_keeps_every_nzb() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        let count = article_total(&main);
        assert_eq!(count, 240);

        let shrunk = run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(shrunk.domains_dropped, 120);
        assert_eq!(domains(&main), shrunk.domains_kept);
        assert!(shrunk.domains_kept <= 5);
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
        assert_eq!(article_total(&main), count);
        assert_eq!(loose(&main), 0, "every file was due (complete), so all are sealed");
        let segs = blob_segs_with_known_domains(&main);
        assert_eq!(segs.len() as i64, count);
        for seg in segs.iter().filter(|s| s.domain != 0) {
            assert_eq!(seg.local, store::repack(&seg.local), "packed the current way");
            assert_ne!(seg.local[0], 0, "not text");
        }
        for path in store::shard_paths(&main) {
            assert!(!with_suffix(&path, "precompact").exists(), "originals removed once all are done");
            assert!(!with_suffix(&path, "domains").exists(), "the domain counts removed");
            let mode: String = db::open_at(&path).unwrap().query_row("pragma journal_mode", [], |r| r.get(0)).unwrap();
            assert_eq!(mode, "wal");
        }

        // saving goes on as before, without new single use domains
        let more = Release {
            name: "Rel.0".into(),
            group: "alt.binaries.g0".into(),
            articles: vec![Article {
                message_id: "<x0y1@Made0Up1>".into(),
                filename: Some("f.rar".into()),
                part: Some(1),
                total_parts: Some(6),
                bytes: 10,
                ..Default::default()
            }],
            ..Default::default()
        };
        store::save(&main, &[more]).unwrap();
        assert_eq!(all_articles(&main), before, "an article already there isnt saved twice");
    }

    #[test]
    fn compacting_shards_from_before_sealing_adds_what_they_lack_and_keeps_every_nzb() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        let count = article_total(&main);
        // back to the shard schema before sealing: no held_back, no seal columns
        for path in store::shard_paths(&main) {
            db::open_at(&path)
                .unwrap()
                .execute_batch(
                    "drop table held_back;
                     alter table files drop column blob;
                     alter table files drop column touched_at;",
                )
                .unwrap();
        }

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
        assert_eq!(article_total(&main), count);
        assert_eq!(loose(&main), 0, "every file was due, so all are sealed");
    }

    #[test]
    fn a_huge_sparse_domain_id_costs_no_memory_to_count() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        // ids as sparse as earlier compactions can leave them
        for path in store::shard_paths(&main) {
            let conn = db::open_at(&path).unwrap();
            conn.execute_batch(
                "update segments set domain = domain + 4000000000000 where domain != 0;
                 update domains set id = id + 4000000000000;",
            )
            .unwrap();
        }
        let shrunk = run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(shrunk.domains_dropped, 120);
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
    }

    /// the file of release `name` in its group's shard: (shard path, file id)
    /// a shard with release Rel: a.rar sealed whole (1/1) and b.rar sealed
    /// with 1 of 2, and a row added to b.rar the way a save past its decode
    /// budget leaves one: (message-id, part, total) held back, its part
    /// counted. The shard, b.rar's id and its release's id.
    fn held_back_fixture(main: &Path, (id, part, total): (&str, i64, i64)) -> (PathBuf, i64, i64) {
        db::create_db_at(main).unwrap();
        let art = |file: &str, part: i64, total: i64, id: &str| Article {
            message_id: id.into(),
            subject: format!("\"{file}\" yEnc ({part}/{total})"),
            filename: Some(file.into()),
            part: Some(part),
            total_parts: Some(total),
            bytes: 100,
            ..Default::default()
        };
        let release = Release {
            name: "Rel".into(),
            group: "alt.binaries.t".into(),
            articles: vec![art("a.rar", 1, 1, "<a1@x>"), art("b.rar", 1, 2, "<b1@x>")],
            ..Default::default()
        };
        store::save(main, &[release]).unwrap();
        let path = store::shard_path(main, store::shard_of("alt.binaries.t"));
        let conn = db::open_at(&path).unwrap();
        let file: i64 = conn.query_row("select id from files where filename = 'b.rar'", [], |r| r.get(0)).unwrap();
        for f in conn.prepare("select id from files").unwrap().query_map([], |r| r.get::<_, i64>(0)).unwrap() {
            store::seal_file(&conn, f.unwrap()).unwrap();
        }
        let release: i64 = conn.query_row("select release_id from files where id = ?", [file], |r| r.get(0)).unwrap();
        conn.execute(
            "insert into segments (file_id, local, domain, part, bytes) values (?, ?, 0, ?, 100)",
            params![file, store::whole(id), part],
        )
        .unwrap();
        conn.execute(
            "insert into held_back (file_id, message_id, subject, part, total_parts) values (?, ?, ?, ?, ?)",
            params![file, id, format!("\"b.rar\" yEnc ({part}/{total})"), part, total],
        )
        .unwrap();
        let mut seen = Vec::new();
        for p in [1, part] {
            store::insert_part(&mut seen, p);
        }
        conn.execute("update files set seen = ? where id = ?", params![seen, file]).unwrap();
        let complete = part == 2;
        conn.execute(
            "update releases set complete = ?, size = size + 100, parts = parts + 1 where id = ?",
            params![complete, release],
        )
        .unwrap();
        store::add_totals(&conn, 0, 1).unwrap();
        (path, file, release)
    }

    /// A new row's held back total survives compaction (which leaves its
    /// file a row), and sealing the file afterwards folds it in.
    #[test]
    fn compacting_keeps_what_a_new_row_held_back() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        let (path, file, _) = held_back_fixture(&main, ("<b3@x>", 3, 3));
        run(&main, &|_| {}, &no_stop()).unwrap();
        let conn = db::open_at(&path).unwrap();
        let held: i64 = conn.query_row("select count(*) from held_back", [], |r| r.get(0)).unwrap();
        assert_eq!(held, 1);
        store::seal_file(&conn, file).unwrap();
        let expected: i64 = conn.query_row("select expected from files where id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(expected, 3);
    }

    /// The stop hook goes with its guard: nothing is left in the registry for
    /// another test's flag to meet.
    #[test]
    fn the_stop_hook_is_removed_when_its_guard_drops() {
        let stop = no_stop();
        let check = HeldCheck::on(&stop);
        assert!(HELD_CHECKS.lock().unwrap().iter().any(|e| e.0 == check.0));
        let at = check.0;
        drop(check);
        assert!(HELD_CHECKS.lock().unwrap().iter().all(|e| e.0 != at));
    }

    /// A stop during a huge held back stream ends it at the next look, not
    /// after the whole file's entries.
    #[test]
    fn a_stop_during_a_large_held_back_stream_ends_it_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        let (path, file, _) = held_back_fixture(&main, ("<b1@x>", 2, 2));
        {
            let mut conn = db::open_at(&path).unwrap();
            let tx = conn.transaction().unwrap();
            {
                let mut insert = tx
                    .prepare("insert into held_back (file_id, message_id, part, total_parts) values (?, ?, ?, 2)")
                    .unwrap();
                for n in 0..(STOP_EVERY as i64 * 3) {
                    insert.execute(params![file, format!("<new{n}@x>"), n + 3]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let stop = no_stop();
        let check = HeldCheck::on(&stop);
        let err = run(&main, &|_| {}, &stop).unwrap_err();
        assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        assert_eq!(check.looks(), 1, "stopped at the first look, not after 3 of them");
    }

    /// A stop during a huge tail of copies (rows `copied` doesnt count) is
    /// looked at every STOP_EVERY rows examined.
    #[test]
    fn a_stop_during_a_tail_of_copies_ends_it_by_rows_examined() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        {
            let conn = db::open_at(&path).unwrap();
            let d: i64 = conn.query_row("select id from domains where suffix = '@ngPost>'", [], |r| r.get(0)).unwrap();
            // past what a blob takes, then as many whole ids as a stop look is
            // apart; each one again packed after it, a copy of the one before
            let n = store::SEAL_MAX_SEGMENTS + 1 + STOP_EVERY as i64;
            conn.execute(
                "with recursive n(i) as (select 1 union all select i + 1 from n where i < ?2)
                 insert into segments (file_id, local, domain, part, bytes)
                 select ?1, cast(x'00' || cast('!' || i as blob) as blob), 0, 100 + i, 1 from n",
                params![file, n],
            )
            .unwrap();
            let mut insert = conn
                .prepare("insert into segments (file_id, local, domain, part, bytes) values (?, ?, ?, 1, 1)")
                .unwrap();
            for i in 1..=STOP_EVERY as i64 * 2 {
                insert.execute(params![file, store::pack_local(&format!("!{i}")), d]).unwrap();
            }
        }
        let stop = no_stop();
        let check = HeldCheck::on(&stop);
        let err = run(&main, &|_| {}, &stop).unwrap_err();
        assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        assert_eq!(check.looks(), 1);
        assert_eq!(check.examined(), STOP_EVERY, "the first look is after STOP_EVERY rows of the tail");
    }

    /// More held back entries than a batch take are streamed across: the new
    /// ones kept, the blob's copies dropped, and the releases' message-id
    /// index comes along.
    #[test]
    fn compacting_streams_many_held_back_entries_and_keeps_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        let (path, file, _) = held_back_fixture(&main, ("<b1@x>", 2, 2));
        let many = BATCH as i64 + 7;
        {
            let conn = db::open_at(&path).unwrap();
            let mut insert = conn
                .prepare("insert into held_back (file_id, message_id, part, total_parts) values (?, ?, ?, 2)")
                .unwrap();
            for n in 0..many {
                insert.execute(params![file, format!("<new{n}@x>"), n + 3]).unwrap();
            }
        }
        let ids = |path: &Path| -> i64 {
            db::open_at(path).unwrap().query_row("select count(*) from release_ids", [], |r| r.get(0)).unwrap()
        };
        let indexed = ids(&path);
        assert!(indexed > 0, "two files: the release is indexed");
        run(&main, &|_| {}, &no_stop()).unwrap();
        let conn = db::open_at(&path).unwrap();
        let held: i64 = conn.query_row("select count(*) from held_back", [], |r| r.get(0)).unwrap();
        assert_eq!(held, many, "the copy's entry dropped, every new one kept");
        assert_eq!(ids(&path), indexed);
    }

    /// Compaction drops a row that copies b.rar's part 1 but claimed part 2:
    /// the file's parts and its release's completeness go back to what the
    /// blob has (1 of 2), and what the copy held back goes with it.
    #[test]
    fn compacting_a_copy_away_takes_its_part_out_of_completeness() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        let (path, file, release) = held_back_fixture(&main, ("<b1@x>", 2, 2));
        let complete = |path: &Path| -> bool {
            db::open_at(path)
                .unwrap()
                .query_row("select complete from releases where id = ?", [release], |r| r.get(0))
                .unwrap()
        };
        assert!(complete(&path), "the copy's part 2 made it look complete");
        run(&main, &|_| {}, &no_stop()).unwrap();
        assert!(!complete(&path), "b.rar has 1 of 2");
        let conn = db::open_at(&path).unwrap();
        let seen: Vec<u8> = conn.query_row("select seen from files where id = ?", [file], |r| r.get(0)).unwrap();
        let mut one = Vec::new();
        store::insert_part(&mut one, 1);
        assert_eq!(seen, one);
        let held: i64 = conn.query_row("select count(*) from held_back", [], |r| r.get(0)).unwrap();
        assert_eq!(held, 0);
    }

    fn file_of(main: &Path, name: &str, group: &str) -> (PathBuf, i64) {
        let path = store::shard_path(main, store::shard_of(group));
        let id = db::open_at(&path)
            .unwrap()
            .query_row(
                "select f.id from files f join releases r on r.id = f.release_id where r.name = ? and r.group_name = ?",
                [name, group],
                |r| r.get(0),
            )
            .unwrap();
        (path, id)
    }

    #[test]
    fn compacting_sealed_shards_again_keeps_every_nzb() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(loose(&main), 0);

        // late articles for a sealed file, under a domain only one of them
        // ends up using (the first two are stored whole), the file untouched
        // long enough to seal again
        let late = Release {
            name: "Rel.1".into(),
            group: "alt.binaries.g1".into(),
            articles: (7..=9)
                .map(|p| Article {
                    message_id: format!("<late{p}@Late>"),
                    filename: Some("f.rar".into()),
                    part: Some(p),
                    total_parts: Some(6),
                    bytes: 20,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        store::save(&main, &[late]).unwrap();
        let (path, late_file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        {
            let conn = db::open_at(&path).unwrap();
            let late_domain: i64 = conn
                .query_row(
                    "select count(*) from segments s join domains d on d.id = s.domain where d.suffix = '@Late>'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(late_domain, 1, "one row under the new domain, two whole");
            conn.execute("update files set touched_at = 0 where id = ?", [late_file]).unwrap();
        }

        // a domain only a sealed blob uses, once
        let (path, lonely_file) = file_of(&main, "Rel.2", "alt.binaries.g2");
        {
            let conn = db::open_at(&path).unwrap();
            conn.execute("insert into domains (suffix) values ('@Lonely>')", []).unwrap();
            let lonely = conn.last_insert_rowid();
            let blob: Vec<u8> =
                conn.query_row("select blob from files where id = ?", [lonely_file], |r| r.get(0)).unwrap();
            let mut segs = crate::blob::decode(&blob).unwrap();
            segs.push(Seg { part: Some(7), bytes: 30, domain: lonely, local: store::pack_local("solo7") });
            conn.execute("update files set blob = ? where id = ?", params![crate::blob::encode(&segs), lonely_file])
                .unwrap();
        }

        let before = all_articles(&main);
        let ids: Vec<&str> = before.iter().flat_map(|(_, rows)| rows.iter().map(|r| r.message_id.as_str())).collect();
        for id in ["<late7@Late>", "<late8@Late>", "<late9@Late>", "<solo7@Lonely>"] {
            assert!(ids.contains(&id), "{id} reads back before compacting");
        }
        let count = article_total(&main);
        assert_eq!(count, 240 + 4);

        let shrunk = run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(shrunk.domains_dropped, 2, "@Late> and @Lonely>");
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
        assert_eq!(article_total(&main), count);
        assert_eq!(loose(&main), 0, "the late rows are sealed in with the blob");
        assert_eq!(blob_segs_with_known_domains(&main).len() as i64, count);
    }

    /// `file`'s release's size and parts, and its shard's article total
    fn release_totals(shard: &Path, file: i64) -> (i64, i64, i64) {
        db::open_at(shard)
            .unwrap()
            .query_row(
                "select r.size, r.parts, (select value from meta where key = 'articles')
                 from releases r join files f on f.release_id = r.id where f.id = ?",
                [file],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// A loose row a sealed file's blob already has (same message-id) goes
    /// when compaction seals the file, instead of becoming a second copy in
    /// the blob for good.
    #[test]
    fn compacting_drops_a_loose_row_its_blob_already_has() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let before = all_articles(&main);
        let count = article_total(&main);

        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        let once = release_totals(&path, file);
        {
            let conn = db::open_at(&path).unwrap();
            let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
            let seg = crate::blob::decode(&blob).unwrap().remove(0);
            conn.execute(
                "insert into segments (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)",
                params![file, seg.local, seg.domain, seg.part, seg.bytes],
            )
            .unwrap();
            conn.execute("update files set touched_at = 0 where id = ?", [file]).unwrap();
            // counted the way a save past its decode budget counts it
            conn.execute(
                "update releases set size = size + ?, parts = parts + 1
                 where id = (select release_id from files where id = ?)",
                params![seg.bytes, file],
            )
            .unwrap();
            store::add_totals(&conn, 0, 1).unwrap();
        }
        assert_eq!(article_total(&main), count + 1);

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(article_total(&main), count, "the copy went, not into the blob");
        assert_eq!(release_totals(&path, file), once, "and out of its release's and the shard's totals");
        assert_eq!(loose(&main), 0);
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
    }

    /// A segment stored whole in a blob (before its domain became shared) and
    /// the same message-id posted again, packed under the shared domain, are
    /// one article: compaction drops the row.
    #[test]
    fn compacting_drops_a_packed_row_whose_blob_has_the_same_id_whole() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let before = all_articles(&main);
        let count = article_total(&main);

        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        let once = release_totals(&path, file);
        {
            let conn = db::open_at(&path).unwrap();
            let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
            let mut segs = crate::blob::decode(&blob).unwrap();
            let at = segs.iter().position(|s| s.domain != 0).expect("a packed segment");
            let seg = segs[at].clone();
            let suffix: String =
                conn.query_row("select suffix from domains where id = ?", [seg.domain], |r| r.get(0)).unwrap();
            // the blob has it whole, the late row packed
            segs[at].local = store::whole(&store::decode(&seg.local, Some(&suffix)));
            segs[at].domain = 0;
            conn.execute("update files set blob = ? where id = ?", params![crate::blob::encode(&segs), file]).unwrap();
            conn.execute(
                "insert into segments (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)",
                params![file, seg.local, seg.domain, seg.part, seg.bytes],
            )
            .unwrap();
            conn.execute("update files set touched_at = 0 where id = ?", [file]).unwrap();
            conn.execute(
                "update releases set size = size + ?, parts = parts + 1
                 where id = (select release_id from files where id = ?)",
                params![seg.bytes, file],
            )
            .unwrap();
            store::add_totals(&conn, 0, 1).unwrap();
        }
        assert_eq!(article_total(&main), count + 1);

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(article_total(&main), count, "the late copy went");
        assert_eq!(release_totals(&path, file), once);
        assert_eq!(loose(&main), 0);
        assert_eq!(all_articles(&main), before);
    }

    /// A sealed segment whose message-id grows past what a blob takes when its
    /// domain is dropped stays a row, so no blob outgrows `decode_capped`.
    #[test]
    fn a_dropped_domains_long_message_id_leaves_the_blob_as_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        {
            let conn = db::open_at(&path).unwrap();
            let suffix = format!("@{}>", "d".repeat(240));
            conn.execute("insert into domains (suffix) values (?)", [&suffix]).unwrap();
            let d: i64 = conn.query_row("select id from domains where suffix = ?", [&suffix], |r| r.get(0)).unwrap();
            let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
            let mut segs = crate::blob::decode(&blob).unwrap();
            let mut local = vec![0u8];
            local.extend(std::iter::repeat_n(b'a', 300));
            segs.push(Seg { part: Some(99), bytes: 5, domain: d, local });
            conn.execute("update files set blob = ? where id = ?", params![crate::blob::encode(&segs), file]).unwrap();
        }
        let before = all_articles(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
        let conn = db::open_at(&path).unwrap();
        let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
        assert!(crate::blob::decode(&blob).unwrap().iter().all(|s| s.local.len() <= crate::blob::MAX_LOCAL));
        let rows: i64 =
            conn.query_row("select count(*) from segments where file_id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "the long one is a row");
    }

    /// A sealed segment and its loose duplicate, the only users of a domain:
    /// dropping it makes both too long for a blob, and the two are one article
    /// stored once, not a clash on the segments key.
    #[test]
    fn a_dropped_domains_long_blob_segment_and_its_loose_copy_are_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        {
            let conn = db::open_at(&path).unwrap();
            let suffix = format!("@{}>", "d".repeat(240));
            conn.execute("insert into domains (suffix) values (?)", [&suffix]).unwrap();
            let d: i64 = conn.query_row("select id from domains where suffix = ?", [&suffix], |r| r.get(0)).unwrap();
            let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
            let mut segs = crate::blob::decode(&blob).unwrap();
            let mut local = vec![0u8];
            local.extend(std::iter::repeat_n(b'a', 300));
            segs.push(Seg { part: Some(99), bytes: 5, domain: d, local: local.clone() });
            conn.execute("update files set blob = ? where id = ?", params![crate::blob::encode(&segs), file]).unwrap();
            // the same message-id again, loose, counted as the save that kept it counted it
            conn.execute(
                "insert into segments (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)",
                params![file, local, d, 99, 5],
            )
            .unwrap();
            conn.execute(
                "update releases set size = size + 10, parts = parts + 2
                 where id = (select release_id from files where id = ?)",
                [file],
            )
            .unwrap();
            store::add_totals(&conn, 0, 2).unwrap();
        }
        let before = all_articles(&main);
        let count = article_total(&main);
        let totals = release_totals(&path, file);

        run(&main, &|_| {}, &no_stop()).unwrap();
        let count_in = |all: &[(i64, Vec<crate::search::ArticleRow>)]| all.iter().map(|(_, a)| a.len()).sum::<usize>();
        assert_eq!(count_in(&all_articles(&main)), count_in(&before) - 1, "the NZBs lose only the copy");
        assert_eq!(article_total(&main), count - 1, "the copy went out of the shard's total");
        assert_eq!(release_totals(&path, file), (totals.0 - 5, totals.1 - 1, totals.2 - 1), "and the release's");
        let conn = db::open_at(&path).unwrap();
        let rows: i64 =
            conn.query_row("select count(*) from segments where file_id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "one row");
        let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
        assert!(crate::blob::decode(&blob).unwrap().iter().all(|s| s.local.len() <= crate::blob::MAX_LOCAL));
    }

    /// The same, in a file with more rows than a blob takes: the loose copy
    /// comes in the streamed tail, past the rows held to seal, and is still
    /// one row with the copy out of the totals, not a failed check every retry.
    #[test]
    fn a_long_blob_segments_loose_copy_past_the_buffered_rows_is_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        {
            let conn = db::open_at(&path).unwrap();
            let suffix = format!("@{}>", "d".repeat(240));
            conn.execute("insert into domains (suffix) values (?)", [&suffix]).unwrap();
            let d: i64 = conn.query_row("select id from domains where suffix = ?", [&suffix], |r| r.get(0)).unwrap();
            let blob: Vec<u8> = conn.query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
            let mut segs = crate::blob::decode(&blob).unwrap();
            let mut local = vec![0u8];
            local.extend(std::iter::repeat_n(b'a', 300));
            segs.push(Seg { part: Some(99), bytes: 5, domain: d, local: local.clone() });
            conn.execute("update files set blob = ? where id = ?", params![crate::blob::encode(&segs), file]).unwrap();
            // rows past what a blob takes, sorting before the copy so it comes after them
            conn.execute(
                "with recursive n(i) as (select 1 union all select i + 1 from n where i < ?2)
                 insert into segments (file_id, local, domain, part, bytes)
                 select ?1, cast(x'00' || cast('A' || i as blob) as blob), 0, 100 + i, 1 from n",
                params![file, store::SEAL_MAX_SEGMENTS + 1],
            )
            .unwrap();
            conn.execute(
                "insert into segments (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)",
                params![file, local, d, 99, 5],
            )
            .unwrap();
            conn.execute(
                "update releases set size = size + 10 + ?2, parts = parts + 2 + ?2
                 where id = (select release_id from files where id = ?1)",
                params![file, store::SEAL_MAX_SEGMENTS + 1],
            )
            .unwrap();
            store::add_totals(&conn, 0, 2 + store::SEAL_MAX_SEGMENTS + 1).unwrap();
        }
        let count = article_total(&main);
        let totals = release_totals(&path, file);

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(article_total(&main), count - 1, "the copy went out of the shard's total");
        assert_eq!(release_totals(&path, file), (totals.0 - 5, totals.1 - 1, totals.2 - 1), "and the release's");
        let conn = db::open_at(&path).unwrap();
        let rows: i64 =
            conn.query_row("select count(*) from segments where file_id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(rows, store::SEAL_MAX_SEGMENTS + 2, "the padding and one long row");
    }

    /// A file a poster padded with millions of rows: the streamed tail is
    /// deduplicated against the copy itself, not a set of every id it has,
    /// and an article stored both whole and packed in the tail is one row.
    #[test]
    fn a_huge_files_tail_is_deduplicated_without_holding_its_ids() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let (path, file) = file_of(&main, "Rel.1", "alt.binaries.g1");
        let tail = store::SEAL_MAX_SEGMENTS + 100;
        {
            let conn = db::open_at(&path).unwrap();
            let d: i64 = conn.query_row("select id from domains where suffix = '@ngPost>'", [], |r| r.get(0)).unwrap();
            // whole ids sorting before the two copies below, well past what a blob takes
            conn.execute(
                "with recursive n(i) as (select 1 union all select i + 1 from n where i < ?2)
                 insert into segments (file_id, local, domain, part, bytes)
                 select ?1, cast(x'00' || cast('!' || i as blob) as blob), 0, 100 + i, 1 from n",
                params![file, tail],
            )
            .unwrap();
            // one article twice: whole, and packed under its shared domain
            conn.execute(
                "insert into segments (file_id, local, domain, part, bytes) values (?, ?, 0, 99, 5)",
                params![file, store::whole("<zzz9@ngPost>")],
            )
            .unwrap();
            conn.execute(
                "insert into segments (file_id, local, domain, part, bytes) values (?, ?, ?, 99, 5)",
                params![file, store::pack_local("zzz9"), d],
            )
            .unwrap();
            conn.execute(
                "update releases set size = size + 10 + ?2, parts = parts + 2 + ?2
                 where id = (select release_id from files where id = ?1)",
                params![file, tail],
            )
            .unwrap();
            store::add_totals(&conn, 0, 2 + tail).unwrap();
        }
        let count = article_total(&main);
        let totals = release_totals(&path, file);

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(TAIL_HELD_GROWTH.load(Ordering::Relaxed), 0, "no id of the tail was held");
        assert_eq!(article_total(&main), count - 1, "the copy went out of the shard's total");
        assert_eq!(release_totals(&path, file), (totals.0 - 5, totals.1 - 1, totals.2 - 1), "and the release's");
        let conn = db::open_at(&path).unwrap();
        let rows: i64 =
            conn.query_row("select count(*) from segments where file_id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(rows, tail + 1, "the padding and the article once");
    }

    #[test]
    fn a_file_over_the_cap_is_copied_as_rows_and_its_nzb_is_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        // a complete file (so due) whose rows a poster padded past what a blob takes
        let (path, big) = file_of(&main, "Rel.1", "alt.binaries.g1");
        let conn = db::open_at(&path).unwrap();
        let before_rows: i64 =
            conn.query_row("select count(*) from segments where file_id = ?", [big], |r| r.get(0)).unwrap();
        let extra = store::SEAL_MAX_SEGMENTS + 1 - before_rows;
        conn.execute(
            "with recursive n(i) as (select 1 union all select i + 1 from n where i < ?2)
             insert into segments (file_id, local, domain, part, bytes)
             select ?1, cast('big' || i as blob), 0, 100 + i, 1 from n",
            params![big, extra],
        )
        .unwrap();
        drop(conn);
        let before = all_articles(&main);
        let count = article_total(&main);

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
        assert_eq!(article_total(&main), count);
        assert_eq!(loose(&main), store::SEAL_MAX_SEGMENTS + 1, "only the big file stays rows");
        let conn = db::open_at(&path).unwrap();
        let (rows, blob): (i64, Option<Vec<u8>>) = conn
            .query_row(
                "select (select count(*) from segments where file_id = f.id), f.blob from files f where id = ?",
                [big],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, blob), (store::SEAL_MAX_SEGMENTS + 1, None));

        // and without the extra row it seals again
        conn.execute("delete from segments where file_id = ? and local = cast('big1' as blob)", [big]).unwrap();
        drop(conn);
        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_eq!(loose(&main), 0, "at the cap a file seals");
    }

    #[test]
    fn a_corrupt_blob_stops_compacting_and_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        run(&main, &|_| {}, &no_stop()).unwrap();
        let (path, file) = file_of(&main, "Rel.3", "alt.binaries.g3");
        db::open_at(&path).unwrap().execute("update files set blob = x'00' where id = ?", [file]).unwrap();

        let err = run(&main, &|_| {}, &no_stop()).unwrap_err();
        assert!(format!("{err:#}").contains("corrupt"), "{err:#}");
        let blob: Vec<u8> =
            db::open_at(&path).unwrap().query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(blob, vec![0], "the original is still in place");
    }

    /// NZB rows of every release outside shard `skip`, by id
    fn articles_outside(main: &Path, skip: usize) -> Vec<(i64, Vec<crate::search::ArticleRow>)> {
        let conn = db::open_with_shards(main).unwrap();
        let sql = (0..SHARDS)
            .filter(|i| *i != skip)
            .map(|i| format!("select id from s{i}.releases"))
            .collect::<Vec<_>>()
            .join(" union all ");
        let mut ids: Vec<i64> =
            conn.prepare(&sql).unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        ids.sort();
        ids.into_iter().map(|id| (id, store::articles(&conn, id).unwrap())).collect()
    }

    #[test]
    fn a_failed_shard_doesnt_stop_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let (path, file) = file_of(&main, "Rel.3", "alt.binaries.g3");
        let bad = store::shard_of("alt.binaries.g3");
        let before = articles_outside(&main, bad);
        assert!(!before.is_empty());
        db::open_at(&path).unwrap().execute("update files set blob = x'00' where id = ?", [file]).unwrap();

        let err = run(&main, &|_| {}, &no_stop()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("shard {bad}:")) && msg.contains("corrupt"), "{msg}");

        // the others were compacted (sealed) and their backups removed
        let loose_in = |p: &Path| -> i64 {
            db::open_at(p).unwrap().query_row("select count(*) from segments", [], |r| r.get(0)).unwrap()
        };
        for (i, shard) in store::shard_paths(&main).iter().enumerate() {
            assert!(!with_suffix(shard, "precompact").exists(), "no backup left over for shard {i}");
            assert!(!with_suffix(shard, "compact").exists(), "no half made copy left over for shard {i}");
            if i == bad {
                assert!(loose_in(shard) > 0, "the failed shard still has its loose rows");
            } else {
                assert_eq!(loose_in(shard), 0, "shard {i} was compacted and sealed");
            }
        }
        assert_eq!(articles_outside(&main, bad), before, "the others read back the same");

        // the failed one is untouched
        let blob: Vec<u8> =
            db::open_at(&path).unwrap().query_row("select blob from files where id = ?", [file], |r| r.get(0)).unwrap();
        assert_eq!(blob, vec![0]);
    }

    /// no shard is half done: the files are all there, the NZBs read back the
    /// same, and nothing from the copying is left lying around
    fn assert_untouched_or_whole(main: &Path, before: &[(i64, Vec<crate::search::ArticleRow>)]) {
        for shard in store::shard_paths(main) {
            assert!(shard.exists(), "{} is still there", shard.display());
            assert!(!with_suffix(&shard, "precompact").exists(), "no backup left over");
            assert!(!with_suffix(&shard, "compact").exists(), "no half made copy left over");
        }
        assert_eq!(all_articles(main), before, "every NZB reads back the same");
    }

    #[test]
    fn a_stop_before_the_start_leaves_every_shard_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        let (rows, count) = (loose(&main), article_total(&main));

        let stop = Arc::new(AtomicBool::new(true));
        let err = run(&main, &|_| {}, &stop).unwrap_err();
        assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        assert_untouched_or_whole(&main, &before);
        assert_eq!((loose(&main), article_total(&main)), (rows, count), "nothing was sealed or dropped");
    }

    #[test]
    fn a_stop_while_copying_drops_the_copies_and_keeps_the_originals() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);

        // the flag goes up as the first shard reports the copy of its files
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let progress = move |msg: &str| {
            if msg.contains("copying files and articles") {
                flag.store(true, Ordering::Relaxed);
            }
        };
        let err = run(&main, &progress, &stop).unwrap_err();
        assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        assert!(stop.load(Ordering::Relaxed));
        assert_untouched_or_whole(&main, &before);
    }

    /// what a swap cut short after its first rename leaves: the shard of
    /// `group` moved aside as `.precompact.db`, its checked copy still next
    /// to it as `.compact.db`, nothing at the shard's own path
    fn cut_swap(main: &Path, group: &str) -> PathBuf {
        let path = store::shard_path(main, store::shard_of(group));
        let copy = with_suffix(&path, "compact");
        std::fs::copy(&path, &copy).unwrap();
        std::fs::rename(&path, with_suffix(&path, "precompact")).unwrap();
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        assert!(!path.exists() && copy.exists());
        path
    }

    #[test]
    fn a_swap_cut_short_gets_its_original_back_on_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        let path = cut_swap(&main, "alt.binaries.g3");

        db::create_db_at(&main).unwrap();
        assert!(path.exists(), "the original is back in place");
        assert!(!with_suffix(&path, "precompact").exists(), "moved back, not copied");
        assert!(!with_suffix(&path, "compact").exists(), "the copy went");
        assert_eq!(all_articles(&main), before, "every NZB reads back the same");
    }

    #[test]
    fn a_missing_shard_with_others_there_is_refused_not_made_empty() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let group = "alt.binaries.g3";
        let path = store::shard_path(&main, store::shard_of(group));

        // no backup at all
        remove_db(&path);
        let err = format!("{:#}", db::create_db_at(&main).unwrap_err());
        assert!(err.contains(&path.display().to_string()) && err.contains("missing"), "{err}");
        assert!(!path.exists(), "no empty shard in its place");

        // only a compacted copy, which may not have been checked: kept, still refused
        let copy = with_suffix(&path, "compact");
        std::fs::copy(store::shard_path(&main, (store::shard_of(group) + 1) % SHARDS), &copy).unwrap();
        let err = format!("{:#}", db::create_db_at(&main).unwrap_err());
        assert!(err.contains(&copy.display().to_string()), "{err}");
        assert!(!path.exists() && copy.exists(), "the copy is left for whoever puts it back");

        // writers dont make it either
        let release = Release { name: "Late".into(), group: group.into(), ..Default::default() };
        assert!(store::save(&main, &[release]).is_err());
        assert!(!path.exists(), "a save made no empty shard");
    }

    #[test]
    fn a_shard_is_left_alone_while_a_compaction_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        let path = cut_swap(&main, "alt.binaries.g3");

        // mid swap of a running compaction: nothing set up, nothing put back
        let compacting = Lock::take(&main).unwrap();
        assert!(db::create_db_holding(&main).unwrap().is_none());
        assert!(!path.exists() && with_suffix(&path, "precompact").exists(), "the swap is left to the compaction");
        drop(compacting);

        let held = db::create_db_holding(&main).unwrap().expect("set up once the compaction is gone");
        assert!(busy(run(&main, &|_| {}, &no_stop())), "and compaction held off while held");
        drop(held);
        assert_eq!(all_articles(&main), before);
    }

    #[test]
    fn a_compaction_puts_back_what_a_cut_swap_left_before_starting() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        cut_swap(&main, "alt.binaries.g3");

        run(&main, &|_| {}, &no_stop()).unwrap();
        assert_untouched_or_whole(&main, &before);
    }

    #[test]
    fn a_backup_left_next_to_a_shard_in_place_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);
        // cut after the copy went in: both there, and which is whole isnt known
        let path = store::shard_path(&main, store::shard_of("alt.binaries.g3"));
        let backup = with_suffix(&path, "precompact");
        std::fs::copy(&path, &backup).unwrap();

        db::create_db_at(&main).unwrap();
        assert!(backup.exists(), "kept on start");
        let err = format!("{:#}", run(&main, &|_| {}, &no_stop()).unwrap_err());
        assert!(err.contains(&backup.display().to_string()), "{err}");
        assert!(backup.exists(), "kept by a compaction too");
        assert_eq!(all_articles(&main), before);
    }

    #[test]
    fn a_shard_and_its_backup_together_refuse_writes_but_not_the_menu() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let before = all_articles(&main);

        // a normal database, no backups: untouched
        drop(hold_off_compaction(&main).unwrap());
        drop(db::create_db_holding(&main).unwrap().unwrap());

        // an older atlas made an empty shard next to the backup: which to keep isnt known
        let path = store::shard_path(&main, store::shard_of("alt.binaries.g3"));
        let backup = with_suffix(&path, "precompact");
        std::fs::copy(&path, &backup).unwrap();

        // writes are refused, with both files named and how to go on
        let err = format!("{:#}", hold_off_compaction(&main).err().expect("refused"));
        assert!(err.contains(&path.display().to_string()) && err.contains(&backup.display().to_string()), "{err}");
        assert!(err.contains("keep") && err.contains("delete"), "says how to resolve it: {err}");
        // the lock isnt kept by a refused hold: a compaction gets its own refusal, not Busy
        assert!(!busy(run(&main, &|_| {}, &no_stop())));

        // the menu still starts and reads
        drop(db::create_db_holding(&main).unwrap().expect("set up"));
        assert_eq!(all_articles(&main), before);

        // resolved: the shard is kept, the backup removed
        std::fs::remove_file(&backup).unwrap();
        drop(hold_off_compaction(&main).unwrap());
    }

    /// A checkpoint held back by a reader (its snapshot keeps WAL frames from
    /// being folded in) is an error, not taken for done.
    #[test]
    fn a_checkpoint_a_reader_holds_back_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        let writer = db::create_at(&path).unwrap();
        writer.execute_batch("pragma journal_mode = wal; pragma wal_autocheckpoint = 0; create table t (x)").unwrap();
        let reader = db::open_at(&path).unwrap();
        reader.execute_batch("begin; select count(*) from t").unwrap();
        writer.execute("insert into t values (1)", []).unwrap();

        writer.busy_timeout(std::time::Duration::ZERO).unwrap();
        let err = checkpoint(&writer).unwrap_err();
        assert!(format!("{err:#}").contains("checkpoint"), "{err:#}");
        reader.execute_batch("commit").unwrap();
        checkpoint(&writer).unwrap();
    }

    /// A shard moved aside with pages still in its WAL (committed, not
    /// folded in) gets them back with it when the swap was cut short.
    #[test]
    fn a_cut_swap_puts_the_originals_wal_back_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        legacy_fixture(&main);
        let path = store::shard_path(&main, store::shard_of("alt.binaries.g3"));
        let backup = with_suffix(&path, "precompact");
        {
            // the moved aside original: its file and its WAL, taken while a
            // connection keeps a write in the WAL
            let conn = db::open_at(&path).unwrap();
            conn.execute_batch("pragma wal_autocheckpoint = 0; create table late (x); insert into late values (7)")
                .unwrap();
            std::fs::copy(&path, &backup).unwrap();
            std::fs::copy(format!("{}-wal", path.display()), format!("{}-wal", backup.display())).unwrap();
        }
        remove_db(&path);

        recover_cut_swaps(&main).unwrap();
        let late: i64 = db::open_at(&path).unwrap().query_row("select x from late", [], |r| r.get(0)).unwrap();
        assert_eq!(late, 7, "the write in the WAL came back too");
        assert!(!Path::new(&format!("{}-wal", backup.display())).exists());
    }
}
