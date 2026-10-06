//! The main database (`atlas.db`): per group cursors and the release id
//! counter. Releases and their articles live in the shards next to it, see
//! store.rs. A database from before the shards still holds everything in
//! `atlas.db` until convert.rs moves it over.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use crate::compact::WriteGuard;
use crate::parser::Release;
use crate::paths;
use crate::store;

pub type Result<T> = rusqlite::Result<T>;

/// The main database with every shard attached (s0 .. s7), for reads across them.
pub fn open() -> Result<Connection> {
    open_with_shards(&paths::database())
}

/// The main database at `path` with its shards attached, see `open`.
pub fn open_with_shards(path: &Path) -> Result<Connection> {
    let conn = open_at(path)?;
    store::attach(&conn, path)?;
    Ok(conn)
}

/// The database at `path`, which has to be there already: opening never
/// makes one. A read (search, the dashboard) during a conversion's swap,
/// with no main database for a moment, would otherwise leave an empty one
/// in the way of the new one. Only setting up makes one, see `create_at`.
pub fn open_at(path: &Path) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    set_up(Connection::open_with_flags(path, flags)?)
}

/// The database at `path`, made (empty) when missing: for setting up
/// (`create_db_at`, a new shard), not for reads.
pub fn create_at(path: &Path) -> Result<Connection> {
    set_up(Connection::open(path)?)
}

/// A shard to write, which has to be there already: `open_at` would make a
/// missing one (empty) on the spot, and a shard a compaction was cut short
/// swapping would then look in place with nothing in it.
pub fn open_shard(path: &Path) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    set_up(Connection::open_with_flags(path, flags)?)
}

fn set_up(conn: Connection) -> Result<Connection> {
    conn.busy_timeout(Duration::from_secs(30))?;
    // bundled sqlite turns these on by default, python's didnt. purge deletes
    // releases before their articles and old dbs can hold orphans.
    conn.execute_batch("pragma foreign_keys = off")?;
    // with WAL this cant corrupt the db, a crash only loses the last commit,
    // which the indexer redoes anyway since cursors move after the data
    conn.execute_batch("pragma synchronous = normal")?;
    Ok(conn)
}

/// Tune a connection for heavy indexing writes: a big page cache, no
/// checkpointing on commit (`checkpointer` copies the WAL into the main file on
/// its own thread, `finish_checkpoint` lets the WAL start over), and a WAL file
/// cut back to 256MB whenever it starts over.
pub fn tune_for_writing(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "pragma cache_size = -262144;
         pragma temp_store = memory;
         pragma wal_autocheckpoint = 0;
         pragma journal_size_limit = 268435456;",
    )
}

/// For the writer, between transactions: copy what `checkpointer` hasnt yet
/// into the main file. With no write in progress it can copy everything, soo
/// the next transaction starts the WAL over instead of growing it. Mostly
/// quick, the background checkpointer has done the bulk of it already.
pub fn finish_checkpoint(conn: &Connection) -> Result<()> {
    conn.query_row("pragma wal_checkpoint(passive)", [], |_| Ok(()))
}

/// Background WAL checkpoints for a writer tuned with `tune_for_writing`.
/// Passive checkpoints never block readers or the writer, soo copying the WAL
/// into the main file happens alongside the writes. They never let the WAL
/// start over on their own (the writer keeps adding to it); the writer does
/// that with `finish_checkpoint`. A blocking checkpoint stalls the writer for
/// as long as it copies (20s for a 1.5GB WAL), soo it is only a last resort
/// when the WAL got huge anyway. Runs until `stop` is set.
pub fn checkpointer(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
    use std::sync::atomic::Ordering;

    std::thread::spawn(move || {
        // the main database and every shard
        let main = paths::database();
        let files: Vec<std::path::PathBuf> = std::iter::once(main.clone()).chain(store::shard_paths(&main)).collect();
        let conns: Vec<(Connection, std::path::PathBuf)> =
            files.iter().filter(|p| p.exists()).filter_map(|p| open_at(p).ok().map(|c| (c, wal_path(p)))).collect();

        let stopping = || stop.load(Ordering::Relaxed);
        while !stopping() {
            // twice a second, looking at `stop` in between soo stopping doesnt wait out the nap
            for _ in 0..5 {
                if stopping() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            for (conn, wal) in &conns {
                let big = fs::metadata(wal).is_ok_and(|m| m.len() > 8 << 30);
                let mode = if big { "truncate" } else { "passive" };
                let _ = conn.query_row(&format!("pragma wal_checkpoint({mode})"), [], |_| Ok(()));
            }
        }

        // leave small wals behind
        for (conn, _) in &conns {
            let _ = conn.query_row("pragma wal_checkpoint(truncate)", [], |_| Ok(()));
        }
    })
}

fn wal_path(db: &Path) -> std::path::PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    std::path::PathBuf::from(p)
}

/// `create_db_holding` for the database in its usual place.
pub fn create_db() -> anyhow::Result<Option<WriteGuard>> {
    let path = paths::database();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    create_db_holding(&path)
}

/// `create_db_at` with compaction held off first: a compaction moves each
/// shard aside for a moment, and setting up then would take it for missing.
/// Returns the hold, for the caller to keep while it writes or drop. None
/// while a compaction runs, and nothing was set up (a compaction only runs
/// on a database that is).
pub fn create_db_holding(path: &Path) -> anyhow::Result<Option<WriteGuard>> {
    // a conversion's swap cut short leaves no main database: finished or
    // undone first, alone (busy means someone is using the database, soo it's there)
    if (!path.try_exists()? || crate::convert::is_empty_interloper(path))
        && let Ok(_alone) = crate::compact::Lock::take(path)
        && let Some(said) = crate::convert::recover_cut_swap(path)?
    {
        crate::ui::warn(&said);
    }
    let Some(held) = crate::compact::try_hold_off_compaction(path)? else { return Ok(None) };
    create_db_at(path)?;
    Ok(Some(held))
}

/// Settings > wipe: every shard, with their sidecars and what a compaction
/// leaves (`.compact.db`, `.precompact.db`, and its scratch `.domains.db`
/// and `.compact.check.db`), a conversion's `atlas.old.db`
/// and `atlas.new.db`, then the main database. Under
/// the exclusive compaction lock, soo refused (`Busy`) while the indexer, a
/// write from outside it or a compaction has the database. Leaves nothing to
/// set up again from: the next `create_db` makes a new one. The main
/// database goes only once every shard has: a new main database next to old
/// shards would hand out their ids again. A shard that cant be removed is an
/// error, with the main database left in place.
pub fn wipe(main: &Path) -> anyhow::Result<()> {
    let _exclusive = crate::compact::Lock::take(main)?;
    let mut dbs = Vec::new();
    for shard in store::shard_paths(main) {
        let copy = crate::compact::with_suffix(&shard, "compact");
        dbs.push(crate::compact::with_suffix(&copy, "check"));
        dbs.push(copy);
        dbs.push(crate::compact::with_suffix(&shard, "domains"));
        dbs.push(crate::compact::with_suffix(&shard, "precompact"));
        dbs.push(shard);
    }
    // a conversion's backup or undone new main database would be put back
    // as the main database at the next start (see `convert::recover_cut_swap`)
    dbs.push(main.with_file_name("atlas.old.db"));
    dbs.push(main.with_file_name("atlas.new.db"));
    dbs.push(main.to_path_buf());
    for db in dbs {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let file = format!("{}{suffix}", db.display());
            match fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {file}")),
            }
        }
    }
    Ok(())
}

/// The main database and its shards, or brings an older one up to date. A
/// database from before the shards is left as it is (apart from missing
/// columns) for convert.rs; until then there are no shards. Run with
/// compaction held off, see `create_db_holding`.
pub fn create_db_at(path: &Path) -> anyhow::Result<()> {
    if !path.try_exists()? {
        refuse_orphaned_shards(path)?;
    }
    let conn = create_at(path)?;

    // wal soo the indexer can write while search reads
    conn.query_row("pragma journal_mode = wal", [], |_| Ok(()))?;

    conn.execute_batch(
        "create table if not exists groups(
            name TEXT PRIMARY KEY,
            live_cursor INTEGER,
            backfill_cursor INTEGER
        );",
    )?;

    // the server's first/last article numbers per group, for progress and ETA, and
    // the post date at both ends of what has been indexed (article number + unix time)
    let group_columns = columns(&conn, "groups")?;
    for col in ["first_article", "last_article", "low_article", "low_posted", "high_article", "high_posted"] {
        if !group_columns.contains(col) {
            conn.execute(&format!("alter table groups add column {col} INTEGER"), [])?;
        }
    }

    if has_old_layout(&conn)? {
        return Ok(migrate_old(&conn)?);
    }

    // a main database that has been used (cursors, ids handed out) is not a
    // new install, whatever shards are there
    let used = has_progress(&conn)?;
    store::create_main(&conn)?;
    create_shards(path, used)
}

/// Refuses shards with something in them when their main database `main`
/// isnt there: a new one would hand out their release ids again and start
/// the cursors and day chunks over.
fn refuse_orphaned_shards(main: &Path) -> anyhow::Result<()> {
    let mut found = Vec::new();
    for shard in store::shard_paths(main) {
        if fs::metadata(&shard).is_ok_and(|m| m.len() > 0) {
            found.push(shard.display().to_string());
        }
    }
    if !found.is_empty() {
        anyhow::bail!(
            "{} is missing but its shards are there: {}. put the main database back from a backup, or delete these shard files to start over",
            main.display(),
            found.join(", ")
        );
    }
    Ok(())
}

/// Whether the main database holds indexing progress: cursors or ids handed
/// out for releases. The shards hold what those stand for.
fn has_progress(conn: &Connection) -> Result<bool> {
    if conn.prepare("select 1 from main.groups")?.exists([])? {
        return Ok(true);
    }
    let has_meta =
        conn.prepare("select 1 from main.sqlite_master where type = 'table' and name = 'meta'")?.exists([])?;
    if !has_meta {
        return Ok(false);
    }
    let next: Option<i64> =
        conn.query_row("select value from main.meta where key = 'next_seq'", [], |r| r.get(0)).optional()?;
    Ok(next.is_some_and(|n| n > 1))
}

/// Every shard of `main`, made if new, once the ones a compaction was cut
/// short swapping are back. One missing while others are there is refused
/// rather than made empty: searches would miss what it held, and new
/// releases would go into the empty one. The same when all are missing from a
/// main database that `used` (has progress): its cursors would skip what the
/// shards held. Only a main database new to this setup gets all of them new.
fn create_shards(main: &Path, used: bool) -> anyhow::Result<()> {
    for line in crate::compact::recover_cut_swaps(main)? {
        crate::ui::warn(&line);
    }
    let shards = store::shard_paths(main);
    let mut missing = Vec::new();
    for path in &shards {
        if !path.try_exists()? {
            missing.push(path);
        }
    }
    if !missing.is_empty() && (missing.len() < shards.len() || used) {
        let each: Vec<String> = missing
            .iter()
            .map(|p| {
                let copy = crate::compact::with_suffix(p, "compact");
                if copy.exists() {
                    format!("{} (a compacted copy of it, maybe not checked, is {})", p.display(), copy.display())
                } else {
                    p.display().to_string()
                }
            })
            .collect();
        anyhow::bail!(
            "shards missing from a database in use: {}. not making empty ones in their place; put them back first",
            each.join(", ")
        );
    }
    for shard in shards {
        store::create_shard(&shard)?;
    }
    Ok(())
}

/// releases still in the main database, from before the shards
pub fn has_old_layout(conn: &Connection) -> Result<bool> {
    conn.prepare("select 1 from main.sqlite_master where type = 'table' and name = 'releases'")?.exists([])
}

fn columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare(&format!("pragma table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_>>()?;
    Ok(cols)
}

/// Old layout: columns its tables gained over time, soo the conversion can
/// read every database the same way.
fn migrate_old(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "create table if not exists articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            release_id INTEGER,
            message_id TEXT,
            subject TEXT,
            filename TEXT,
            part INTEGER,
            total_parts INTEGER,
            bytes INTEGER,
            file_total INTEGER,
            unique(release_id, message_id)
        );",
    )?;

    let release_columns = columns(conn, "releases")?;
    let add = |col: &str, ty: &str| -> Result<()> {
        if !release_columns.contains(col) {
            conn.execute(&format!("alter table releases add column {col} {ty}"), [])?;
        }
        Ok(())
    };

    add("group_name", "TEXT")?;
    add("poster", "TEXT")?;
    add("posted_date", "TEXT")?;

    if !release_columns.contains("parts") {
        conn.execute("alter table releases add column parts INTEGER", [])?;
        // backfill parts for releases already in the db
        conn.execute(
            "update releases set parts = (select count(*) from articles where release_id = releases.id) where parts is null",
            [],
        )?;
    }

    add("file_total", "INTEGER")?;
    add("display_name", "TEXT")?;

    if !release_columns.contains("is_obfuscated") {
        conn.execute("alter table releases add column is_obfuscated INTEGER default 0", [])?;
        if release_columns.contains("obfuscated") {
            conn.execute("update releases set is_obfuscated = obfuscated", [])?;
        }
    }

    let article_columns = columns(conn, "articles")?;
    if !article_columns.contains("subject") {
        conn.execute("alter table articles add column subject TEXT", [])?;
    }
    if !article_columns.contains("file_total") {
        conn.execute("alter table articles add column file_total INTEGER", [])?;
    }

    rebuild_articles_unique(conn)
}

/// very old dbs had no unique(release_id, message_id), rebuild the table with it
fn rebuild_articles_unique(conn: &Connection) -> Result<()> {
    let sql: Option<String> = conn
        .query_row("select sql from sqlite_master where type = 'table' and name = 'articles'", [], |r| r.get(0))
        .optional()?;

    match sql {
        None => return Ok(()),
        Some(sql) if sql.to_lowercase().contains("unique(release_id, message_id)") => return Ok(()),
        _ => {}
    }

    conn.execute_batch(
        "
        begin;
        create table articles_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            release_id INTEGER,
            message_id TEXT,
            subject TEXT,
            filename TEXT,
            part INTEGER,
            total_parts INTEGER,
            bytes INTEGER,
            file_total INTEGER,
            foreign key (release_id) references releases(id),
            unique(release_id, message_id)
        );
        insert or ignore into articles_new
            (id, release_id, message_id, subject, filename, part, total_parts, bytes, file_total)
            select id, release_id, message_id, subject, filename, part, total_parts, bytes, file_total from articles;
        drop table articles;
        alter table articles_new rename to articles;
        commit;
        ",
    )
    .inspect_err(|_| {
        let _ = conn.execute_batch("rollback");
    })
}

/// Save releases from outside the indexer (AI search), each into its group's
/// shard. Refused while the database is being compacted, and holds off a
/// compaction until saved.
pub fn save_releases_bulk(releases: &[Release]) -> anyhow::Result<()> {
    if releases.is_empty() {
        return Ok(());
    }
    let main = paths::database();
    let _writing = crate::compact::hold_off_compaction(&main)?;
    Ok(store::save(&main, releases)?)
}

/// delete incomplete releases, returns bytes freed on disk. Refused while the
/// database is being compacted, and holds off a compaction until done.
pub fn purge_broken() -> anyhow::Result<i64> {
    let main = paths::database();
    let _writing = crate::compact::hold_off_compaction(&main)?;
    let size =
        || store::shard_paths(&main).iter().map(|p| fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)).sum::<i64>();
    let before = size();
    store::purge_incomplete(&main)?;
    Ok(before - size())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupState {
    pub live_cursor: i64,
    pub backfill_cursor: i64,
}

pub fn get_group_state(conn: &Connection, group: &str) -> Result<Option<GroupState>> {
    let row: Option<(Option<i64>, Option<i64>)> = conn
        .query_row("select live_cursor, backfill_cursor from groups where name = ?", [group], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;

    Ok(match row {
        Some((Some(live), Some(back))) => Some(GroupState { live_cursor: live, backfill_cursor: back }),
        _ => None,
    })
}

pub fn init_group_state(conn: &Connection, group: &str, cursor: i64) -> Result<()> {
    conn.execute(
        "insert into groups(name, live_cursor, backfill_cursor) values(?, ?, ?)
         on conflict(name) do update set
         live_cursor = excluded.live_cursor,
         backfill_cursor = excluded.backfill_cursor",
        params![group, cursor, cursor],
    )?;
    Ok(())
}

/// Remember the article numbers the server reported for a group (cursor key).
pub fn save_group_bounds(conn: &Connection, group: &str, first: i64, last: i64) -> Result<()> {
    conn.execute("update groups set first_article = ?, last_article = ? where name = ?", params![first, last, group])?;
    Ok(())
}

/// An article number and when the articles around it were posted (unix time).
pub type Dated = (i64, i64);

/// Widen the dated ends of what has been indexed on a group (cursor key): `low`
/// replaces the stored low end when it is further back, `high` the high end
/// when it is further ahead.
pub fn save_group_dates(conn: &Connection, group: &str, low: Dated, high: Dated) -> Result<()> {
    // every right hand side sees the row as it was, soo each pair moves together
    conn.execute(
        "update groups set
         low_article = case when low_article is null or ?1 < low_article then ?1 else low_article end,
         low_posted = case when low_article is null or ?1 < low_article then ?2 else low_posted end,
         high_article = case when high_article is null or ?3 > high_article then ?3 else high_article end,
         high_posted = case when high_article is null or ?3 > high_article then ?4 else high_posted end
         where name = ?5",
        params![low.0, low.1, high.0, high.1, group],
    )?;
    Ok(())
}

/// One row of indexing progress: cursors, the server's article range and the
/// dated ends of what has been indexed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupProgress {
    pub key: String,
    pub live_cursor: i64,
    pub backfill_cursor: i64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub low: Option<Dated>,
    pub high: Option<Dated>,
}

pub fn group_progress(conn: &Connection) -> Result<Vec<GroupProgress>> {
    let mut stmt = conn.prepare(
        "select name, live_cursor, backfill_cursor, first_article, last_article,
                low_article, low_posted, high_article, high_posted from groups
         where live_cursor is not null and backfill_cursor is not null",
    )?;
    let pair = |a: Option<i64>, b: Option<i64>| a.zip(b);
    stmt.query_map([], |r| {
        Ok(GroupProgress {
            key: r.get(0)?,
            live_cursor: r.get(1)?,
            backfill_cursor: r.get(2)?,
            first: r.get(3)?,
            last: r.get(4)?,
            low: pair(r.get(5)?, r.get(6)?),
            high: pair(r.get(7)?, r.get(8)?),
        })
    })?
    .collect()
}

pub fn save_group_state(conn: &Connection, group: &str, state: GroupState) -> Result<()> {
    conn.execute(
        "insert into groups(name, live_cursor, backfill_cursor) values(?, ?, ?)
         on conflict(name) do update set
         live_cursor = excluded.live_cursor,
         backfill_cursor = excluded.backfill_cursor",
        params![group, state.live_cursor, state.backfill_cursor],
    )?;
    Ok(())
}

pub fn update_live_cursor(conn: &Connection, group: &str, article: i64) -> Result<()> {
    conn.execute(
        "insert into groups(name, live_cursor, backfill_cursor) values(?, ?, ?)
         on conflict(name) do update set live_cursor = excluded.live_cursor",
        params![group, article, article],
    )?;
    Ok(())
}

pub fn update_backfill_cursor(conn: &Connection, group: &str, article: i64) -> Result<()> {
    conn.execute(
        "insert into groups(name, live_cursor, backfill_cursor) values(?, ?, ?)
         on conflict(name) do update set backfill_cursor = excluded.backfill_cursor",
        params![group, article, article],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn group_dates_only_ever_widen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atlas.db");
        create_db_at(&path).unwrap();
        let conn = open_at(&path).unwrap();
        init_group_state(&conn, "g@news.x", 1000).unwrap();
        save_group_bounds(&conn, "g@news.x", 1, 1000).unwrap();
        let ends = || {
            let p = group_progress(&conn).unwrap().remove(0);
            (p.low, p.high)
        };
        assert_eq!(ends(), (None, None));

        // first pass: the newest slice
        save_group_dates(&conn, "g@news.x", (900, 9_000), (990, 9_900)).unwrap();
        assert_eq!(ends(), (Some((900, 9_000)), Some((990, 9_900))));

        // backfill reaches further back: only the low end moves, with its date
        save_group_dates(&conn, "g@news.x", (500, 5_000), (800, 8_000)).unwrap();
        assert_eq!(ends(), (Some((500, 5_000)), Some((990, 9_900))));

        // a live pass: only the high end moves
        save_group_dates(&conn, "g@news.x", (995, 9_950), (1000, 10_000)).unwrap();
        assert_eq!(ends(), (Some((500, 5_000)), Some((1000, 10_000))));
    }

    fn art(file: &str, part: i64, total: i64, file_total: Option<i64>, id: &str) -> crate::parser::Article {
        crate::parser::Article {
            message_id: format!("<{id}@x>"),
            subject: format!("\"{file}\" yEnc ({part}/{total})"),
            filename: Some(file.into()),
            part: Some(part),
            total_parts: Some(total),
            file_total,
            bytes: 100,
            ..Default::default()
        }
    }

    fn slice(name: &str, articles: Vec<crate::parser::Article>) -> Vec<Release> {
        vec![Release { name: name.into(), group: "alt.binaries.t".into(), articles, ..Default::default() }]
    }

    /// (size, complete, parts) of a release, from its shard
    fn stats(main: &Path, name: &str) -> (i64, bool, i64) {
        let conn = open_at(&store::shard_path(main, store::shard_of("alt.binaries.t"))).unwrap();
        conn.query_row("select size, complete, parts from releases where name = ?", [name], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? == 1, r.get(2)?))
        })
        .unwrap()
    }

    #[test]
    fn stats_build_up_over_slices() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        let save = |releases: &[Release]| store::save(&main, releases).unwrap();

        // a.rar has 3 parts, b.rar 2, the post says 2 files
        save(&slice("Rel", vec![art("a.rar", 1, 3, Some(2), "a1"), art("a.rar", 2, 3, Some(2), "a2")]));
        assert_eq!(stats(&main, "Rel"), (200, false, 2));

        let second = slice(
            "Rel",
            vec![
                art("a.rar", 3, 3, Some(2), "a3"),
                art("b.rar", 1, 2, Some(2), "b1"),
                art("b.rar", 2, 2, Some(2), "b2"),
            ],
        );
        save(&second);
        assert_eq!(stats(&main, "Rel"), (500, true, 5));

        // the same articles again change nothing
        save(&second);
        assert_eq!(stats(&main, "Rel"), (500, true, 5));

        // a repost of a part under a new message id doesnt break completeness
        save(&slice("Rel", vec![art("b.rar", 2, 2, Some(2), "b2-repost")]));
        assert_eq!(stats(&main, "Rel"), (600, true, 6));

        // a third file when the post said 2 makes it incomplete
        save(&slice("Rel", vec![art("c.rar", 1, 1, Some(2), "c1")]));
        assert!(!stats(&main, "Rel").1);

        // the running totals and the nzb
        let conn = open_at(&main).unwrap();
        store::attach(&conn, &main).unwrap();
        assert_eq!(store::totals(&conn).unwrap(), (1, 7));
        let id: i64 = conn
            .query_row(&format!("select id from s{}.releases", store::shard_of("alt.binaries.t")), [], |r| r.get(0))
            .unwrap();
        let rows = store::articles(&conn, id).unwrap();
        let order: Vec<(String, i64, String)> =
            rows.iter().map(|r| (r.filename.clone().unwrap(), r.part.unwrap(), r.message_id.clone())).collect();
        assert_eq!(order[0], ("a.rar".into(), 1, "<a1@x>".into()));
        assert_eq!(order[4], ("b.rar".into(), 2, "<b2-repost@x>".into()), "ties in message-id order");
        assert_eq!(rows.iter().map(|r| r.bytes.unwrap()).sum::<i64>(), 700);
    }

    #[test]
    fn stats_updates_leave_the_search_index_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atlas.s0.db");
        store::create_shard(&path).unwrap();
        let conn = open_at(&path).unwrap();
        let hits = |q: &str| -> i64 {
            conn.query_row("select count(*) from releases_fts where releases_fts match ?", [q], |r| r.get(0)).unwrap()
        };

        conn.execute("insert into releases (id, name, group_name) values (8, 'Some.Thing', 'g')", []).unwrap();
        let changes =
            |conn: &Connection| -> i64 { conn.query_row("select total_changes()", [], |r| r.get(0)).unwrap() };

        let before = changes(&conn);
        conn.execute("update releases set size = 5, parts = 1, complete = 1 where name = 'Some.Thing'", []).unwrap();
        assert_eq!(changes(&conn) - before, 1, "only the release row changes, no fts rows");

        conn.execute("update releases set display_name = 'Real.Name' where name = 'Some.Thing'", []).unwrap();
        assert_eq!(hits("\"Real\"*"), 1);
        assert_eq!(hits("\"Some\"*"), 1);
    }

    #[test]
    fn new_databases_get_shards_old_ones_wait_for_the_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        assert!(store::exists(&main));
        assert!(!has_old_layout(&open_at(&main).unwrap()).unwrap());

        let old = dir.path().join("old.db");
        create_at(&old).unwrap().execute_batch("create table releases (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
        create_db_at(&old).unwrap();
        assert!(has_old_layout(&open_at(&old).unwrap()).unwrap());
        assert!(!store::shard_path(&old, 0).exists(), "no shards until it is converted");
    }

    /// releases in every shard, and the id counter
    fn held(main: &Path) -> (i64, Option<i64>) {
        let releases = store::shard_paths(main)
            .iter()
            .map(|p| {
                open_at(p).unwrap().query_row("select count(*) from releases", [], |r| r.get::<_, i64>(0)).unwrap()
            })
            .sum();
        (releases, store::get_meta(&open_at(main).unwrap(), "next_seq").unwrap())
    }

    #[test]
    fn a_wipe_clears_every_shard_and_restarts_the_ids() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        let releases: Vec<Release> = ["alt.binaries.a", "alt.binaries.b", "alt.binaries.c"]
            .iter()
            .map(|g| Release { name: format!("Old {g}"), group: g.to_string(), ..Default::default() })
            .collect();
        store::save(&main, &releases).unwrap();
        init_group_state(&open_at(&main).unwrap(), "alt.binaries.a@news.x", 500).unwrap();
        let (before, next) = held(&main);
        assert_eq!(before, 3);
        assert!(next.unwrap() > 1);

        // what a compaction leaves behind, and the sidecars of a shard
        let shard = store::shard_path(&main, 2);
        let leftovers = [
            crate::compact::with_suffix(&shard, "compact"),
            crate::compact::with_suffix(&shard, "precompact"),
            // a compaction's scratch: the domain counts and a release check
            crate::compact::with_suffix(&shard, "domains"),
            PathBuf::from(format!("{}-wal", crate::compact::with_suffix(&shard, "domains").display())),
            crate::compact::with_suffix(&crate::compact::with_suffix(&shard, "compact"), "check"),
            PathBuf::from(format!(
                "{}-journal",
                crate::compact::with_suffix(&crate::compact::with_suffix(&shard, "compact"), "check").display()
            )),
            PathBuf::from(format!("{}-wal", shard.display())),
            PathBuf::from(format!("{}-shm", shard.display())),
        ];
        for f in &leftovers {
            std::fs::write(f, b"old").unwrap();
        }

        // the indexer, or a compaction, using the database: nothing is wiped
        let writing = crate::compact::try_hold_off_compaction(&main).unwrap().unwrap();
        let err = wipe(&main).unwrap_err();
        assert!(err.downcast_ref::<crate::compact::Busy>().is_some(), "{err:#}");
        assert_eq!(held(&main).0, 3, "refused, all still there");
        drop(writing);

        wipe(&main).unwrap();
        for f in leftovers.iter().chain(&store::shard_paths(&main)) {
            assert!(!f.exists(), "{} is gone", f.display());
        }
        assert!(!main.exists());

        create_db_at(&main).unwrap();
        assert_eq!(held(&main), (0, Some(1)), "no releases, ids from the start");
        assert!(group_progress(&open_at(&main).unwrap()).unwrap().is_empty(), "no cursors");
    }

    /// a conversion's atlas.old.db (kept after it) or atlas.new.db (a swap
    /// undone) would be put back as the main database at the next start
    #[test]
    fn a_wipe_takes_a_conversions_leftovers_too() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        init_group_state(&open_at(&main).unwrap(), "alt.binaries.a@news.x", 500).unwrap();
        let mut leftovers = Vec::new();
        for name in ["atlas.old.db", "atlas.new.db"] {
            let db = dir.path().join(name);
            std::fs::copy(&main, &db).unwrap();
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", db.display()));
                std::fs::write(&sidecar, b"").unwrap();
                leftovers.push(sidecar);
            }
            leftovers.push(db);
        }

        wipe(&main).unwrap();
        for f in &leftovers {
            assert!(!f.exists(), "{} is gone", f.display());
        }
        create_db_holding(&main).unwrap();
        assert!(group_progress(&open_at(&main).unwrap()).unwrap().is_empty(), "nothing came back");
    }

    #[test]
    fn a_wipe_that_cant_remove_a_shard_keeps_the_main_database() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        init_group_state(&open_at(&main).unwrap(), "alt.binaries.a@news.x", 500).unwrap();

        // a shard that cant be removed (on Windows one still open, here a folder)
        let stuck = store::shard_paths(&main).pop().unwrap();
        std::fs::remove_file(&stuck).unwrap();
        std::fs::create_dir(&stuck).unwrap();
        let err = wipe(&main).expect_err("a shard was left");
        assert!(format!("{err:#}").contains(&stuck.display().to_string()), "{err:#}");
        assert!(main.exists(), "the main database stays while a shard is left");
        assert_eq!(group_progress(&open_at(&main).unwrap()).unwrap().len(), 1, "with its cursors");

        std::fs::remove_dir(&stuck).unwrap();
        wipe(&main).unwrap();
        assert!(!main.exists());
    }

    #[test]
    fn a_used_main_database_without_any_shard_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        init_group_state(&open_at(&main).unwrap(), "alt.binaries.a@news.x", 500).unwrap();
        let shards = store::shard_paths(&main);
        for shard in &shards {
            std::fs::remove_file(shard).unwrap();
        }

        // its cursors are past what the empty shards would hold
        let err = create_db_at(&main).expect_err("not a new install");
        let err = format!("{err:#}");
        assert!(shards.iter().all(|s| err.contains(&s.display().to_string())), "{err}");
        assert!(shards.iter().all(|s| !s.exists()), "no empty shards made");

        // the same when only the ids were handed out
        std::fs::remove_file(&main).unwrap();
        create_db_at(&main).unwrap();
        store::set_next_seq(&open_at(&main).unwrap(), 50).unwrap();
        for shard in &shards {
            std::fs::remove_file(shard).unwrap();
        }
        create_db_at(&main).expect_err("ids were handed out");
    }

    #[test]
    fn shards_without_their_main_database_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        let release =
            crate::parser::Release { name: "Kept".into(), group: "alt.binaries.a".into(), ..Default::default() };
        store::save(&main, &[release]).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", main.display()));
        }

        // a new main database would hand the shards' ids out again
        let err = format!("{:#}", create_db_at(&main).expect_err("the shards are in use"));
        let shard = store::shard_path(&main, store::shard_of("alt.binaries.a"));
        assert!(err.contains(&main.display().to_string()) && err.contains(&shard.display().to_string()), "{err}");
        assert!(!main.exists(), "no new main database made");
    }

    #[test]
    fn a_main_database_with_nothing_in_it_gets_its_shards() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        create_db_at(&main).unwrap();
        for shard in store::shard_paths(&main) {
            std::fs::remove_file(shard).unwrap();
        }
        // a setup cut short before the shards
        create_db_at(&main).unwrap();
        assert!(store::shard_paths(&main).iter().all(|s| s.exists()));
    }
}
