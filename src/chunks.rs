//! Day chunks of a group's backfill that any server carrying the group can
//! take (see docs/superpowers/specs/2026-10-05-faster-backfill-smaller-storage-design.md).
//! Lives in the main database.

use rusqlite::{Connection, OptionalExtension, Result, params};

/// a claim older than this is taken over (its worker died or was stopped)
pub const CLAIM_TIMEOUT: i64 = 30 * 60;

const PENDING: i64 = 0;
const CLAIMED: i64 = 1;
const DONE: i64 = 2;

/// Initialize the backfill_chunks table if it doesn't exist, and give one
/// made before chunks kept their finish time its done_at column. Also the
/// server that goes back furthest, by split group (see `deepest`), and the
/// servers whose sweep of a split group is done (see `swept`).
pub fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "create table if not exists backfill_chunks (
            grp TEXT NOT NULL,
            day INTEGER NOT NULL,
            state INTEGER NOT NULL,
            server TEXT,
            claimed_at INTEGER,
            done_at INTEGER,
            primary key (grp, day)
        ) without rowid;
        create table if not exists backfill_deepest (
            grp TEXT PRIMARY KEY,
            server TEXT NOT NULL,
            oldest_at INTEGER,
            runner_up_at INTEGER
        ) without rowid;
        create table if not exists backfill_sweeps (
            grp TEXT NOT NULL,
            server TEXT NOT NULL,
            primary key (grp, server)
        ) without rowid;
        create table if not exists backfill_sweep_gen (
            grp TEXT PRIMARY KEY,
            gen INTEGER NOT NULL
        ) without rowid;
        create table if not exists backfill_speculative (
            grp TEXT PRIMARY KEY,
            runner_up_day INTEGER NOT NULL,
            reach INTEGER NOT NULL,
            days INTEGER NOT NULL,
            articles INTEGER NOT NULL
        ) without rowid;",
    )?;
    add_column(conn, "backfill_chunks", "done_at")?;
    add_column(conn, "backfill_deepest", "oldest_at")?;
    add_column(conn, "backfill_deepest", "runner_up_at")
}

/// Give `table` the INTEGER column `column` if a table made before it lacks it.
fn add_column(conn: &Connection, table: &str, column: &str) -> Result<()> {
    let has =
        || conn.prepare(&format!("select 1 from pragma_table_info('{table}') where name = '{column}'"))?.exists([]);
    if has()? {
        return Ok(());
    }
    match conn.execute_batch(&format!("alter table {table} add column {column} INTEGER")) {
        // the indexer and the menu can both add it at once
        Err(_) if has()? => Ok(()),
        r => r,
    }
}

/// days since 1970-01-01 UTC
pub fn unix_day(t: i64) -> i64 {
    t.div_euclid(86_400)
}

/// Pending chunks for `newest_day` down to `oldest_day`, keeping any that exist.
pub fn add(conn: &Connection, group: &str, newest_day: i64, oldest_day: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut added = 0;
    {
        let mut insert =
            tx.prepare_cached("insert or ignore into backfill_chunks (grp, day, state) values (?, ?, ?)")?;
        for day in (oldest_day..=newest_day).rev() {
            added += insert.execute(params![group, day, PENDING])?;
        }
    }
    tx.commit()?;
    Ok(added)
}

/// When a server goes back further than any did on split `group`, to
/// `oldest_at` (unix seconds) in `oldest_day`: chunks for the days from
/// before the split's oldest one back to `oldest_day`. The old oldest day
/// was done (if it was) only from where the server going back furthest then
/// started: it is to do again, and that server is forgotten (see `deepest`).
/// A claim on it is cleared too, so its worker's finish does nothing, and
/// the carriers' sweeps are to do again.
/// Also when `oldest_day` is the oldest day but `oldest_at` is before where
/// that server started in it. Returns the chunks added or to do again.
pub fn reach_back(conn: &Connection, group: &str, oldest_day: i64, oldest_at: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let Some(old) = self::oldest_day(&tx, group)? else { return Ok(0) };
    // never past the window of days only the deepest server has
    let spec = speculative(&tx, group)?;
    let floor = spec.as_ref().map_or(i64::MIN, |s| s.runner_up_day - s.reach);
    if oldest_day < floor {
        if floor >= old {
            return Ok(0);
        }
        return add_speculative(&tx, group, old, floor).and_then(|n| tx.commit().map(|_| n));
    }
    if oldest_day > old {
        return Ok(0);
    }
    if spec.as_ref().is_some_and(|s| old <= s.runner_up_day) && oldest_day < old {
        // only days the deepest server alone has: added, nothing redone
        return add_speculative(&tx, group, old, oldest_day).and_then(|n| tx.commit().map(|_| n));
    }
    if oldest_day == old {
        let since: Option<i64> = tx
            .query_row("select oldest_at from backfill_deepest where grp = ?", [group], |r| r.get(0))
            .optional()?
            .flatten();
        if since.is_none_or(|since| oldest_at >= since) {
            return Ok(0);
        }
    }
    let mut added = 0;
    {
        let mut insert =
            tx.prepare_cached("insert or ignore into backfill_chunks (grp, day, state) values (?, ?, ?)")?;
        for day in (oldest_day..old).rev() {
            added += insert.execute(params![group, day, PENDING])?;
        }
    }
    let redo = tx.execute(
        &format!(
            "update backfill_chunks set state = {PENDING}, server = null, claimed_at = null, done_at = null
             where grp = ? and day = ? and state in ({DONE}, {CLAIMED})"
        ),
        params![group, old],
    )?;
    tx.execute("delete from backfill_deepest where grp = ?", [group])?;
    // a carrier's sweep stopped at its first article then: it may keep older ones now
    tx.execute("delete from backfill_sweeps where grp = ?", [group])?;
    // and a sweep that started before this doesnt get to note itself done (see `set_swept`)
    tx.execute(
        "insert into backfill_sweep_gen (grp, gen) values (?, 1) on conflict (grp) do update set gen = gen + 1",
        [group],
    )?;
    tx.commit()?;
    Ok(added + redo)
}

/// Chunks for the days before `old` back to `from`, all of them days only
/// the deepest server has: they dont change what any sweep covers.
fn add_speculative(tx: &Connection, group: &str, old: i64, from: i64) -> Result<usize> {
    let mut insert = tx.prepare_cached("insert or ignore into backfill_chunks (grp, day, state) values (?, ?, ?)")?;
    let mut added = 0;
    for day in (from..old).rev() {
        added += insert.execute(params![group, day, PENDING])?;
    }
    Ok(added)
}

/// A split's days older than the next deepest server goes back stand on the
/// deepest one's own (forgeable) Date headers. At most `reach` of them,
/// counted back from `runner_up_day` (the first day the next deepest
/// keeps), are chunks at once; the older ones are left to the deep server's
/// sweep. `days` of them were done holding `articles` dated in them. Kept
/// per group in the database, soo a restart or `reach_back` doesnt undo it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Speculative {
    pub runner_up_day: i64,
    pub reach: i64,
    pub days: i64,
    pub articles: i64,
}

pub fn speculative(conn: &Connection, group: &str) -> Result<Option<Speculative>> {
    conn.query_row(
        "select runner_up_day, reach, days, articles from backfill_speculative where grp = ?",
        [group],
        |r| Ok(Speculative { runner_up_day: r.get(0)?, reach: r.get(1)?, days: r.get(2)?, articles: r.get(3)? }),
    )
    .optional()
}

pub fn set_speculative(conn: &Connection, group: &str, s: &Speculative) -> Result<()> {
    conn.execute(
        "insert or replace into backfill_speculative (grp, runner_up_day, reach, days, articles) values (?, ?, ?, ?, ?)",
        params![group, s.runner_up_day, s.reach, s.days, s.articles],
    )?;
    Ok(())
}

/// Drop split `group`'s chunks older than `day` that arent done: days no
/// carrier keeps (a split reaching back that far was made from forged
/// dates), which would wait for a server forever and keep the split from
/// completing. Returns the chunks dropped.
pub fn drop_before(conn: &Connection, group: &str, day: i64) -> Result<usize> {
    conn.execute(
        &format!("delete from backfill_chunks where grp = ? and day < ? and state != {DONE}"),
        params![group, day],
    )
}

/// Check if a group has any chunks in backfill.
pub fn is_split(conn: &Connection, group: &str) -> Result<bool> {
    conn.prepare_cached("select 1 from backfill_chunks where grp = ? limit 1")?.exists([group])
}

/// A claimed chunk. Its server and claim time tell this claim from a later
/// one of the same chunk: a chunk that ran past CLAIM_TIMEOUT and was taken
/// over is the new owner's, finishing or giving back the old claim does nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub group: String,
    pub day: i64,
    pub server: String,
    pub claimed_at: i64,
}

/// Atomically claim the newest chunk that is pending or whose claim went
/// stale, of `groups`: (group, oldest day the server keeps), `i64::MIN` when
/// any day will do.
pub fn claim(conn: &Connection, groups: &[(String, i64)], host: &str, now: i64) -> Result<Option<Claim>> {
    if groups.is_empty() {
        return Ok(None);
    }
    let rows = vec!["(?, ?)"; groups.len()].join(", ");
    let sql = format!(
        "with wanted (g, oldest) as (values {rows})
         update backfill_chunks set state = ?, server = ?, claimed_at = ?
         where (grp, day) = (select c.grp, c.day from backfill_chunks c join wanted w on w.g = c.grp
                             where c.day >= w.oldest
                               and (c.state = {PENDING} or (c.state = {CLAIMED} and c.claimed_at < ?))
                             order by c.day desc limit 1)
         returning grp, day"
    );
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    for (g, oldest) in groups {
        values.push(g.clone().into());
        values.push((*oldest).into());
    }
    values.extend([CLAIMED.into(), host.to_string().into(), now.into()]);
    values.push((now - CLAIM_TIMEOUT).into());
    let found: Option<(String, i64)> =
        conn.prepare(&sql)?.query_row(rusqlite::params_from_iter(values), |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(found.map(|(group, day)| Claim { group, day, server: host.to_string(), claimed_at: now }))
}

/// Mark a claimed chunk as done at `now`. False when the claim was taken over.
pub fn finish(conn: &Connection, claim: &Claim, now: i64) -> Result<bool> {
    let n = conn.execute(
        &format!(
            "update backfill_chunks set state = ?, done_at = ?
             where grp = ? and day = ? and state = {CLAIMED} and server = ? and claimed_at = ?"
        ),
        params![DONE, now, claim.group, claim.day, claim.server, claim.claimed_at],
    )?;
    Ok(n > 0)
}

/// Return a claimed chunk to pending state. False when the claim was taken over.
pub fn release(conn: &Connection, claim: &Claim) -> Result<bool> {
    let n = conn.execute(
        &format!(
            "update backfill_chunks set state = ?, server = null, claimed_at = null
             where grp = ? and day = ? and state = {CLAIMED} and server = ? and claimed_at = ?"
        ),
        params![PENDING, claim.group, claim.day, claim.server, claim.claimed_at],
    )?;
    Ok(n > 0)
}

/// (done, total) chunks of a group
pub fn progress(conn: &Connection, group: &str) -> Result<(i64, i64)> {
    conn.query_row("select coalesce(sum(state = 2), 0), count(*) from backfill_chunks where grp = ?", [group], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
}

/// the oldest day of a group's chunks
pub fn oldest_day(conn: &Connection, group: &str) -> Result<Option<i64>> {
    conn.query_row("select min(day) from backfill_chunks where grp = ?", [group], |r| r.get(0))
}

/// the newest day of a group's chunks
pub fn newest_day(conn: &Connection, group: &str) -> Result<Option<i64>> {
    conn.query_row("select max(day) from backfill_chunks where grp = ?", [group], |r| r.get(0))
}

/// The server whose first article of a split group is the oldest: the one
/// to index the split's oldest day, which no server keeps whole.
pub fn deepest(conn: &Connection, group: &str) -> Result<Option<String>> {
    conn.query_row("select server from backfill_deepest where grp = ?", [group], |r| r.get(0)).optional()
}

/// Note `server` as the one going back furthest on `group`, from `oldest_at`
/// (when its first article was posted, unix seconds).
pub fn set_deepest(conn: &Connection, group: &str, server: &str, oldest_at: i64) -> Result<()> {
    conn.execute(
        "insert or replace into backfill_deepest (grp, server, oldest_at) values (?, ?, ?)",
        params![group, server, oldest_at],
    )?;
    Ok(())
}

/// Note how far back the server going back second furthest on `group`
/// goes (unix seconds): days before it are the furthest one's alone.
pub fn set_runner_up(conn: &Connection, group: &str, runner_up_at: i64) -> Result<()> {
    conn.execute("update backfill_deepest set runner_up_at = ? where grp = ?", params![runner_up_at, group])?;
    Ok(())
}

/// How far back the server going back second furthest on `group` goes
/// (unix seconds), when noted with the furthest one.
pub fn runner_up(conn: &Connection, group: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("select runner_up_at from backfill_deepest where grp = ?", [group], |r| r.get(0))
        .optional()?
        .flatten())
}

/// Forget `server` as the one going back furthest on `group`, when it no
/// longer carries it or no longer keeps the oldest day: the next to reach
/// the oldest day asks again.
pub fn forget_deepest(conn: &Connection, group: &str, server: &str) -> Result<()> {
    conn.execute("delete from backfill_deepest where grp = ? and server = ?", [group, server])?;
    Ok(())
}

/// Whether any chunk of `groups`: (group, oldest day the server keeps) is
/// still to do (pending or claimed) on a day the server keeps.
pub fn waiting(conn: &Connection, groups: &[(String, i64)]) -> Result<bool> {
    let mut stmt = conn.prepare_cached("select 1 from backfill_chunks where grp = ? and day >= ? and state != 2")?;
    for (g, oldest) in groups {
        if stmt.exists(params![g, oldest])? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether every day chunk of split `group` is done.
pub fn chunks_done(conn: &Connection, group: &str) -> Result<bool> {
    Ok(is_split(conn, group)?
        && !conn.prepare_cached("select 1 from backfill_chunks where grp = ? and state != 2")?.exists([group])?)
}

/// The split generation of `group`: counts the times its split reached back
/// (see `reach_back`), which undoes what a sweep covered.
pub fn sweep_generation(conn: &Connection, group: &str) -> Result<i64> {
    Ok(conn
        .query_row("select gen from backfill_sweep_gen where grp = ?", [group], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}

/// Note `server`'s sweep of split `group` as done: its cursor backfill got
/// down to its first article after the day chunks (see `indexer::run_sweep`).
/// `generation` is the group's (see `sweep_generation`) when the sweep was
/// chosen: one from before the split reached back is stale and notes nothing.
pub fn set_swept(conn: &Connection, group: &str, server: &str, generation: i64) -> Result<()> {
    conn.execute(
        "insert or ignore into backfill_sweeps (grp, server)
         select ?1, ?2 where coalesce((select gen from backfill_sweep_gen where grp = ?1), 0) = ?3",
        params![group, server, generation],
    )?;
    Ok(())
}

/// Whether `server`'s sweep of split `group` is done.
pub fn swept(conn: &Connection, group: &str, server: &str) -> Result<bool> {
    conn.prepare_cached("select 1 from backfill_sweeps where grp = ? and server = ?")?.exists([group, server])
}

/// Whether split `group` is complete: every day chunk done and every one of
/// `servers` (the ones carrying it) swept down to its first article.
pub fn complete(conn: &Connection, group: &str, servers: &[String]) -> Result<bool> {
    if !chunks_done(conn, group)? {
        return Ok(false);
    }
    for s in servers {
        if !swept(conn, group, s)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// groups with chunks still to do
pub fn split_groups(conn: &Connection) -> Result<Vec<String>> {
    conn.prepare("select distinct grp from backfill_chunks where state != 2 order by grp")?
        .query_map([], |r| r.get(0))?
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        create(&c).unwrap();
        c
    }

    /// the second furthest server's retention is kept with the furthest,
    /// and goes with it
    #[test]
    fn the_runner_up_is_kept_with_the_deepest() {
        let c = conn();
        set_deepest(&c, "g", "a", 100).unwrap();
        assert_eq!(runner_up(&c, "g").unwrap(), None);
        set_runner_up(&c, "g", 200).unwrap();
        assert_eq!(runner_up(&c, "g").unwrap(), Some(200));
        forget_deepest(&c, "g", "a").unwrap();
        assert_eq!(runner_up(&c, "g").unwrap(), None);
    }

    #[test]
    fn reaching_back_keeps_to_the_speculative_window() {
        let c = conn();
        // days 100.. the next deepest keeps, 40..99 the window, older pruned
        add(&c, "g", 120, 40).unwrap();
        set_speculative(&c, "g", &Speculative { runner_up_day: 100, reach: 60, days: 3, articles: 7 }).unwrap();
        set_swept(&c, "g", "a", 0).unwrap();
        // the hourly look finds the deep server back in 2000 again: nothing
        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400).unwrap(), 0);
        assert_eq!(oldest_day(&c, "g").unwrap(), Some(40));
        assert!(swept(&c, "g", "a").unwrap(), "the sweep isnt to do again");
        // the window kept across a restart
        assert_eq!(speculative(&c, "g").unwrap().map(|s| (s.reach, s.days, s.articles)), Some((60, 3, 7)));
        // a wider window: the days in it, sweeps left alone
        set_speculative(&c, "g", &Speculative { runner_up_day: 100, reach: 70, days: 3, articles: 7 }).unwrap();
        assert_eq!(reach_back(&c, "g", 5, 5 * 86_400).unwrap(), 10);
        assert_eq!(oldest_day(&c, "g").unwrap(), Some(30));
        assert!(swept(&c, "g", "a").unwrap());
    }

    /// the day of a claim
    fn day(claimed: Result<Option<Claim>>) -> Option<i64> {
        claimed.unwrap().map(|c| c.day)
    }

    #[test]
    fn days_are_claimed_newest_first_and_once() {
        let c = conn();
        assert_eq!(add(&c, "g", 20_000, 19_998).unwrap(), 3);
        assert_eq!(add(&c, "g", 20_000, 19_998).unwrap(), 0, "adding again adds nothing");
        assert!(is_split(&c, "g").unwrap());
        let groups = vec![("g".to_string(), i64::MIN)];

        let a = claim(&c, &groups, "a", 1000).unwrap().unwrap();
        assert_eq!(a, Claim { group: "g".into(), day: 20_000, server: "a".into(), claimed_at: 1000 });
        let b = claim(&c, &groups, "b", 1000).unwrap().unwrap();
        assert_eq!(b.day, 19_999);
        assert!(finish(&c, &a, 1100).unwrap());
        assert!(release(&c, &b).unwrap());
        assert_eq!(day(claim(&c, &groups, "b", 1000)), Some(19_999), "released goes back");
        assert_eq!(day(claim(&c, &groups, "a", 1000)), Some(19_998));
        assert_eq!(claim(&c, &groups, "a", 1000).unwrap(), None);
        assert_eq!(progress(&c, "g").unwrap(), (1, 3));
    }

    #[test]
    fn the_oldest_day_is_redone_when_retention_reaches_further_into_it() {
        let c = conn();
        let done = |c: &Connection| {
            c.execute("update backfill_chunks set state = 2, server = 'a', done_at = 1 where day = 10", []).unwrap()
        };
        let state = |c: &Connection| -> i64 {
            c.query_row("select state from backfill_chunks where day = 10", [], |r| r.get(0)).unwrap()
        };
        add(&c, "g", 12, 10).unwrap();
        done(&c);
        // "a" went back furthest, to 18:00 on day 10
        set_deepest(&c, "g", "a", 10 * 86_400 + 18 * 3600).unwrap();

        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400 + 18 * 3600).unwrap(), 0, "no further");
        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400 + 20 * 3600).unwrap(), 0, "less far");
        assert_eq!((state(&c), deepest(&c, "g").unwrap().as_deref()), (2, Some("a")));

        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400 + 3600).unwrap(), 1, "01:00 the same day");
        assert_eq!((state(&c), deepest(&c, "g").unwrap()), (0, None), "to do again, deepest asked again");

        // a deepest noted before its time was: nothing to compare, left alone
        done(&c);
        c.execute("insert into backfill_deepest (grp, server) values ('g', 'a')", []).unwrap();
        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400).unwrap(), 0);
        assert_eq!(state(&c), 2);
        // a day before it still reaches back
        assert_eq!(reach_back(&c, "g", 9, 9 * 86_400).unwrap(), 2, "day 9 added, day 10 to do again");
    }

    #[test]
    fn a_claimed_oldest_day_is_reset_when_retention_reaches_further_into_it() {
        let c = conn();
        add(&c, "g", 12, 10).unwrap();
        let groups = vec![("g".to_string(), i64::MIN)];
        // a takes day 12, then day 11, then day 10 (the split's oldest), as the deepest
        for _ in 0..2 {
            claim(&c, &groups, "a", 1000).unwrap().unwrap();
        }
        let a = claim(&c, &groups, "a", 1000).unwrap().unwrap();
        assert_eq!(a.day, 10);
        set_deepest(&c, "g", "a", 10 * 86_400 + 18 * 3600).unwrap();

        // b goes back further into day 10 while a is still on it
        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400 + 3600).unwrap(), 1);
        assert_eq!(deepest(&c, "g").unwrap(), None);
        let row: (i64, Option<String>, Option<i64>) = c
            .query_row("select state, server, claimed_at from backfill_chunks where day = 10", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(row, (PENDING, None, None), "pending again, a's claim gone");

        // a's late finish and release do nothing
        assert!(!finish(&c, &a, 2000).unwrap());
        assert!(!release(&c, &a).unwrap());
        assert_eq!(progress(&c, "g").unwrap(), (0, 3));
        // and the next to claim it is not a stale claim's
        assert_eq!(day(claim(&c, &groups, "b", 1001)), Some(10));
    }

    /// a carrier swept down to its first article before the split reached
    /// back sweeps again: its first article may be older now
    #[test]
    fn reaching_back_undoes_the_sweeps() {
        let c = conn();
        add(&c, "g", 12, 10).unwrap();
        c.execute("update backfill_chunks set state = 2", []).unwrap();
        set_deepest(&c, "g", "a", 10 * 86_400 + 18 * 3600).unwrap();
        set_swept(&c, "g", "b", 0).unwrap();
        set_swept(&c, "other", "b", 0).unwrap();

        assert_eq!(reach_back(&c, "g", 10, 10 * 86_400 + 20 * 3600).unwrap(), 0, "not further");
        assert!(swept(&c, "g", "b").unwrap(), "kept when nothing changed");

        assert_eq!(reach_back(&c, "g", 9, 9 * 86_400).unwrap(), 2);
        assert!(!swept(&c, "g", "b").unwrap(), "to sweep again");
        assert!(swept(&c, "other", "b").unwrap(), "other groups' sweeps stay");
    }

    /// a sweep chosen before the split reached back, finishing after, doesnt
    /// note itself done: it covered what is older now only down to the old first article
    #[test]
    fn a_sweep_from_before_reaching_back_is_not_noted() {
        let c = conn();
        add(&c, "g", 12, 10).unwrap();
        c.execute("update backfill_chunks set state = 2", []).unwrap();
        set_deepest(&c, "g", "a", 10 * 86_400 + 18 * 3600).unwrap();
        let started = sweep_generation(&c, "g").unwrap();

        assert_eq!(reach_back(&c, "g", 9, 9 * 86_400).unwrap(), 2);
        set_swept(&c, "g", "b", started).unwrap();
        assert!(!swept(&c, "g", "b").unwrap(), "stale, left to do");

        let now = sweep_generation(&c, "g").unwrap();
        assert_ne!(now, started);
        set_swept(&c, "g", "b", now).unwrap();
        assert!(swept(&c, "g", "b").unwrap(), "one started since is noted");
    }

    #[test]
    fn days_before_are_dropped_unless_done() {
        let c = conn();
        add(&c, "g", 12, 5).unwrap();
        add(&c, "h", 12, 5).unwrap();
        c.execute("update backfill_chunks set state = 2 where grp = 'g' and day = 6", []).unwrap();
        claim(&c, &[("g".to_string(), i64::MIN)], "a", 1000).unwrap();
        assert_eq!(drop_before(&c, "g", 8).unwrap(), 2, "days 5 and 7");
        assert_eq!(oldest_day(&c, "g").unwrap(), Some(6), "a done day stays");
        assert_eq!(progress(&c, "h").unwrap(), (0, 8), "other groups untouched");
    }

    #[test]
    fn stale_claims_are_taken_over() {
        let c = conn();
        add(&c, "g", 5, 5).unwrap();
        let groups = vec![("g".to_string(), i64::MIN)];
        assert!(claim(&c, &groups, "a", 1000).unwrap().is_some());
        assert_eq!(claim(&c, &groups, "b", 1000 + CLAIM_TIMEOUT - 1).unwrap(), None);
        assert_eq!(day(claim(&c, &groups, "b", 1000 + CLAIM_TIMEOUT + 1)), Some(5));
    }

    #[test]
    fn a_claim_taken_over_cant_be_finished_or_given_back() {
        let c = conn();
        add(&c, "g", 5, 5).unwrap();
        let groups = vec![("g".to_string(), i64::MIN)];
        // a's chunk runs past CLAIM_TIMEOUT, b takes it over and finishes it
        let a = claim(&c, &groups, "a", 1000).unwrap().unwrap();
        let b = claim(&c, &groups, "b", 1000 + CLAIM_TIMEOUT + 1).unwrap().unwrap();
        let done_at = 1000 + CLAIM_TIMEOUT + 60;
        assert!(finish(&c, &b, done_at).unwrap());

        // a's late release and finish change nothing: done, and b's
        assert!(!release(&c, &a).unwrap());
        assert!(!finish(&c, &a, done_at + 60).unwrap());
        let row = |c: &Connection| {
            c.query_row("select state, server, done_at from backfill_chunks where grp = 'g' and day = 5", [], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?))
            })
            .unwrap()
        };
        assert_eq!(row(&c), (DONE, Some("b".to_string()), Some(done_at)));
    }

    #[test]
    fn a_table_from_before_done_at_gets_the_column() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "create table backfill_chunks (grp TEXT NOT NULL, day INTEGER NOT NULL, state INTEGER NOT NULL,
                 server TEXT, claimed_at INTEGER, primary key (grp, day)) without rowid;
             insert into backfill_chunks values ('g', 5, 1, 'a', 1000);",
        )
        .unwrap();
        create(&c).unwrap();
        create(&c).unwrap();
        let a = Claim { group: "g".into(), day: 5, server: "a".into(), claimed_at: 1000 };
        assert!(finish(&c, &a, 1100).unwrap(), "the chunk claimed before is still there");
        let done_at: Option<i64> = c.query_row("select done_at from backfill_chunks", [], |r| r.get(0)).unwrap();
        assert_eq!(done_at, Some(1100));
    }

    #[test]
    fn only_listed_groups_and_unsplit_groups() {
        let c = conn();
        add(&c, "g", 5, 5).unwrap();
        assert_eq!(claim(&c, &[("other".to_string(), i64::MIN)], "a", 0).unwrap(), None);
        assert!(!is_split(&c, "other").unwrap());
        assert_eq!(split_groups(&c).unwrap(), vec!["g".to_string()]);
        assert_eq!(unix_day(86_400 * 3 + 5), 3);
    }

    #[test]
    fn days_older_than_a_servers_oldest_are_left_to_others() {
        let c = conn();
        add(&c, "g", 12, 10).unwrap();
        let from_11 = [("g".to_string(), 11)];
        assert_eq!(day(claim(&c, &from_11, "short", 0)), Some(12));
        assert_eq!(day(claim(&c, &from_11, "short", 0)), Some(11));
        assert_eq!(day(claim(&c, &from_11, "short", 0)), None, "day 10 is older than it keeps");
        assert_eq!(day(claim(&c, &[("g".to_string(), i64::MIN)], "long", 0)), Some(10));
    }

    #[test]
    fn claim_is_atomic_with_concurrent_connections() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // create db and add chunks
        let c1 = Connection::open(&db_path).unwrap();
        create(&c1).unwrap();
        add(&c1, "g", 20, 19).unwrap();
        c1.close().ok();

        // open two connections and claim
        let c_a = Connection::open(&db_path).unwrap();
        let c_b = Connection::open(&db_path).unwrap();

        let groups = vec![("g".to_string(), i64::MIN)];
        let a_claim = claim(&c_a, &groups, "a", 1000).unwrap();
        let b_claim = claim(&c_b, &groups, "b", 1000).unwrap();

        // they should get different days (newest first, then next)
        assert!(a_claim.is_some() && b_claim.is_some());
        assert_ne!(a_claim.as_ref().map(|c| c.day), b_claim.as_ref().map(|c| c.day));
        assert_eq!(a_claim.as_ref().map(|c| c.day), Some(20));
        assert_eq!(b_claim.as_ref().map(|c| c.day), Some(19));

        // third claim should get None
        let c_c = Connection::open(&db_path).unwrap();
        assert_eq!(claim(&c_c, &groups, "c", 1000).unwrap(), None);
    }
}
