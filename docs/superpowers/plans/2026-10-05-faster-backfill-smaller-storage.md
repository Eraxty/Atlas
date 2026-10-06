# Faster Backfill and Smaller Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split big groups' backfill across usenet servers by day, pack message-id locals by shape, seal finished files into zstd blobs, and add an opt-in 24 hour auto compaction.

**Architecture:** Day chunks of a split group live in a `backfill_chunks` table in `atlas.db`; idle workers on any carrying server claim one, map the day to that server's article numbers by binary search (`Pool::article_at`), and index it through the existing `process_range` path. Storage changes stay inside the shard layer (`src/store.rs` plus a new `src/blob.rs` codec): a new local-part packing, and a `blob` column on `files` that replaces a file's `segments` rows once it's complete or untouched for 3 days. `atlas --compact` gains re-encoding and sealing; the background indexer can run it every 24 hours.

**Tech Stack:** Rust 1.99 (edition 2024), tokio, rusqlite 0.40 (bundled SQLite), new dependency `zstd = "0.13"`.

**Spec:** `docs/superpowers/specs/2026-10-05-faster-backfill-smaller-storage-design.md`

## Global Constraints

- Rust toolchain 1.99.0 (pinned in `rust-toolchain.toml`). Run commands through `just` (`just test`, `just lint`, `just format`) so the pinned toolchain is used.
- `just lint` is `cargo clippy --all-targets --locked -- -D warnings`: no warnings allowed. Clippy lints seen in this repo: `manual_is_multiple_of`, `type_complexity` (use type aliases), `needless_borrow`, `unnecessary_cast`.
- `--locked`: adding the `zstd` dependency (Task 7) must update `Cargo.lock` in the same commit.
- Comment style: short lowercase-start comments, plain words, matching the surrounding code. Doc comments on public items.
- Never touch the user's live data: `atlas.db`, `atlas.s*.db`, `config.json`, `news.txt` in the repo root. Tests use `tempfile::tempdir()` and `ATLAS_HOME`; integration tests that set env vars are the only test in their binary.
- Commits: `git commit` as the configured user (xbmc4lyfe, SSH signed); end every message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Don't push.
- Constants (verbatim from the spec): `SPLIT_MIN_BACKLOG = 10_000_000`, `CHUNK_CLAIM_TIMEOUT = 30 min`, chunk overlap 1 hour, `SEAL_PER_TICK = 2_000`, seal age 3 days, zstd level 6, compact interval 24 hours, `auto_run_compact` default `false`.

## Model routing (for the controller)

| Task | Model | Why |
|---|---|---|
| 1 article_at | sonnet | protocol code + mock change, clear spec |
| 2 chunk table | haiku | plain SQL functions + unit tests |
| 3 run_chunk | sonnet | joins existing pass machinery |
| 4 scheduler split | opus | concurrency, scheduling, e2e test |
| 5 dashboard progress | haiku | small read-only change |
| 6 local packing | sonnet | codec with exactness requirements |
| 7 blob codec | sonnet | self-contained codec |
| 8 sealing in store | opus | schema, read path, dedupe correctness |
| 9 online sealing | sonnet | writer loop integration |
| 10 compact re-encode + seal | sonnet | extends existing tool + check |
| 11 auto_run_compact | sonnet | supervise loop + config |
| 12 docs + full verification | haiku | README + run the suite |

---

## Part A: split backfill by date

### Task 1: `Pool::article_at` (day to article number) and mock post dates

**Files:**
- Modify: `src/nntp.rs` (add `posted_at` and `article_at` to `impl Pool`, near `select_group_on`)
- Modify: `tests/common/mod.rs` (`Post` gets a `date`, the mock's XOVER uses it)
- Test: `tests/date_search.rs` (new)

**Interfaces:**
- Consumes: `Pool::xover_on(&self, i: usize, group: &str, start: u64, end: u64) -> Result<Vec<Overview>>` (private, same impl), `crate::dates::posted_timestamp(&str) -> Option<i64>`.
- Produces: `pub async fn article_at(&self, i: usize, group: &str, low: u64, high: u64, when: i64) -> Result<u64>`. Returns the first article number in `low..=high` posted at or after `when` (unix seconds), or `high + 1` if none. Also `pub fn post_at(number: u64, subject: &str, date: &str) -> Post` in `tests/common/mod.rs`.

- [ ] **Step 1: Give mock posts a date**

In `tests/common/mod.rs`, add `pub date: String` to `struct Post`. Update `post()` so every existing test keeps the old date:

```rust
pub fn post(number: u64, subject: &str, bytes: u64, body: Vec<Vec<u8>>) -> Post {
    Post {
        number,
        subject: subject.into(),
        message_id: format!("<msg{number}@mock>"),
        bytes,
        body,
        date: "Fri, 02 Oct 2026 10:11:12 +0000".into(),
    }
}

/// A post with its own date (rfc 2822), for date based tests.
pub fn post_at(number: u64, subject: &str, date: &str) -> Post {
    Post { date: date.into(), ..post(number, subject, 10, vec![]) }
}
```

In the XOVER row `format!`, replace the literal `Fri, 02 Oct 2026 10:11:12 +0000` with `{}` and pass `p.date`. Run `grep -rn "Post {" tests/` and fix any struct literal that now lacks `date`.

- [ ] **Step 2: Write the failing test**

Create `tests/date_search.rs`:

```rust
//! Finding the article posted at a given time by binary search over one
//! article XOVERs, with gaps in the numbering and slightly out of order dates.

mod common;

use atlas::nntp::BlockingPool;
use common::{GROUP, Server, mock, post_at, spawn_server};

/// posts 1..=1000, one per hour from 2026-01-01 00:00 UTC, with every 10th
/// number missing and number 500 dated an hour too early
fn hourly() -> Vec<common::Post> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    (1..=1000u64)
        .filter(|n| n % 10 != 0)
        .map(|n| {
            let hours = if n == 500 { n as i64 - 2 } else { n as i64 - 1 };
            let when = start + chrono::Duration::hours(hours);
            post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &when.to_rfc2822())
        })
        .collect()
}

#[test]
fn finds_the_first_article_at_or_after_a_time() {
    let port = spawn_server(Server::new(hourly()));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();
    let at = |rfc3339: &str| {
        let t = chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap().timestamp();
        pool.block_on(pool.pool.article_at(0, GROUP, 1, 1000, t)).unwrap()
    };

    assert_eq!(at("2025-12-01T00:00:00+00:00"), 1, "before everything: the first article");
    assert_eq!(at("2026-01-01T04:00:00+00:00"), 5, "exactly on an article");
    assert_eq!(at("2026-01-01T08:30:00+00:00"), 11, "number 10 is missing, 11 is next");
    assert_eq!(at("2026-03-01T00:00:00+00:00"), 1001, "after everything: high + 1");
}
```

- [ ] **Step 3: Run it to see it fail**

Run: `cargo test --test date_search`
Expected: compile error, `no method named article_at`.

- [ ] **Step 4: Implement**

In `src/nntp.rs`, inside `impl Pool`:

```rust
/// The first article at or after `number` within the next `look` numbers on
/// server `i`, with its post time: (number, unix seconds). None when there is
/// none (a gap in the numbering).
async fn posted_at(&self, i: usize, group: &str, number: u64, look: u64) -> Result<Option<(u64, i64)>> {
    match self.xover_on(i, group, number, number + look - 1).await {
        Ok(rows) => Ok(rows
            .iter()
            .filter_map(|r| crate::dates::posted_timestamp(&r.date).map(|t| (r.number, t)))
            .min_by_key(|(n, _)| *n)),
        Err(e) if e.code() == Some(423) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The first article number in `low..=high` on server `i` posted at or after
/// `when` (unix seconds), `high + 1` when there is none. A binary search over
/// one article requests (about 35 for a billion numbers). Post dates are
/// only roughly in order, soo the answer is approximate near the edges:
/// callers overlap their ranges.
pub async fn article_at(&self, i: usize, group: &str, low: u64, high: u64, when: i64) -> Result<u64> {
    const LOOK: u64 = 100;
    let (mut lo, mut hi) = (low, high + 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match self.posted_at(i, group, mid, LOOK).await? {
            Some((n, t)) if t < when => lo = n + 1,
            Some(_) => hi = mid,
            // a gap: nothing to compare, keep searching to the left of it
            None => hi = mid,
        }
    }
    Ok(lo)
}
```

`Overview` has a public `date` field and `number` (check with `grep -n "pub struct Overview" -A10 src/nntp.rs`). If the mock group's articles are not selected on the connection yet, `xover_on` already selects the group. Make sure `xover_on` is reachable from `article_at`; it's in the same `impl`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test date_search && just test`
Expected: PASS, and every other test still passes (the mock date default is unchanged).

- [ ] **Step 6: Commit**

```bash
git add src/nntp.rs tests/common/mod.rs tests/date_search.rs
git commit -m "Find the article posted at a time by binary search

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: `backfill_chunks` table and its functions

**Files:**
- Create: `src/chunks.rs`
- Modify: `src/lib.rs` (`pub mod chunks;`)
- Modify: `src/store.rs` (`create_main` creates the table)
- Test: unit tests in `src/chunks.rs`

**Interfaces:**
- Consumes: `rusqlite::Connection` on the main database (`atlas.db`).
- Produces (all `pub`, in `atlas::chunks`):
  - `pub const CLAIM_TIMEOUT: i64 = 30 * 60;`
  - `pub fn create(conn: &Connection) -> Result<()>`
  - `pub fn unix_day(t: i64) -> i64` (days since epoch, UTC)
  - `pub fn add(conn: &Connection, group: &str, newest_day: i64, oldest_day: i64) -> Result<usize>` (inserts pending days from newest to oldest that don't exist yet; returns how many were added)
  - `pub fn is_split(conn: &Connection, group: &str) -> Result<bool>`
  - `pub fn claim(conn: &Connection, groups: &[String], host: &str, now: i64) -> Result<Option<(String, i64)>>` (newest pending, or stale claimed, chunk among `groups`; marks it claimed by `host`)
  - `pub fn finish(conn: &Connection, group: &str, day: i64) -> Result<()>`
  - `pub fn release(conn: &Connection, group: &str, day: i64) -> Result<()>`
  - `pub fn progress(conn: &Connection, group: &str) -> Result<(i64, i64)>` (done, total)
  - `pub fn split_groups(conn: &Connection) -> Result<Vec<String>>` (groups with any pending or claimed chunk)

- [ ] **Step 1: Write the failing tests**

Create `src/chunks.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        create(&c).unwrap();
        c
    }

    #[test]
    fn days_are_claimed_newest_first_and_once() {
        let c = conn();
        assert_eq!(add(&c, "g", 20_000, 19_998).unwrap(), 3);
        assert_eq!(add(&c, "g", 20_000, 19_998).unwrap(), 0, "adding again adds nothing");
        assert!(is_split(&c, "g").unwrap());
        let groups = vec!["g".to_string()];

        assert_eq!(claim(&c, &groups, "a", 1000).unwrap(), Some(("g".into(), 20_000)));
        assert_eq!(claim(&c, &groups, "b", 1000).unwrap(), Some(("g".into(), 19_999)));
        finish(&c, "g", 20_000).unwrap();
        release(&c, "g", 19_999).unwrap();
        assert_eq!(claim(&c, &groups, "b", 1000).unwrap(), Some(("g".into(), 19_999)), "released goes back");
        assert_eq!(claim(&c, &groups, "a", 1000).unwrap(), Some(("g".into(), 19_998)));
        assert_eq!(claim(&c, &groups, "a", 1000).unwrap(), None);
        assert_eq!(progress(&c, "g").unwrap(), (1, 3));
    }

    #[test]
    fn stale_claims_are_taken_over() {
        let c = conn();
        add(&c, "g", 5, 5).unwrap();
        let groups = vec!["g".to_string()];
        assert!(claim(&c, &groups, "a", 1000).unwrap().is_some());
        assert_eq!(claim(&c, &groups, "b", 1000 + CLAIM_TIMEOUT - 1).unwrap(), None);
        assert_eq!(claim(&c, &groups, "b", 1000 + CLAIM_TIMEOUT + 1).unwrap(), Some(("g".into(), 5)));
    }

    #[test]
    fn only_listed_groups_and_unsplit_groups() {
        let c = conn();
        add(&c, "g", 5, 5).unwrap();
        assert_eq!(claim(&c, &["other".to_string()], "a", 0).unwrap(), None);
        assert!(!is_split(&c, "other").unwrap());
        assert_eq!(split_groups(&c).unwrap(), vec!["g".to_string()]);
        assert_eq!(unix_day(86_400 * 3 + 5), 3);
    }
}
```

Add `pub mod chunks;` to `src/lib.rs` (alphabetical, after `pub mod app;`... keep the existing order style).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib chunks`
Expected: compile errors for missing functions.

- [ ] **Step 3: Implement**

Above the test module in `src/chunks.rs`:

```rust
//! Day chunks of a group's backfill that any server carrying the group can
//! take (see docs/superpowers/specs/2026-10-05-faster-backfill-smaller-storage-design.md).
//! Lives in the main database.

use rusqlite::{Connection, OptionalExtension, Result, params};

/// a claim older than this is taken over (its worker died or was stopped)
pub const CLAIM_TIMEOUT: i64 = 30 * 60;

const PENDING: i64 = 0;
const CLAIMED: i64 = 1;
const DONE: i64 = 2;

pub fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "create table if not exists backfill_chunks (
            grp TEXT NOT NULL,
            day INTEGER NOT NULL,
            state INTEGER NOT NULL,
            server TEXT,
            claimed_at INTEGER,
            primary key (grp, day)
        ) without rowid;",
    )
}

/// days since 1970-01-01 UTC
pub fn unix_day(t: i64) -> i64 {
    t.div_euclid(86_400)
}

/// Pending chunks for `newest_day` down to `oldest_day`, keeping any that exist.
pub fn add(conn: &Connection, group: &str, newest_day: i64, oldest_day: i64) -> Result<usize> {
    let mut insert =
        conn.prepare_cached("insert or ignore into backfill_chunks (grp, day, state) values (?, ?, ?)")?;
    let mut added = 0;
    for day in (oldest_day..=newest_day).rev() {
        added += insert.execute(params![group, day, PENDING])?;
    }
    Ok(added)
}

pub fn is_split(conn: &Connection, group: &str) -> Result<bool> {
    conn.prepare_cached("select 1 from backfill_chunks where grp = ? limit 1")?.exists([group])
}

/// The newest chunk of `groups` that is pending or whose claim went stale,
/// claimed for `host`.
pub fn claim(conn: &Connection, groups: &[String], host: &str, now: i64) -> Result<Option<(String, i64)>> {
    if groups.is_empty() {
        return Ok(None);
    }
    let marks = vec!["?"; groups.len()].join(",");
    let sql = format!(
        "select grp, day from backfill_chunks
         where grp in ({marks}) and (state = {PENDING} or (state = {CLAIMED} and claimed_at < ?))
         order by day desc limit 1"
    );
    let mut values: Vec<rusqlite::types::Value> = groups.iter().map(|g| g.clone().into()).collect();
    values.push((now - CLAIM_TIMEOUT).into());
    let found: Option<(String, i64)> = conn
        .prepare(&sql)?
        .query_row(rusqlite::params_from_iter(values), |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((group, day)) = &found {
        conn.execute(
            "update backfill_chunks set state = ?, server = ?, claimed_at = ? where grp = ? and day = ?",
            params![CLAIMED, host, now, group, day],
        )?;
    }
    Ok(found)
}

pub fn finish(conn: &Connection, group: &str, day: i64) -> Result<()> {
    conn.execute("update backfill_chunks set state = ? where grp = ? and day = ?", params![DONE, group, day])?;
    Ok(())
}

pub fn release(conn: &Connection, group: &str, day: i64) -> Result<()> {
    conn.execute(
        "update backfill_chunks set state = ?, server = null, claimed_at = null where grp = ? and day = ?",
        params![PENDING, group, day],
    )?;
    Ok(())
}

/// (done, total) chunks of a group
pub fn progress(conn: &Connection, group: &str) -> Result<(i64, i64)> {
    conn.query_row(
        "select coalesce(sum(state = 2), 0), count(*) from backfill_chunks where grp = ?",
        [group],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// groups with chunks still to do
pub fn split_groups(conn: &Connection) -> Result<Vec<String>> {
    conn.prepare("select distinct grp from backfill_chunks where state != 2 order by grp")?
        .query_map([], |r| r.get(0))?
        .collect()
}
```

The callers (Task 4) serialize access through the indexer's single main connection, so claim's select-then-update is safe without a transaction. Add the doc line "the indexer's main connection is the only writer, soo select then update is safe" above `claim`.

In `src/store.rs` `create_main`, append `crate::chunks::create(conn)?;` before `Ok(())` (convert `create_main` from an expression to a block if needed).

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib chunks && just test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/chunks.rs src/lib.rs src/store.rs
git commit -m "Day chunks of backfill in the main database

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: `run_chunk`: index one day of a group on one server

**Files:**
- Modify: `src/indexer.rs` (new `pub async fn run_chunk` beside `run_pass`; `Pass::process_range` reused)
- Test: `tests/date_search.rs` (add a test; it already has a date mock)

**Interfaces:**
- Consumes: `Pool::select_group_on`, `Pool::article_at` (Task 1), `chunks::{finish, release}` (Task 2), `Pass::process_range(&self, start, end, kind, progress) -> Result<(Progress, bool)>`, `on_db`, `cursor_key`.
- Produces:
  - `pub const CHUNK_OVERLAP: i64 = 3600;`
  - `pub async fn run_chunk<P>(ctx: &PassContext, settings: &PassSettings, db: &Db, group: &str, server: usize, day: i64, progress: &mut P) -> Result<Progress> where P: FnMut(&Progress) + ?Sized`. On success it calls `chunks::finish`; when stopped part way, or on error, it calls `chunks::release` and returns the error (stopped: `Ok` with what was saved).
  - Errors: if `select_group_on(server, group)` answers from a different server, return `Err(anyhow!("{group} isnt on {host}"))` after releasing the chunk.

- [ ] **Step 1: Write the failing test**

Append to `tests/date_search.rs`:

```rust
/// One day of a group indexed on one server: exactly that day's posts, the
/// chunk marked done.
#[test]
fn a_day_chunk_indexes_that_day() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let port = spawn_server(Server::new(hourly()));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let day = atlas::chunks::unix_day(chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00+00:00").unwrap().timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, day, day).unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let saved = pool
        .block_on(atlas::indexer::run_chunk(&ctx, &Default::default(), &db, GROUP, 0, day, &mut |_| {}))
        .unwrap();

    // hours 24..47 are articles 25..48, plus an hour of overlap each side
    // (24 and 49); 30 and 40 don't exist. 24..=49 minus 2 = 24 articles
    assert_eq!(saved.articles, 24);
    let conn = atlas::db::open_at(&main).unwrap();
    assert_eq!(atlas::chunks::progress(&conn, GROUP).unwrap(), (1, 1));
}
```

`PassContext` fields are public; check `RunStates` implements `Default` (`grep -n "struct RunStates" -B2 src/indexer.rs`). If `PassSettings` doesn't implement `Default` publicly, use `atlas::indexer::PassSettings::default()`; it exists.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --test date_search a_day_chunk`
Expected: compile error, `run_chunk` not found.

- [ ] **Step 3: Implement**

In `src/indexer.rs`, after `run_pass`:

```rust
/// a day chunk also takes this much on each side: post dates are only
/// roughly in article number order, duplicates are dropped when saved
pub const CHUNK_OVERLAP: i64 = 3600;

/// Index one day (`day`, unix days) of `group` on `server`, for a group whose
/// backfill is split into day chunks (see chunks.rs). The chunk is marked done
/// when the whole day is in, released again if stopped part way or failing.
pub async fn run_chunk<P>(
    ctx: &PassContext,
    settings: &PassSettings,
    db: &Db,
    group: &str,
    server: usize,
    day: i64,
    progress: &mut P,
) -> Result<Progress>
where
    P: FnMut(&Progress) + ?Sized,
{
    let release = |db: &Db, group: &str| {
        let g = group.to_string();
        on_db(db, move |conn| Ok(crate::chunks::release(conn, &g, day)?))
    };

    let found = match ctx.pool.select_group_on(server, group).await {
        Ok((s, info)) if s == server => info,
        Ok(_) | Err(_) => {
            release(db, group).await?;
            return Err(anyhow!("{group} isnt on {}", ctx.pool.host(server)));
        }
    };
    let (_count, first, last, _name) = found;

    let from = day * 86_400 - CHUNK_OVERLAP;
    let to = (day + 1) * 86_400 + CHUNK_OVERLAP;
    let start = match ctx.pool.article_at(server, group, first, last, from).await {
        Ok(n) => n,
        Err(e) => {
            release(db, group).await?;
            return Err(e.into());
        }
    };
    let end = match ctx.pool.article_at(server, group, start.max(first), last, to).await {
        Ok(n) => n.saturating_sub(1),
        Err(e) => {
            release(db, group).await?;
            return Err(e.into());
        }
    };
    if start > end {
        let g = group.to_string();
        on_db(db, move |conn| Ok(crate::chunks::finish(conn, &g, day)?)).await?;
        return Ok(Progress::default());
    }

    let pass = Pass { ctx, settings, db, group, server, key: cursor_key(&ctx.pool, server, group) };
    match pass.process_range(start as i64, end as i64, "CHUNK", progress).await {
        Ok((saved, true)) => {
            let g = group.to_string();
            on_db(db, move |conn| Ok(crate::chunks::finish(conn, &g, day)?)).await?;
            Ok(saved)
        }
        Ok((saved, false)) => {
            release(db, group).await?;
            Ok(saved)
        }
        Err(e) => {
            release(db, group).await?;
            Err(e)
        }
    }
}
```

`NntpError` converts into `anyhow::Error` through `?`/`.into()` (it implements `std::error::Error`). `make_slices` reverses only for `"BACKFILL"`, so `"CHUNK"` slices run oldest first, which is fine.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test date_search && just test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/indexer.rs tests/date_search.rs
git commit -m "Index one day chunk of a group on one server

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Split big groups and let idle workers take chunks

**Files:**
- Modify: `src/indexer.rs` (`run_pass` splits a qualifying group; a split group's backfill pass runs a chunk instead of the cursor)
- Modify: `src/bg_indexer.rs` (`worker` claims chunks when it has nothing of its own; the scheduler remembers which servers don't carry a split group)
- Modify: `src/config.rs` (optional `split_min_backlog` in config.json, default 10,000,000, for tests and tuning)
- Test: `tests/split_backfill.rs` (new; the only test in its binary)

**Interfaces:**
- Consumes: Task 1 `article_at`, Task 2 `chunks::*`, Task 3 `run_chunk`, `CHUNK_OVERLAP`.
- Produces:
  - `PassSettings` gains `pub split_min_backlog: i64` (default `SPLIT_MIN_BACKLOG = 10_000_000`, from `config.split_min_backlog()`).
  - `pub const SPLIT_MIN_BACKLOG: i64 = 10_000_000;` in `src/config.rs`, `Config::split_min_backlog(&self) -> i64`, read from optional JSON key `split_min_backlog`.
  - In `src/indexer.rs`: `async fn maybe_split(ctx, db, group, home: usize, first: u64, cursor: u64) -> Result<bool>` (private).
  - In `src/bg_indexer.rs`: `async fn take_chunk(sched: &Scheduler, server: usize, db: &Db) -> Option<(String, i64)>`.

- [ ] **Step 1: Write the failing end-to-end test**

Create `tests/split_backfill.rs`:

```rust
//! A big group's backfill split across two servers that number the same
//! posts differently: both servers index day chunks, every post is saved
//! exactly once, and every chunk ends done.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post_at, spawn_server};

/// 40 posts a day for 30 days, numbered from `offset`
fn posts(offset: u64) -> Vec<common::Post> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    (0..1200u64)
        .map(|i| {
            let when = start + chrono::Duration::minutes(i as i64 * 36);
            let mut p = post_at(offset + i, &format!(r#""rel{}.part{}.rar" yEnc (1/1)"#, i / 10, i % 10 + 1), &when.to_rfc2822());
            p.message_id = format!("<post{i}@split>");
            p
        })
        .collect()
}

#[test]
fn a_split_group_is_shared_by_both_servers() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    let a = Server::new(posts(1));
    let b = Server::new(posts(500_001));
    let (pa, pb) = (spawn_server(a.clone()), spawn_server(b.clone()));
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": pa, "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": pb, "ssl": false, "connections": 4, "priority": 1}
        ],
        "groups": [GROUP],
        "index_mode": "backfill",
        "batch_size": 100,
        "request_size": 50,
        "split_min_backlog": 500
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    let main = home.path().join("atlas.db");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        let (_, articles) = atlas::store::totals(&conn).unwrap();
        let split = atlas::chunks::is_split(&conn, GROUP).unwrap_or(false);
        let (done, total) = atlas::chunks::progress(&conn, GROUP).unwrap_or((0, 0));
        if split && total > 0 && done == total && articles >= 1200 {
            break;
        }
        assert!(Instant::now() < deadline, "articles {articles}, chunks {done}/{total}, split {split}");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);

    let conn = atlas::db::open_with_shards(&main).unwrap();
    assert_eq!(atlas::store::totals(&conn).unwrap().1, 1200, "every post once, overlaps deduplicated");
    assert!(a.xovers.load(Ordering::SeqCst) > 0 && b.xovers.load(Ordering::SeqCst) > 0, "both servers worked");
}
```

`Post.message_id` is a `pub` field (Task 1 kept it). If `Server` has `any_group`, leave it false: both carry `GROUP`.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --test split_backfill`
Expected: compile error for the `split_min_backlog` key not existing. Or, if config ignores unknown keys, a timeout panic with `split false`.

- [ ] **Step 3: Config and settings**

In `src/config.rs`: add `pub const SPLIT_MIN_BACKLOG: i64 = 10_000_000;` beside the other defaults, a field `pub split_min_backlog: Option<i64>` on `Config` (default `None`), parse it from the JSON key `split_min_backlog` the same way `batch_size` is parsed, write it back in `save_config` only if `Some`, and add:

```rust
/// article numbers of backfill left before a group's backfill is split
/// over every server that carries it
pub fn split_min_backlog(&self) -> i64 {
    self.split_min_backlog.unwrap_or(SPLIT_MIN_BACKLOG).max(1)
}
```

In `src/indexer.rs` `PassSettings`, add `pub split_min_backlog: i64,` and set it in `Default` to `crate::config::SPLIT_MIN_BACKLOG`. In `src/bg_indexer.rs` `settings_of`, set `split_min_backlog: config.split_min_backlog()`. Fix any other `PassSettings { .. }` literals (`grep -rn "PassSettings {" src tests`).

- [ ] **Step 4: Split in `run_pass`**

In `src/indexer.rs`, add:

```rust
/// Split `group`'s backfill into day chunks when its home server still has
/// more than `split_min_backlog` article numbers to go and another indexing
/// server carries it too. Chunks run from the day at the home cursor back to
/// the oldest day any carrying server has. True when the group is split.
async fn maybe_split(ctx: &PassContext, settings: &PassSettings, db: &Db, group: &str, home: usize, first: u64, cursor: u64) -> Result<bool> {
    let g = group.to_string();
    if on_db(db, move |conn| Ok(crate::chunks::is_split(conn, &g)?)).await? {
        return Ok(true);
    }
    if (cursor.saturating_sub(first) as i64) < settings.split_min_backlog {
        return Ok(false);
    }

    // the newest day still to do: the post date at the home cursor
    let newest = ctx.pool.article_at(home, group, cursor, cursor, i64::MIN).await.ok();
    let newest_day = match ctx.pool.posted_date(home, group, newest.unwrap_or(cursor)).await? {
        Some(t) => crate::chunks::unix_day(t),
        None => return Ok(false),
    };

    // the oldest day any other carrying server has
    let mut oldest_day = newest_day;
    let mut carriers = 1;
    for other in ctx.pool.indexing_servers().into_iter().filter(|&s| s != home) {
        let Ok((s, (_, low, _, _))) = ctx.pool.select_group_on(other, group).await else { continue };
        if s != other {
            continue;
        }
        if let Some(t) = ctx.pool.posted_date(other, group, low).await? {
            carriers += 1;
            oldest_day = oldest_day.min(crate::chunks::unix_day(t));
        }
    }
    if let Some(t) = ctx.pool.posted_date(home, group, first).await? {
        oldest_day = oldest_day.min(crate::chunks::unix_day(t));
    }
    if carriers < 2 {
        return Ok(false);
    }

    let g = group.to_string();
    let added = on_db(db, move |conn| Ok(crate::chunks::add(conn, &g, newest_day, oldest_day)?)).await?;
    println!("[SPLIT] {group}: backfill split into {added} day chunks over {carriers} servers");
    Ok(true)
}
```

Also add to `impl Pool` in `src/nntp.rs` (reuses Task 1's `posted_at`):

```rust
/// When the first article at or after `number` on server `i` was posted (unix seconds).
pub async fn posted_date(&self, i: usize, group: &str, number: u64) -> Result<Option<i64>> {
    Ok(self.posted_at(i, group, number, 100).await?.map(|(_, t)| t))
}
```

Drop the unused `newest` probe line in `maybe_split` if clippy flags it; `posted_date(home, group, cursor)` is enough. Keep the function minimal.

In `run_pass`, right before `let pass = Pass { ... }`, decide the backfill route. Only when this pass would backfill (`settings.mode == "backfill"`, or dynamic in the backfill phase):

```rust
let backfilling = settings.mode == "backfill" || (settings.mode != "live" && phase_is_backfill);
if backfilling && maybe_split(ctx, settings, db, group, server, first as u64, state.backfill_cursor.max(0) as u64).await? {
    // a split group's backfill is day chunks, this server takes the next one
    let host = ctx.pool.host(server);
    let g = group.to_string();
    let claimed = on_db(db, move |conn| {
        Ok(crate::chunks::claim(conn, std::slice::from_ref(&g), &host, chrono::Utc::now().timestamp())?)
    })
    .await?;
    return match claimed {
        Some((_, day)) => run_chunk(ctx, settings, db, group, server, day, progress).await,
        None => {
            ctx.states.with(group, |st| st.backfilling = false);
            Ok(Progress::default())
        }
    };
}
```

Read `phase` before this block, as the existing code already does (`let phase = ctx.states.with(group, |st| st.phase);`), and use `phase == Phase::Backfill` for `phase_is_backfill`. Live passes are untouched.

- [ ] **Step 5: Idle workers take chunks**

In `src/bg_indexer.rs`, add to `Scheduler` a field `skip: Mutex<HashSet<(String, usize)>>` (group, server pairs that answered "not on this server"), initialized empty. Add:

```rust
/// A day chunk of any split group for an idle worker on `server`: the newest
/// one pending, among the groups this server hasnt said it lacks.
async fn take_chunk(sched: &Scheduler, server: usize, db: &Db) -> Option<(String, i64)> {
    let host = sched.ctx.pool.host(server);
    let skip: Vec<String> = sched.skip.lock().unwrap().iter().filter(|(_, s)| *s == server).map(|(g, _)| g.clone()).collect();
    crate::indexer::claim_chunk(db, host, skip).await.ok().flatten()
}
```

and in `src/indexer.rs` a public helper that reaches the main connection through `on_db`:

```rust
/// Claim the newest chunk of any split group for `host`, except `skip`.
pub async fn claim_chunk(db: &Db, host: String, skip: Vec<String>) -> Result<Option<(String, i64)>> {
    on_db(db, move |conn| {
        let groups: Vec<String> = crate::chunks::split_groups(conn)?.into_iter().filter(|g| !skip.contains(g)).collect();
        Ok(crate::chunks::claim(conn, &groups, &host, chrono::Utc::now().timestamp())?)
    })
    .await
}
```

In `worker`, when `take_group` returns `None`, try a chunk before napping:

```rust
let Some(group) = sched.take_group(server) else {
    if let Some((group, day)) = take_chunk(&sched, server, &db).await {
        let settings = sched.settings.read().unwrap().clone();
        let stats = sched.stats.clone();
        let mut progress = |p: &Progress| stats.lock().unwrap().tick(p.articles, p.bytes, p.releases, &group);
        let chunk = crate::indexer::run_chunk(&sched.ctx, &settings, &db, &group, server, day, &mut progress);
        match unless_stopped(&sched.ctx.stop, chunk).await {
            None => break,
            Some(Err(e)) if e.to_string().contains("isnt on") => {
                sched.skip.lock().unwrap().insert((group, server));
            }
            Some(Err(e)) => ui::error(&format!("Indexing error ({group}, day chunk): {e}")),
            Some(Ok(_)) => {}
        }
        continue;
    }
    nap(Duration::from_secs(1), || sched.stopping()).await;
    continue;
};
```

Don't mark the group busy for chunks: chunks of one group run on several servers at once by design.

- [ ] **Step 6: Run the tests**

Run: `cargo test --test split_backfill && just test`
Expected: PASS. If the split test times out, print `bg_index.log` from the temp home in the assertion message to debug. Check that `[SPLIT]` appears and that both servers' `xovers` counters move.

- [ ] **Step 7: Lint and commit**

Run: `just format && just lint`

```bash
git add src/indexer.rs src/bg_indexer.rs src/config.rs src/nntp.rs tests/split_backfill.rs
git commit -m "Split big groups' backfill into day chunks across servers

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Backfill page counts chunk progress

**Files:**
- Modify: `src/stats_dashboard.rs` (`load_quick` reads chunk progress; `draw_backfill` shows it)
- Test: unit test in `src/stats_dashboard.rs`

**Interfaces:**
- Consumes: `chunks::{split_groups, progress}` (Task 2).
- Produces: `Quick` gains `chunks: Vec<(String, i64, i64)>` (group, done, total). The Backfill page adds a line "split groups: N, day chunks D of T done".

- [ ] **Step 1: Write the failing test**

In the `tests` module of `src/stats_dashboard.rs`, extend `pages_render` (or add a test) to build `Quick { chunks: vec![("alt.binaries.x".into(), 3, 10)], ..Quick::default() }` in the app's `quick`, render page 1 (Backfill) with `TestBackend`, and assert the text contains `day chunks 3 of 10 done`.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --lib stats_dashboard`
Expected: compile error, no `chunks` field.

- [ ] **Step 3: Implement**

- Add `chunks: Vec<(String, i64, i64)>` to `Quick` (it derives `Default`).
- In `load_quick`, after `group_progress`:

```rust
q.chunks = crate::chunks::split_groups(&conn)
    .unwrap_or_default()
    .into_iter()
    .filter_map(|g| crate::chunks::progress(&conn, &g).ok().map(|(d, t)| (g, d, t)))
    .collect();
```

- In `draw_backfill`, after the "groups not started" line:

```rust
if !quick.chunks.is_empty() {
    let (done, total) = quick.chunks.iter().fold((0, 0), |(d, t), c| (d + c.1, t + c.2));
    progress.push(kv("split groups", format!("{}, day chunks {done} of {total} done", quick.chunks.len())));
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib stats_dashboard && just lint`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/stats_dashboard.rs
git commit -m "Backfill page shows day chunk progress of split groups

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Part B: pack message-id locals

### Task 6: Pack locals by alphabet

**Files:**
- Modify: `src/store.rs` (`pack_local`, `unpack_local`, `add_segment`)
- Test: unit tests in `src/store.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub(crate) fn pack_local(local: &str) -> Vec<u8>`: same signature, new tags.
  - `pub(crate) fn unpack_local(blob: &[u8]) -> String`: now `pub(crate)`, decodes tags 0–7.
  - `pub(crate) fn repack(local: &[u8]) -> Vec<u8>`, `= pack_local(&unpack_local(local))` (used by compact, Task 10).
  - Tags (verbatim from the spec): 0 text, 1 hex lower, 2 hex upper (both without a length byte), 3 `0-9`, 4 `0-9a-z`, 5 `0-9A-Z`, 6 `0-9A-Za-z`, 7 `0-9A-Za-z-_`. Tags 3–7 are `[tag][len][base-N big-endian number bytes]`; `len` is the character count (1–255).

- [ ] **Step 1: Write the failing tests**

Add to `src/store.rs` tests:

```rust
#[test]
fn locals_pack_by_shape_and_come_back_exactly() {
    let cases = [
        ("1064b678f3f54e28a5afd48a3a986076", 1u8), // ngPost hex
        ("ABCDEF0123", 2),
        ("0001234", 3),                             // digits (odd length, so not hex), leading zeros kept
        ("0abc9xyz", 4),
        ("0ABC9XYZ", 5),
        ("hotfTpetaZRIbOYuTuQ31", 6),               // JBinUp
        ("DR59tDkIGMDKQS1YflogRq2MTqVgoHslO", 6),
        ("ZjPsQyLgOjNtQvXbHeKaXyCm1730295651235", 6), // Nyuu: letters + ms timestamp
        ("a-b_c-9", 7),
        ("abc", 4),                                 // odd length hex-looking goes base36
        ("nnd$009a5634$43be2fd7", 0),               // anything else stays text
        ("", 0),
    ];
    for (local, tag) in cases {
        let packed = pack_local(local);
        assert_eq!(packed[0], tag, "{local}");
        assert_eq!(unpack_local(&packed), local, "{local}");
        assert_eq!(pack_local(local), packed, "deterministic: {local}");
    }
    let long = "a".repeat(300);
    assert_eq!(pack_local(&long)[0], 0, "longer than 255 stays text");
    assert!(pack_local("ZjPsQyLgOjNtQvXbHeKaXyCm1730295651235").len() < 30, "base62 is smaller than text");
}
```

and to the existing `only_shared_domains_get_rows_and_nothing_is_saved_twice` test (or a new one), a legacy duplicate check: insert a segment row by hand with `local = [0] ++ b"hotfTpetaZRIbOYuTuQ31"` and a shared domain id, then `add_segment` the same message-id `<hotfTpetaZRIbOYuTuQ31@JBinUp.local>` after the domain is shared, and assert it returns `false`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib store`
Expected: FAIL on tag assertions.

- [ ] **Step 3: Implement**

Replace `pack_local` and `unpack_local`:

```rust
const DIGITS: u8 = 3;
const BASE36_LOWER: u8 = 4;
const BASE36_UPPER: u8 = 5;
const BASE62: u8 = 6;
const BASE64_URL: u8 = 7;

/// the alphabet of each packed tag, by digit value
fn alphabet(tag: u8) -> &'static [u8] {
    match tag {
        DIGITS => b"0123456789",
        BASE36_LOWER => b"0123456789abcdefghijklmnopqrstuvwxyz",
        BASE36_UPPER => b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        BASE62 => b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        BASE64_URL => b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-_",
        _ => b"",
    }
}

/// big-endian base-256 bytes of a base-`n` number given as digit values
fn to_bytes(digits: &[u8], n: u32) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new(); // little endian while building
    for &d in digits {
        let mut carry = u32::from(d);
        for b in out.iter_mut() {
            let v = u32::from(*b) * n + carry;
            *b = (v & 0xff) as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            out.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    out.reverse();
    out
}

/// `len` base-`n` digit values of a big-endian base-256 number
fn from_bytes(bytes: &[u8], n: u32, len: usize) -> Vec<u8> {
    let mut num: Vec<u8> = bytes.to_vec();
    let mut digits = Vec::with_capacity(len);
    for _ in 0..len {
        let mut rem = 0u32;
        for b in num.iter_mut() {
            let v = (rem << 8) | u32::from(*b);
            *b = (v / n) as u8;
            rem = v % n;
        }
        digits.push(rem as u8);
    }
    digits.reverse();
    digits
}

pub(crate) fn pack_local(local: &str) -> Vec<u8> {
    let bytes = local.as_bytes();
    let even = !bytes.is_empty() && bytes.len().is_multiple_of(2);
    let lower = even && bytes.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    let upper = even && !lower && bytes.iter().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(b));
    if lower || upper {
        let nibble = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        let mut out = Vec::with_capacity(1 + bytes.len() / 2);
        out.push(if lower { HEX_LOWER } else { HEX_UPPER });
        out.extend(bytes.chunks(2).map(|p| (nibble(p[0]) << 4) | nibble(p[1])));
        return out;
    }
    if (1..=255).contains(&bytes.len()) {
        for tag in [DIGITS, BASE36_LOWER, BASE36_UPPER, BASE62, BASE64_URL] {
            let abc = alphabet(tag);
            let values: Option<Vec<u8>> = bytes.iter().map(|b| abc.iter().position(|a| a == b).map(|p| p as u8)).collect();
            if let Some(values) = values {
                let mut out = vec![tag, bytes.len() as u8];
                out.extend(to_bytes(&values, abc.len() as u32));
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(1 + bytes.len());
    out.push(TEXT);
    out.extend_from_slice(bytes);
    out
}

pub(crate) fn unpack_local(blob: &[u8]) -> String {
    match blob.first() {
        Some(&HEX_LOWER) => blob[1..].iter().map(|b| format!("{b:02x}")).collect(),
        Some(&HEX_UPPER) => blob[1..].iter().map(|b| format!("{b:02X}")).collect(),
        Some(&tag) if (DIGITS..=BASE64_URL).contains(&tag) && blob.len() >= 2 => {
            let abc = alphabet(tag);
            from_bytes(&blob[2..], abc.len() as u32, blob[1] as usize).iter().map(|&d| abc[d as usize] as char).collect()
        }
        _ => String::from_utf8_lossy(blob.get(1..).unwrap_or_default()).into_owned(),
    }
}

/// a local packed the current way, whatever way it was packed before
pub(crate) fn repack(local: &[u8]) -> Vec<u8> {
    pack_local(&unpack_local(local))
}
```

Precedence is hex first (even length, so `"00012345"` would be tag 1), then tags 3, 4, 5, 6, 7 in that order. The test table follows it.

In `add_segment`, after the existing whole-id check, also check the legacy text form when the new local is packed with tags 3–7:

```rust
// a row from before this packing has the same local stored as text
if (3..=7).contains(&local[0]) {
    let mut legacy = vec![0u8];
    legacy.extend_from_slice(unpack_local(&local).as_bytes());
    let stored_legacy = conn
        .prepare_cached("select 1 from segments where file_id = ? and local = ? and domain = ?")?
        .exists(params![file_id, legacy, domain])?;
    if stored_legacy {
        return Ok(false);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store && just test`
Expected: PASS. NZB equality tests elsewhere (`tests/convert.rs`, `compact` tests) must still pass, because reads decode both forms.

- [ ] **Step 5: Lint and commit**

Run: `just format && just lint`

```bash
git add src/store.rs
git commit -m "Pack message-id locals by alphabet

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Part C: seal finished files into blobs

### Task 7: Blob codec

**Files:**
- Create: `src/blob.rs`
- Modify: `src/lib.rs` (`pub mod blob;`), `Cargo.toml` (`zstd = "0.13"`), `Cargo.lock`
- Test: unit tests in `src/blob.rs`

**Interfaces:**
- Produces (in `atlas::blob`):
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct Seg { pub part: Option<i64>, pub bytes: i64, pub domain: i64, pub local: Vec<u8> }`
  - `pub fn encode(segs: &[Seg]) -> Vec<u8>` (sorts a copy by `(part.is_some(), part, local)` for compression; zstd level 6)
  - `pub fn decode(blob: &[u8]) -> std::io::Result<Vec<Seg>>`

- [ ] **Step 1: Add the dependency**

Add `zstd = "0.13"` under `[dependencies]` in `Cargo.toml` (alphabetical). Run `cargo build` so `Cargo.lock` updates.

- [ ] **Step 2: Write the failing tests**

Create `src/blob.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn seg(part: Option<i64>, bytes: i64, domain: i64, local: &[u8]) -> Seg {
        Seg { part, bytes, domain, local: local.to_vec() }
    }

    #[test]
    fn segments_come_back_exactly() {
        let segs = vec![
            seg(Some(2), 750_000, 3, b"\x06\x05abc"),
            seg(Some(1), 750_123, 3, b"\x01\xde\xad"),
            seg(None, 0, 0, b"\x00<whole@id>"),
            seg(Some(3), 12, 9, b""),
        ];
        let mut back = decode(&encode(&segs)).unwrap();
        let mut want = segs.clone();
        back.sort_by(|a, b| (a.part, &a.local).cmp(&(b.part, &b.local)));
        want.sort_by(|a, b| (a.part, &a.local).cmp(&(b.part, &b.local)));
        assert_eq!(back, want);
        assert!(decode(&encode(&[])).unwrap().is_empty());
    }

    #[test]
    fn a_big_file_is_small() {
        let segs: Vec<Seg> = (1..=1000)
            .map(|p| seg(Some(p), 768_000, 1, &[1, (p % 251) as u8, (p * 7 % 251) as u8, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]))
            .collect();
        let size = encode(&segs).len();
        assert!(size < 1000 * 20, "{size} bytes for 1000 segments");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"not zstd").is_err());
    }
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --lib blob`
Expected: compile errors.

- [ ] **Step 4: Implement**

Above the tests in `src/blob.rs`:

```rust
//! A sealed file's articles in one compressed value (files.blob), see the
//! storage spec. Format v1, zstd compressed:
//! u8 version, varint count, then per segment: varint part+1 (0 = none),
//! zigzag varint bytes minus the previous segment's, varint domain, varint
//! local length, local bytes.

use std::io::{Error, ErrorKind, Result};

const VERSION: u8 = 1;
const LEVEL: i32 = 6;

/// One article of a sealed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seg {
    pub part: Option<i64>,
    pub bytes: i64,
    pub domain: i64,
    pub local: Vec<u8>,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "blob cut short"))?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::new(ErrorKind::InvalidData, "varint too long"))
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

pub fn encode(segs: &[Seg]) -> Vec<u8> {
    let mut sorted: Vec<&Seg> = segs.iter().collect();
    sorted.sort_by(|a, b| (a.part.is_some(), a.part, &a.local).cmp(&(b.part.is_some(), b.part, &b.local)));
    let mut raw = vec![VERSION];
    put_varint(&mut raw, sorted.len() as u64);
    let mut prev_bytes = 0i64;
    for s in sorted {
        put_varint(&mut raw, s.part.map_or(0, |p| p as u64 + 1));
        put_varint(&mut raw, zigzag(s.bytes - prev_bytes));
        prev_bytes = s.bytes;
        put_varint(&mut raw, s.domain as u64);
        put_varint(&mut raw, s.local.len() as u64);
        raw.extend_from_slice(&s.local);
    }
    zstd::bulk::compress(&raw, LEVEL).expect("zstd compress of an in memory buffer")
}

pub fn decode(blob: &[u8]) -> Result<Vec<Seg>> {
    let raw = zstd::stream::decode_all(blob)?;
    if raw.first() != Some(&VERSION) {
        return Err(Error::new(ErrorKind::InvalidData, "unknown blob version"));
    }
    let mut pos = 1;
    let count = get_varint(&raw, &mut pos)? as usize;
    let mut segs = Vec::with_capacity(count.min(1 << 20));
    let mut prev_bytes = 0i64;
    for _ in 0..count {
        let part = match get_varint(&raw, &mut pos)? {
            0 => None,
            p => Some(p as i64 - 1),
        };
        let bytes = prev_bytes + unzigzag(get_varint(&raw, &mut pos)?);
        prev_bytes = bytes;
        let domain = get_varint(&raw, &mut pos)? as i64;
        let len = get_varint(&raw, &mut pos)? as usize;
        let local = raw.get(pos..pos + len).ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "blob cut short"))?.to_vec();
        pos += len;
        segs.push(Seg { part, bytes, domain, local });
    }
    Ok(segs)
}
```

Add `pub mod blob;` to `src/lib.rs`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib blob && just lint`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/blob.rs src/lib.rs
git commit -m "Compressed per-file segment blob codec

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Sealing in the store: schema, seal, read, late articles

**Files:**
- Modify: `src/store.rs`
- Test: unit tests in `src/store.rs`

**Interfaces:**
- Consumes: `blob::{Seg, encode, decode}` (Task 7), `pack_local`/`unpack_local` (Task 6).
- Produces:
  - Schema: `files.blob BLOB`, `files.touched_at INTEGER`, added in `create_shard` for new shards. `pub fn migrate_shard(conn: &Connection) -> Result<()>` adds them to existing shards with `alter table` when missing; `create_shard` calls it.
  - `FileState` gains `pub sealed: bool` (`blob is not null`), set in `file()`.
  - `put_file` also sets `touched_at = unixepoch()`.
  - `pub const SEAL_AGE: i64 = 3 * 86_400;`
  - `pub(crate) fn seal_file(conn: &Connection, file_id: i64) -> Result<usize>` (number of segments in the new blob; 0 if the file had no rows)
  - `pub(crate) fn sealable(conn: &Connection, after_id: i64, limit: usize, now: i64) -> Result<(Vec<i64>, i64)>` (eligible file ids after `after_id`, plus the last id looked at, for walking the table)
  - `pub(crate) struct SealedCache` with `fn new() -> Self` and `fn contains(&mut self, conn, file_id, local: &[u8], domain: i64) -> Result<bool>` (decodes a file's blob once per cache lifetime)
  - `add_segment` signature becomes `add_segment(conn, domains, f: &FileState, a: &Article, sealed: &mut SealedCache) -> Result<bool>`, and it skips articles already in the blob.
  - `articles()` merges blob segments with loose rows.
  - `pub fn article_count(conn: &Connection, schema: &str) -> Result<i64>` (loose rows plus blob segments), used by compact's check (Task 10).

- [ ] **Step 1: Write the failing tests**

Add to `src/store.rs` tests:

```rust
/// a shard with one release: file a.rar (3 parts) and b.rar (2 parts)
fn sealed_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    db::create_db_at(&main).unwrap();
    let art = |file: &str, part: i64, total: i64, id: &str| Article {
        message_id: id.into(),
        subject: format!("\"{file}\" yEnc ({part}/{total})"),
        filename: Some(file.into()),
        part: Some(part),
        total_parts: Some(total),
        bytes: 100 + part,
        ..Default::default()
    };
    let release = Release {
        name: "Rel".into(),
        group: "alt.binaries.t".into(),
        articles: vec![
            art("a.rar", 1, 3, "<a1@x>"),
            art("a.rar", 2, 3, "<0abc12def34@ngPost>"),
            art("a.rar", 3, 3, "<a3@x>"),
            art("b.rar", 1, 2, "<b1@x>"),
        ],
        ..Default::default()
    };
    save(&main, &[release]).unwrap();
    (dir, main)
}

#[test]
fn sealing_keeps_every_nzb_and_late_articles_merge() {
    let (_dir, main) = sealed_fixture();
    let shard = shard_path(&main, shard_of("alt.binaries.t"));
    let read = || {
        let conn = db::open_with_shards(&main).unwrap();
        let id: i64 = conn.query_row("select id from releases", [], |r| r.get(0)).unwrap();
        articles(&conn, id).unwrap()
    };
    let before = read();

    let conn = db::open_at(&shard).unwrap();
    let ids: Vec<i64> = conn.prepare("select id from files").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    for id in &ids {
        seal_file(&conn, *id).unwrap();
    }
    let loose: i64 = conn.query_row("select count(*) from segments", [], |r| r.get(0)).unwrap();
    assert_eq!(loose, 0, "sealed files have no rows left");
    assert_eq!(read(), before, "same articles back from the blobs");
    assert_eq!(article_count(&db::open_with_shards(&main).unwrap(), &format!("s{}", shard_of("alt.binaries.t"))).unwrap(), 4);

    // a late article for a sealed file is saved once, one already sealed isnt
    let late = Release {
        name: "Rel".into(),
        group: "alt.binaries.t".into(),
        articles: vec![
            Article { message_id: "<b2@x>".into(), filename: Some("b.rar".into()), part: Some(2), total_parts: Some(2), bytes: 102, ..Default::default() },
            Article { message_id: "<a1@x>".into(), filename: Some("a.rar".into()), part: Some(1), total_parts: Some(3), bytes: 101, ..Default::default() },
        ],
        ..Default::default()
    };
    save(&main, &[late]).unwrap();
    let after = read();
    assert_eq!(after.len(), 5);
    assert_eq!(after.iter().filter(|a| a.message_id == "<a1@x>").count(), 1);

    // resealing folds the late row in
    let b: i64 = conn.query_row("select id from files where filename = 'b.rar'", [], |r| r.get(0)).unwrap();
    assert_eq!(seal_file(&conn, b).unwrap(), 2);
    assert_eq!(read(), after);
}

#[test]
fn complete_or_stale_files_are_sealable() {
    let (_dir, main) = sealed_fixture();
    let conn = db::open_at(&shard_path(&main, shard_of("alt.binaries.t"))).unwrap();
    let now = 2_000_000_000;
    // a.rar is complete (1..=3), b.rar has 1 of 2 and was just touched
    let (ids, _) = sealable(&conn, 0, 100, now).unwrap();
    let name = |id: i64| -> String { conn.query_row("select filename from files where id = ?", [id], |r| r.get(0)).unwrap() };
    assert_eq!(ids.iter().map(|&i| name(i)).collect::<Vec<_>>(), vec!["a.rar".to_string()]);
    conn.execute("update files set touched_at = ? where filename = 'b.rar'", [now - SEAL_AGE - 1]).unwrap();
    assert_eq!(sealable(&conn, 0, 100, now).unwrap().0.len(), 2, "b.rar went stale");
}
```

`put_file` stamps `touched_at` with the real clock. `sealable` takes `now` as a parameter, so the test sets `touched_at` relative to it.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib store`
Expected: compile errors.

- [ ] **Step 3: Schema and FileState**

- In `create_shard`'s `create table if not exists files (...)`, add `blob BLOB, touched_at INTEGER` columns. After the `execute_batch`, call `migrate_shard(&conn)?`.
- `migrate_shard`: read `pragma table_info(files)` (see `db::columns`; add a small local helper) and `alter table files add column blob BLOB` / `alter table files add column touched_at INTEGER` for any that are missing.
- `FileState`: add `pub sealed: bool`. In `file()`, select `blob is not null` as a 7th column and set `sealed`. New files get `sealed: false`.
- `put_file`: add `touched_at = unixepoch()` to the update.

- [ ] **Step 4: Seal, sealable, read, cache**

```rust
/// files untouched this long get sealed even when incomplete
pub const SEAL_AGE: i64 = 3 * 86_400;

/// Seal a file: its rows (and any blob it already has) become one blob, the
/// rows go. Returns the segments in the blob.
pub(crate) fn seal_file(conn: &Connection, file_id: i64) -> Result<usize> {
    let mut segs: Vec<crate::blob::Seg> = match conn
        .query_row("select blob from files where id = ? and blob is not null", [file_id], |r| r.get::<_, Vec<u8>>(0))
        .optional()?
    {
        Some(b) => crate::blob::decode(&b).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
        None => Vec::new(),
    };
    let rows: Vec<crate::blob::Seg> = conn
        .prepare_cached("select part, bytes, domain, local from segments where file_id = ?")?
        .query_map([file_id], |r| {
            Ok(crate::blob::Seg { part: r.get(0)?, bytes: r.get::<_, Option<i64>>(1)?.unwrap_or(0), domain: r.get(2)?, local: r.get(3)? })
        })?
        .collect::<Result<_>>()?;
    if rows.is_empty() && !segs.is_empty() {
        return Ok(segs.len());
    }
    segs.extend(rows);
    if segs.is_empty() {
        return Ok(0);
    }
    conn.prepare_cached("update files set blob = ? where id = ?")?.execute(params![crate::blob::encode(&segs), file_id])?;
    conn.prepare_cached("delete from segments where file_id = ?")?.execute([file_id])?;
    Ok(segs.len())
}

/// Files after `after_id` with rows to seal: complete, or untouched for
/// `SEAL_AGE` (never touched since the upgrade counts as old). Returns the
/// ids and the last id looked at.
pub(crate) fn sealable(conn: &Connection, after_id: i64, limit: usize, now: i64) -> Result<(Vec<i64>, i64)> {
    let mut stmt = conn.prepare_cached(
        "select f.id, f.expected, f.seen, f.touched_at from files f
         where f.id > ? and exists (select 1 from segments s where s.file_id = f.id)
         order by f.id limit ?",
    )?;
    let mut ids = Vec::new();
    let mut last = after_id;
    let mut rows = stmt.query(params![after_id, limit as i64])?;
    while let Some(r) = rows.next()? {
        let (id, expected, seen, touched): (i64, Option<i64>, Vec<u8>, Option<i64>) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        last = id;
        let complete = expected.is_some_and(|e| is_exactly(&seen, e));
        if complete || touched.is_none_or(|t| t < now - SEAL_AGE) {
            ids.push(id);
        }
    }
    Ok((ids, last))
}

/// Blobs of sealed files, decoded once each, for checking late articles.
#[derive(Default)]
pub(crate) struct SealedCache {
    files: HashMap<i64, std::collections::HashSet<(Vec<u8>, i64)>>,
}

impl SealedCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// the article (local, domain) is already in file `file_id`'s blob
    pub(crate) fn contains(&mut self, conn: &Connection, file_id: i64, local: &[u8], domain: i64) -> Result<bool> {
        if !self.files.contains_key(&file_id) {
            let set = match conn
                .query_row("select blob from files where id = ? and blob is not null", [file_id], |r| r.get::<_, Vec<u8>>(0))
                .optional()?
            {
                Some(b) => crate::blob::decode(&b)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                    .into_iter()
                    .map(|s| (s.local, s.domain))
                    .collect(),
                None => Default::default(),
            };
            self.files.insert(file_id, set);
        }
        Ok(self.files[&file_id].contains(&(local.to_vec(), domain)))
    }
}
```

Change `add_segment` to take `f: &FileState` and `sealed: &mut SealedCache` (`file_id` becomes `f.id`). Before inserting, add:

```rust
if f.sealed && sealed.contains(conn, f.id, &local, domain)? {
    return Ok(false);
}
```

Also check the whole-id form against the blob when `domain != 0` (`sealed.contains(conn, f.id, &whole(&a.message_id), 0)`) for sealed files. Update the caller in `ShardWriter::save`: create `let mut sealed = SealedCache::new();` at the start of `save` and pass `&f` / `&mut sealed`. Update tests that call `add_segment` directly (the domain test) to build a `FileState` via `file()` and pass `&mut SealedCache::new()`.

In `articles()`, after collecting the loose rows, add blob rows:

```rust
let mut sealed = conn.prepare(&format!(
    "select f.blob, f.filename, f.expected, f.subject, r.poster, r.posted_date
     from s{s}.files f join s{s}.releases r on r.id = f.release_id
     where f.release_id = ? and f.blob is not null"
))?;
let mut suffixes: HashMap<i64, Option<String>> = HashMap::new();
let mut blob_rows = sealed.query([release_id])?;
while let Some(r) = blob_rows.next()? {
    let blob: Vec<u8> = r.get(0)?;
    let filename: String = r.get(1)?;
    for seg in crate::blob::decode(&blob).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))? {
        if !suffixes.contains_key(&seg.domain) {
            let suffix: Option<String> = conn
                .query_row(&format!("select suffix from s{s}.domains where id = ?"), [seg.domain], |r| r.get(0))
                .optional()?;
            suffixes.insert(seg.domain, suffix);
        }
        rows.push(ArticleRow {
            message_id: decode(&seg.local, suffixes[&seg.domain].as_deref()),
            filename: (!filename.is_empty()).then(|| filename.clone()),
            part: seg.part,
            total_parts: r.get(2)?,
            bytes: Some(seg.bytes),
            subject: r.get(3)?,
            poster: r.get(4)?,
            posted_date: r.get(5)?,
        });
    }
}
```

The existing final sort covers the merged rows. `rows` must be `let mut rows: Vec<ArticleRow>` before this block.

`article_count`:

```rust
/// articles in shard `schema` (`s3`, or `main` for a shard opened on its
/// own): loose rows plus every sealed blob's segments
pub fn article_count(conn: &Connection, schema: &str) -> Result<i64> {
    let loose: i64 = conn.query_row(&format!("select count(*) from {schema}.segments"), [], |r| r.get(0))?;
    let mut sealed = 0i64;
    let mut stmt = conn.prepare(&format!("select blob from {schema}.files where blob is not null"))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let b: Vec<u8> = r.get(0)?;
        sealed += crate::blob::decode(&b).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?.len() as i64;
    }
    Ok(loose + sealed)
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib store && just test`
Expected: PASS, including `tests/convert.rs` and the compact test.

- [ ] **Step 6: Lint and commit**

Run: `just format && just lint` (use a type alias if clippy flags `type_complexity` on the tuple in `sealable`).

```bash
git add src/store.rs
git commit -m "Seal a file's articles into one blob; reads and late articles see it

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Shard writers seal as they go

**Files:**
- Modify: `src/store.rs` (`ShardWriter` keeps a seal cursor; `seal_some`)
- Modify: `src/indexer.rs` (the writer thread calls it between transactions)
- Modify: `src/profile.rs` (`writer_seal_ns` load counter)
- Test: unit test in `src/store.rs`

**Interfaces:**
- Consumes: `sealable`, `seal_file` (Task 8).
- Produces:
  - `pub const SEAL_PER_TICK: usize = 2_000;`
  - `ShardWriter::seal_some(&mut self, conn: &mut Connection, now: i64) -> Result<usize>` (seals up to `SEAL_PER_TICK` files in one transaction; walks `files` by id from where it stopped, wrapping to 0 at the end; returns the files sealed).
  - `LOAD.writer_seal_ns` in `src/profile.rs`, reported in the load snapshot as `writer_seal_ns`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_writer_seals_what_is_due_a_tick_at_a_time() {
    let (_dir, main) = sealed_fixture();
    let shard = shard_of("alt.binaries.t");
    let mut conn = db::open_at(&shard_path(&main, shard)).unwrap();
    let mut writer = ShardWriter::new(shard);
    let now = 2_000_000_000;
    conn.execute("update files set touched_at = ?", [now]).unwrap();
    assert_eq!(writer.seal_some(&mut conn, now).unwrap(), 1, "only the complete file");
    conn.execute("update files set touched_at = ?", [now - SEAL_AGE - 1]).unwrap();
    assert_eq!(writer.seal_some(&mut conn, now).unwrap(), 1, "the stale one, after wrapping around");
    assert_eq!(writer.seal_some(&mut conn, now).unwrap(), 0);
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --lib a_writer_seals`
Expected: compile error.

- [ ] **Step 3: Implement**

Add `seal_after: i64` (starts at 0) to `ShardWriter`, plus:

```rust
/// files sealed per writer tick at most
pub const SEAL_PER_TICK: usize = 2_000;

impl ShardWriter {
    /// Seal up to SEAL_PER_TICK files that are due, walking the files table by
    /// id from where the last tick stopped (back to the start at the end).
    pub fn seal_some(&mut self, conn: &mut Connection, now: i64) -> Result<usize> {
        let tx = conn.transaction()?;
        let (mut ids, last) = sealable(&tx, self.seal_after, SEAL_PER_TICK * 4, now)?;
        ids.truncate(SEAL_PER_TICK);
        if ids.len() == SEAL_PER_TICK {
            self.seal_after = *ids.last().unwrap();
        } else if last == self.seal_after {
            self.seal_after = 0;
        } else {
            self.seal_after = last;
        }
        for id in &ids {
            seal_file(&tx, *id)?;
        }
        tx.commit()?;
        Ok(ids.len())
    }
}
```

The test expects the second call to find the stale file even though the cursor moved past it, so the wrap must happen within the same call when nothing was found. Adjust the logic: if `ids.is_empty()` and `self.seal_after != 0`, reset `seal_after = 0` and search once more before returning. Keep the test as the source of truth.

In `src/profile.rs`, add `pub writer_seal_ns: AtomicU64` to `Load` (initialize it in `LOAD`, add `"writer_seal_ns"` to `snapshot`).

In `src/indexer.rs` `writer()`, after `store.save(...)` and before `finish_checkpoint`:

```rust
let t = std::time::Instant::now();
let _ = store.seal_some(conn, chrono::Utc::now().timestamp());
Load::add_since(&LOAD.writer_seal_ns, t);
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store && just test`
Expected: PASS.

- [ ] **Step 5: Lint and commit**

Run: `just format && just lint`

```bash
git add src/store.rs src/indexer.rs src/profile.rs
git commit -m "Shard writers seal due files between transactions

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: `--compact` re-encodes locals and seals

**Files:**
- Modify: `src/compact.rs`
- Test: the existing test module in `src/compact.rs`

**Interfaces:**
- Consumes: `store::repack` (Task 6), `store::{sealable, seal_file, article_count}` (Task 8), `blob` (Task 7).
- Produces: `compact_shard` writes locals repacked with the current packing, then seals every eligible file in the copy. `check()` compares `article_count` instead of raw `segments` row counts, and samples NZBs as before.

- [ ] **Step 1: Write the failing test**

Extend `compacting_drops_single_use_domains_and_keeps_every_nzb` (or add a test) so the fixture's shared-domain rows are stored with legacy text locals. Then, after `run`, assert:

```rust
for path in store::shard_paths(&main) {
    let conn = db::open_at(&path).unwrap();
    let loose: i64 = conn.query_row("select count(*) from segments", [], |r| r.get(0)).unwrap();
    assert_eq!(loose, 0, "every file was due (never touched since the upgrade), so all are sealed");
}
assert_eq!(all_articles(&main), before, "every NZB reads back the same");
```

Also assert that `article_count` over all shards equals the article count before compaction.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --lib compact`
Expected: FAIL, `loose` is not 0.

- [ ] **Step 3: Implement**

In the article copy loop, for kept rows (`domain == 0 || shared(domain)`), use `store::repack(&local)` for packed shared-domain locals (`domain != 0`). Whole ids (domain 0) stay as they are. Before the `insert or ignore`, normalize: for `domain != 0`, `let local = store::repack(&local);`.

After copying segments and before `building indexes`, seal everything due in the copy:

```rust
say("sealing files");
let now = chrono::Utc::now().timestamp();
let mut after = 0;
loop {
    let tx = conn.transaction()?;
    let (ids, last) = store::sealable(&tx, after, 50_000, now)?;
    for id in &ids {
        store::seal_file(&tx, *id)?;
    }
    tx.commit()?;
    if last == after {
        break;
    }
    after = last;
}
```

`sealable` and `seal_file` must be reachable: make them `pub` (not `pub(crate)`) if `compact.rs` can't see them; it's in the same crate, so `pub(crate)` works.

In `check()`, replace the `segments` count comparison with `store::article_count(&old, &format!("s{shard}"))? == store::article_count(&new, &format!("s{shard}"))?`, and replace `copied != count(&new, "segments")` with a comparison of `copied` against the new `article_count` (allowing for nothing lost: the counts must be equal). Keep the `releases` and `files` row checks.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib compact && just test`
Expected: PASS.

- [ ] **Step 5: Lint and commit**

Run: `just format && just lint`

```bash
git add src/compact.rs src/store.rs
git commit -m "Compact re-encodes locals and seals due files

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Part D: auto compaction

### Task 11: `auto_run_compact`

**Files:**
- Modify: `src/config.rs` (`auto_run_compact` key)
- Modify: `src/bg_indexer.rs` (timer task, run compaction between `run_servers` rounds)
- Modify: `src/store.rs` (`get_meta`/`set_meta` on the main database)
- Test: `tests/auto_compact.rs` (new; only test in its binary)

**Interfaces:**
- Produces:
  - `Config.auto_run_compact: bool` (JSON `auto_run_compact`, default `false`, written back only when true).
  - `pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<i64>>`, `pub fn set_meta(conn: &Connection, key: &str, value: i64) -> Result<()>` in `store`.
  - `pub const COMPACT_EVERY: Duration = Duration::from_secs(24 * 3600);` in `bg_indexer`, overridable in tests through env `ATLAS_COMPACT_EVERY_SECS`.

- [ ] **Step 1: Write the failing test**

Create `tests/auto_compact.rs`:

```rust
//! With auto_run_compact on, the indexer stops for a compaction once the
//! interval has passed, records it, and goes back to indexing.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post, spawn_server};

#[test]
fn compacts_on_schedule_and_keeps_indexing() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
        std::env::set_var("ATLAS_COMPACT_EVERY_SECS", "2");
    }
    let posts: Vec<common::Post> = (1..=200).map(|n| post(n, &format!(r#""r{}.part{}.rar" yEnc (1/1)"#, n / 20, n % 20 + 1), 10, vec![])).collect();
    let port = spawn_server(Server::new(posts));
    let config = serde_json::json!({
        "usenet_servers": [{"host": "127.0.0.1", "username": "bob", "password": "secret", "port": port, "ssl": false, "connections": 2, "priority": 1}],
        "groups": [GROUP], "index_mode": "backfill", "request_size": 50, "auto_run_compact": true
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    assert!(cfg.auto_run_compact);
    let runner = { let stop = stop.clone(); std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop)) };

    let main = home.path().join("atlas.db");
    let deadline = Instant::now() + Duration::from_secs(60);
    let compacted = loop {
        let conn = atlas::db::open_at(&main).unwrap();
        if let Some(t) = atlas::store::get_meta(&conn, "last_compact").unwrap() {
            break t;
        }
        assert!(Instant::now() < deadline, "never compacted");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(compacted > 0);

    // indexing goes on after it: everything gets indexed
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        if atlas::store::totals(&conn).unwrap().1 >= 200 { break; }
        assert!(Instant::now() < deadline, "indexing didnt resume");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --test auto_compact`
Expected: compile error, no `auto_run_compact` field.

- [ ] **Step 3: Config and meta**

- `src/config.rs`: `pub auto_run_compact: bool` on `Config` (default `false`), parsed from `auto_run_compact` (bool), written back by `save_config` only when `true`.
- `src/store.rs`:

```rust
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<i64>> {
    conn.query_row("select value from main.meta where key = ?", [key], |r| r.get(0)).optional()
}

pub fn set_meta(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute("insert or replace into main.meta (key, value) values (?, ?)", params![key, value])?;
    Ok(())
}
```

- [ ] **Step 4: Schedule in the indexer**

In `src/bg_indexer.rs`:

```rust
/// how often auto_run_compact compacts (ATLAS_COMPACT_EVERY_SECS for tests)
fn compact_every() -> Duration {
    std::env::var("ATLAS_COMPACT_EVERY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(24 * 3600))
}
```

- Add `compact_due: AtomicBool` to `Scheduler`.
- In `run_servers`, if `config.auto_run_compact`, spawn a task that naps in 1 s steps until `due` (computed from `get_meta(main, "last_compact")` plus `compact_every()`, or from the indexer start when there's none). When due, it sets `compact_due` and `wind_down` and returns.
- `run_servers` returns `bool` (compaction due).
- In `supervise`, after `run_servers(...)` and `pool.close()`, if it returned true:

```rust
write_status(true, "compacting the database", &config.index_mode, false, "running", false, 0);
let main = paths::database();
let started = std::time::Instant::now();
let report = |msg: &str| println!("{msg}");
match crate::compact::run(&main, &report) {
    Ok(_) => println!("compacted in {}", crate::dashboard::human_time(started.elapsed().as_secs() as i64)),
    Err(e) => {
        ui::error(&format!("auto compaction failed, the originals were kept: {e:#}"));
        stats.lock().unwrap().error_count += 1;
    }
}
if let Ok(conn) = db::open_at(&main) {
    let _ = crate::store::set_meta(&conn, "last_compact", chrono::Utc::now().timestamp());
}
```

`compact::run` takes `&(dyn Fn(&str) + Sync)`; a closure that only prints is `Sync`. The loop continues and rebuilds the pool as for a config change. Record `last_compact` even on failure, so a failing compaction is retried 24 hours later, not every minute.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test auto_compact && just test`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

Run: `just format && just lint`

```bash
git add src/config.rs src/bg_indexer.rs src/store.rs tests/auto_compact.rs
git commit -m "auto_run_compact: compact every 24 hours from the indexer

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: Documentation and full verification

**Files:**
- Modify: `README.md`, `config.example.json`

- [ ] **Step 1: README**

In `README.md`, using the same Google developer style as the rest of the file (second person, present tense, sentence-case headings, no "e.g."):

- **Top-level keys table:** add rows for `split_min_backlog` (default `10000000`: "Article numbers of backfill left before a group's backfill is split into day chunks that every server carrying the group can take") and `auto_run_compact` (default `false`: "Compact the database every 24 hours; indexing pauses while it runs").
- **"How the database is stored":** add one paragraph on sealed files ("a file's articles are sealed into one compressed blob once it's complete or untouched for 3 days") and one on packed message-ids.
- **Indexing section:** add a short "Splitting big groups" paragraph: chunks by day, idle workers on any carrying server take the newest, live indexing stays on the home server.
- **Converting an older database / compact text:** `--compact` also re-encodes message-ids and seals due files.

- [ ] **Step 2: config.example.json**

Add `"auto_run_compact": false` at the top level.

- [ ] **Step 3: Full verification**

Run: `just format && just format-check && just lint && just test`
Expected: all clean and every test passing. Record the test count.

- [ ] **Step 4: Commit**

```bash
git add README.md config.example.json
git commit -m "Document split backfill, sealed files, packed message-ids, auto_run_compact

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Self-review notes

- **Spec coverage:** Part 1 maps to Tasks 1–5 (`article_at`, table, `run_chunk`, scheduling, dashboard). Part 2 maps to Task 6. Part 3 maps to Tasks 7–10 (codec, store sealing plus reads plus late articles, online sealing, compact). Part 4 maps to Task 11. Testing items are spread through the tasks; docs are in Task 12.
- **Deviation from the spec:** the blob format sorts segments by `(part, local)` for compression instead of by message-id. Reads sort after decoding (`articles()` already does), so NZB output is unchanged.
- **Type consistency:**
  - `add_segment` changes signature in Task 8 (`&FileState`, `&mut SealedCache`). Task 6 adds a check inside it first, and Task 8 must keep that check.
  - `PassSettings.split_min_backlog` is added in Task 4; any `PassSettings` literal in tests must add it or use `..Default::default()`.
