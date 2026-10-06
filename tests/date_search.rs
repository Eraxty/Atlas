//! Finding the article posted at a given time by binary search over one
//! article XOVERs, with gaps in the numbering and slightly out of order dates.

mod common;

use atlas::nntp::BlockingPool;
use common::{GROUP, Server, mock, post_at, spawn_server};

/// Pass settings whose day chunks are exactly what their date searches
/// find, without the reach for neighbouring days on other servers
fn exact() -> atlas::indexer::PassSettings {
    atlas::indexer::PassSettings { chunk_safety: 0, ..Default::default() }
}

/// `day` of `group` claimed by the mock's server, the way a worker claims it
/// (only the claim's owner finishes or gives back a chunk)
fn claimed(main: &std::path::Path, group: &str, day: i64) -> atlas::chunks::Claim {
    let chunk = atlas::chunks::Claim { group: group.into(), day, server: "127.0.0.1".into(), claimed_at: 1 };
    atlas::db::open_at(main)
        .unwrap()
        .execute(
            "update backfill_chunks set state = 1, server = ?, claimed_at = ? where grp = ? and day = ?",
            rusqlite::params![chunk.server, chunk.claimed_at, group, day],
        )
        .unwrap();
    chunk
}

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

    let day =
        atlas::chunks::unix_day(chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00+00:00").unwrap().timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    // a day between two others: the oldest and newest reach the marks
    atlas::chunks::add(&main_conn, GROUP, day + 1, day - 1).unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let chunk = claimed(&main, GROUP, day);
    let saved = pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {})).unwrap();

    // hours 24..47 are articles 25..48, plus an hour of overlap each side
    // (24 and 49); 30 and 40 don't exist. 24..=49 minus 2 = 24 articles
    assert_eq!(saved.articles, 24);
    let conn = atlas::db::open_at(&main).unwrap();
    assert_eq!(atlas::chunks::progress(&conn, GROUP).unwrap(), (1, 3));
}

/// posts `low..=high`, number n posted n - 1 hours after 2026-01-01 00:00 UTC,
/// minus the numbers in `gap`
fn with_gap(low: u64, high: u64, gap: std::ops::RangeInclusive<u64>) -> Vec<common::Post> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    (low..=high)
        .filter(|n| !gap.contains(n))
        .map(|n| {
            let when = start + chrono::Duration::hours(n as i64 - 1);
            post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &when.to_rfc2822())
        })
        .collect()
}

/// unix seconds of the hour number `n` is posted at in `with_gap`
fn hour_of(n: u64) -> i64 {
    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap().timestamp() + (n as i64 - 1) * 3600
}

fn search(posts: Vec<common::Post>, low: u64, high: u64, when: i64) -> u64 {
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();
    pool.block_on(pool.pool.article_at(0, GROUP, low, high, when)).unwrap()
}

#[test]
fn a_long_gap_at_the_midpoint_is_skipped_forward() {
    // 1..=2000 with 900..=1199 missing: the first probe (1000) lands in the gap
    let posts = || with_gap(1, 2000, 900..=1199);
    assert_eq!(search(posts(), 1, 2000, hour_of(1500)), 1500, "after the gap");
    assert_eq!(search(posts(), 1, 2000, hour_of(1200)), 1200, "the first article after the gap");
    assert_eq!(search(posts(), 1, 2000, hour_of(1000)), 1200, "inside the gap: the next article");
    assert_eq!(search(posts(), 1, 2000, hour_of(800)), 800, "before the gap");
}

#[test]
fn a_long_gap_at_the_start_settles_on_the_first_article() {
    // 1..=300 missing, the group's low says 1
    let posts = || with_gap(1, 1300, 1..=300);
    assert_eq!(search(posts(), 1, 1300, hour_of(1) - 86_400), 301, "before everything: the first article");
    assert_eq!(search(posts(), 1, 1300, hour_of(700)), 700);
}

#[test]
fn a_long_gap_at_the_end_is_high_plus_one() {
    // the group's high says 2000, nothing after 1699 exists
    let posts = || with_gap(1, 1699, 0..=0);
    assert_eq!(search(posts(), 1, 2000, hour_of(1800)), 2001, "after everything: high + 1");
    assert_eq!(search(posts(), 1, 2000, hour_of(1650)), 1650);
}

/// one post a minute: 1..=50 from 2026-01-01 00:00 UTC, nothing from 51 to
/// 3,000,000, then 3,000,001..=3,100,000 from 2026-01-03 00:00
fn holed() -> Vec<common::Post> {
    let at = |rfc3339: &str| chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap();
    let (before, after) = (at("2026-01-01T00:00:00+00:00"), at("2026-01-03T00:00:00+00:00"));
    let post = |n: u64, when: chrono::DateTime<chrono::FixedOffset>| {
        post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &when.to_rfc2822())
    };
    let early = (1..=50u64).map(|n| post(n, before + chrono::Duration::minutes(n as i64 - 1)));
    let late = (3_000_001..=3_100_000u64).map(|n| post(n, after + chrono::Duration::minutes((n - 3_000_001) as i64)));
    early.chain(late).collect()
}

/// unix seconds article `n` of `holed` is posted at
fn minute_of(n: u64) -> i64 {
    let at = |rfc3339: &str| chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap().timestamp();
    if n <= 50 {
        at("2026-01-01T00:00:00+00:00") + (n as i64 - 1) * 60
    } else {
        at("2026-01-03T00:00:00+00:00") + (n - 3_000_001) as i64 * 60
    }
}

#[test]
fn a_hole_of_millions_is_crossed_to_its_first_article_in_few_requests() {
    let server = Server::new(holed());
    let port = spawn_server(server.clone());
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();
    let xovers = || server.xovers.load(std::sync::atomic::Ordering::SeqCst);
    let at = |low: u64, when: i64| {
        let before = xovers();
        let n = pool.block_on(pool.pool.article_at(0, GROUP, low, 3_100_000, when)).unwrap();
        let used = xovers() - before;
        assert!(used <= 150, "{used} requests to find {n}");
        n
    };
    let in_the_hole = minute_of(50) + 86_400;

    assert_eq!(at(1, minute_of(40)), 40);
    assert_eq!(at(1, in_the_hole), 3_000_001, "inside the hole: the first article after it");
    assert_eq!(at(1, minute_of(3_000_001)), 3_000_001, "the first article after the hole");
    assert_eq!(at(1, minute_of(3_000_002)), 3_000_002);
    assert_eq!(at(1, minute_of(3_050_000)), 3_050_000);
    assert_eq!(at(1, minute_of(3_100_000) + 60), 3_100_001, "after everything: high + 1");

    // a low watermark that still says 1,500,000 though nothing is there
    assert_eq!(at(1_500_000, minute_of(40)), 3_000_001, "everything from low on is newer");
    assert_eq!(at(1_500_000, in_the_hole), 3_000_001);
    assert_eq!(at(1_500_000, minute_of(3_000_001)), 3_000_001);
}

/// one post a minute from `when` on, numbered `numbers`
fn minutely(numbers: std::ops::RangeInclusive<u64>, when: &str) -> Vec<common::Post> {
    let start = chrono::DateTime::parse_from_rfc3339(when).unwrap();
    let first = *numbers.start();
    numbers
        .map(|n| {
            let at = start + chrono::Duration::minutes((n - first) as i64);
            post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &at.to_rfc2822())
        })
        .collect()
}

fn unix(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap().timestamp()
}

/// A few articles in the numbers the spread out windows step over, before
/// the articles they land on: going forward, they are still the first.
#[test]
fn a_small_cluster_between_spread_out_windows_is_found_going_forward() {
    // nothing below 40,000, five posts there, nothing again till 100,000
    let mut posts = minutely(40_000..=40_004, "2026-01-01T00:00:00+00:00");
    posts.extend(minutely(100_000..=103_000, "2026-01-03T00:00:00+00:00"));
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let first = pool.block_on(pool.pool.first_post(0, GROUP, 1, 103_000)).unwrap();
    assert_eq!(first, Some(unix("2026-01-01T00:00:00+00:00")), "the cluster's first post");
    let at = |when: &str| pool.block_on(pool.pool.article_at(0, GROUP, 1, 103_000, unix(when))).unwrap();
    assert_eq!(at("2025-12-31T00:00:00+00:00"), 40_000);
    assert_eq!(at("2026-01-01T00:02:00+00:00"), 40_002);
    assert_eq!(at("2026-01-02T00:00:00+00:00"), 100_000);
}

/// Articles only in numbers the spread out windows step over, none where
/// any window lands: the search finds them instead of taking the range for
/// empty (a server whose history is all there would look like it has none).
#[test]
fn articles_only_between_spread_out_windows_are_found() {
    // five posts at 20,000, then nothing till a run at 190,000
    let mut posts = minutely(20_000..=20_004, "2026-01-02T00:00:00+00:00");
    posts.extend(minutely(190_000..=190_100, "2026-01-03T00:00:00+00:00"));
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    // going forward: every window in 1..=100,000 is empty
    let first = pool.block_on(pool.pool.first_post(0, GROUP, 1, 100_000)).unwrap();
    assert_eq!(first, Some(unix("2026-01-02T00:00:00+00:00")), "the cluster's first post");
    // going backward from the middle (a hole up to the run): the cluster is
    // the last article before it, every window short of it is empty
    let at = |when: &str| pool.block_on(pool.pool.article_at(0, GROUP, 1, 190_100, unix(when))).unwrap();
    assert_eq!(at("2026-01-01T00:00:00+00:00"), 20_000);
    assert_eq!(at("2026-01-02T00:03:00+00:00"), 20_003);
    assert_eq!(at("2026-01-02T12:00:00+00:00"), 190_000);
}

/// The same going backwards: a few articles in the numbers the spread out
/// windows step over, after the articles they land on, are still the last
/// before the hole.
#[test]
fn a_small_cluster_between_spread_out_windows_is_found_going_backward() {
    // 1..=600 (ten hours), five posts at 25,000, nothing again till 90,000
    let mut posts = minutely(1..=600, "2026-01-01T00:00:00+00:00");
    posts.extend(minutely(25_000..=25_004, "2026-01-02T00:00:00+00:00"));
    posts.extend(minutely(90_000..=93_000, "2026-01-03T00:00:00+00:00"));
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let at = |when: &str| pool.block_on(pool.pool.article_at(0, GROUP, 1, 93_000, unix(when))).unwrap();
    assert_eq!(at("2026-01-02T00:00:00+00:00"), 25_000);
    assert_eq!(at("2026-01-01T12:00:00+00:00"), 25_000, "between the first run and the cluster");
    assert_eq!(at("2026-01-02T12:00:00+00:00"), 90_000);
}

/// The first day after a hole of millions is indexed whole.
#[test]
fn a_day_chunk_right_after_a_hole_of_millions_gets_every_article() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let port = spawn_server(Server::new(holed()));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let day = atlas::chunks::unix_day(minute_of(3_000_001));
    let main_conn = atlas::db::open_at(&main).unwrap();
    // a day between two others: the oldest and newest reach the marks
    atlas::chunks::add(&main_conn, GROUP, day + 1, day - 1).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let chunk = claimed(&main, GROUP, day);
    let saved = pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {})).unwrap();

    // the day's 1,440 posts and the hour after it; the hour before is the hole
    assert_eq!(saved.articles, 1_440 + 60);
    let conn = atlas::db::open_at(&main).unwrap();
    assert_eq!(atlas::chunks::progress(&conn, GROUP).unwrap(), (1, 3));
}

/// A day chunk whose day has 150 numbers missing in the middle still fetches
/// every article of that day.
#[test]
fn a_day_chunk_skips_a_gap_inside_the_day() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();

    // one post every 2 minutes from 2026-01-01: day 1 is 1..=720, day 2 is
    // 721..=1440, and 1050..=1199 are missing right where the search for the
    // day's end first probes
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts: Vec<common::Post> = (1..=1440u64)
        .filter(|n| !(1050..=1199).contains(n))
        .map(|n| {
            let when = start + chrono::Duration::minutes((n as i64 - 1) * 2);
            post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &when.to_rfc2822())
        })
        .collect();
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let day =
        atlas::chunks::unix_day(chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00+00:00").unwrap().timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    // a day between two others: the oldest and newest reach the marks
    atlas::chunks::add(&main_conn, GROUP, day + 1, day - 1).unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let chunk = claimed(&main, GROUP, day);
    let saved = pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {})).unwrap();

    // day 2 is 720 numbers minus the 150 missing, plus the hour before it
    // (691..=720, 30 posts); nothing comes after 1440
    assert_eq!(saved.articles, 720 - 150 + 30);
    let conn = atlas::db::open_at(&main).unwrap();
    assert_eq!(atlas::chunks::progress(&conn, GROUP).unwrap(), (1, 3));
}

/// A chunk that fails and then cant be given back still reports why it
/// failed, not the failure to give it back.
#[test]
fn a_failing_chunk_reports_its_own_error() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let port = spawn_server(Server::new(hourly()));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    // giving the chunk back fails: there is no chunk table
    let main_conn = atlas::db::open_at(&main).unwrap();
    main_conn.execute_batch("drop table backfill_chunks").unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    // the mock doesnt carry this group: GROUP answers 411
    let err = pool
        .block_on(atlas::indexer::run_chunk(
            &ctx,
            &exact(),
            &db,
            &atlas::chunks::Claim {
                group: "alt.binaries.other".into(),
                day: 20_455,
                server: "127.0.0.1".into(),
                claimed_at: 1,
            },
            0,
            &mut |_| {},
        ))
        .unwrap_err();
    assert!(err.downcast_ref::<atlas::indexer::NotCarried>().is_some(), "{err:#}");
}

/// A day before anything the server keeps goes back unfinished (another
/// server may have it); an empty day the server does keep is finished.
#[test]
fn a_day_older_than_the_server_keeps_is_given_back() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour of 2026-01-01 and of 2026-01-03, none on 2026-01-02
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = (0..72u64)
        .filter(|h| !(24..48).contains(h))
        .map(|h| {
            post_at(
                h + 1,
                &format!(r#""p{h}.bin" yEnc (1/1)"#),
                &(start + chrono::Duration::hours(h as i64)).to_rfc2822(),
            )
        })
        .collect();
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let first_day = atlas::chunks::unix_day(start.timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, first_day + 2, first_day - 1).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let run = |day: i64| {
        let chunk = claimed(&main, GROUP, day);
        pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {}))
    };

    let err = run(first_day - 1).unwrap_err();
    let too_old = err.downcast_ref::<atlas::indexer::TooOld>().expect("a TooOld");
    assert_eq!(too_old.oldest_day, first_day);
    assert_eq!(ctx.states.keeps_from(GROUP, &pool.pool.host(0)), first_day, "remembered for the next claims");
    let empty = run(first_day + 1).unwrap();
    assert_eq!(empty.articles, 2, "only the overlap hours");
    let conn = atlas::db::open_at(&main).unwrap();
    let state = |day: i64| -> Option<i64> {
        conn.query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0)).ok()
    };
    // the only server: no server has a post that old, the day is dropped
    assert_eq!(state(first_day - 1), None, "before every server's first post, dropped");
    assert_eq!(state(first_day + 1), Some(2), "an empty day it keeps is done");
}

/// Stopping drops a chunk part way (here on a request stuck on the network):
/// the chunk goes back to pending right away, not after CLAIM_TIMEOUT, soo a
/// quick restart doesnt find it taken.
#[test]
fn a_chunk_dropped_by_stopping_is_given_back() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let mut server = Server::new(hourly());
    let s = std::sync::Arc::get_mut(&mut server).unwrap();
    s.stall = std::time::Duration::from_secs(10);
    s.stalls_left.store(1, Ordering::SeqCst);
    let port = spawn_server(server.clone());
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let day =
        atlas::chunks::unix_day(chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00+00:00").unwrap().timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, day, day).unwrap();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: stop.clone(),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let chunk = claimed(&main, GROUP, day);
    let stuck = server.clone();
    let stopper = std::thread::spawn(move || {
        while stuck.stalled.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        stop.store(true, Ordering::SeqCst);
    });
    let (settings, mut progress) = (Default::default(), |_: &atlas::indexer::Progress| {});
    let run = atlas::indexer::run_chunk(&ctx, &settings, &db, &chunk, 0, &mut progress);
    assert!(pool.block_on(atlas::nntp::unless_stopped(&ctx.stop, run)).is_none(), "dropped by stopping");
    stopper.join().unwrap();

    let state: i64 = atlas::db::open_at(&main)
        .unwrap()
        .query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0))
        .unwrap();
    assert_eq!(state, 0, "given back");
}

/// The split's oldest day, which no server keeps from its start: only the
/// server that goes back furthest on it indexes it, from its first article.
/// One whose retention starts later that day gives it back.
#[test]
fn only_the_server_going_back_furthest_does_the_oldest_day() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 3 days from 2026-01-01, from `from_hour` on
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |from_hour: u64, offset: u64| -> Vec<common::Post> {
        (from_hour..72)
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    // "127.0.0.1" keeps the first day from 01:00, "localhost" from 18:00
    let deep = spawn_server(Server::new(posts(1, 1)));
    let shallow = spawn_server(Server::new(posts(18, 1001)));
    let mut later = mock(shallow, "secret", 2, 1);
    later.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(deep, "secret", 2, 1), later]);
    pool.connect().unwrap();

    let day = atlas::chunks::unix_day(start.timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, day + 2, day).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let conn = atlas::db::open_at(&main).unwrap();
    let run = |server: usize| {
        let chunk = atlas::chunks::Claim { group: GROUP.into(), day, server: pool.pool.host(server), claimed_at: 1 };
        conn.execute(
            "update backfill_chunks set state = 1, server = ?, claimed_at = 1 where grp = ? and day = ?",
            rusqlite::params![chunk.server, GROUP, day],
        )
        .unwrap();
        pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, server, &mut |_| {}))
    };
    let state =
        || -> i64 { conn.query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0)).unwrap() };

    // the 18:00 server gets to it first, and again: it isnt the one
    for _ in 0..2 {
        let err = run(1).expect_err("the server going back less far did the oldest day");
        assert!(err.downcast_ref::<atlas::indexer::TooOld>().is_some(), "{err:#}");
        assert_eq!(state(), 0, "given back, still pending");
    }
    // the 01:00 one does it: 01:00 to midnight, plus the hour after
    let saved = run(0).unwrap();
    assert_eq!(saved.articles, 24);
    assert_eq!(state(), 2);
}

/// The server noted as going back furthest drops the group (GROUP answers
/// 411): the note goes, and the next deepest carrier does the oldest day
/// instead of it staying pending.
#[test]
fn the_oldest_day_moves_on_when_the_deepest_server_drops_the_group() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 3 days from 2026-01-01, from `from_hour` on
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |from_hour: u64, offset: u64| -> Vec<common::Post> {
        (from_hour..72)
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    // "127.0.0.1" keeps the first day from 01:00, "localhost" from 18:00
    let deep_server = Server::new(posts(1, 1));
    let deep = spawn_server(deep_server.clone());
    let shallow = spawn_server(Server::new(posts(18, 1001)));
    let mut later = mock(shallow, "secret", 2, 1);
    later.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(deep, "secret", 2, 1), later]);
    pool.connect().unwrap();

    let day = atlas::chunks::unix_day(start.timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, day + 2, day).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let conn = atlas::db::open_at(&main).unwrap();
    let run = |server: usize| {
        let chunk = atlas::chunks::Claim { group: GROUP.into(), day, server: pool.pool.host(server), claimed_at: 1 };
        conn.execute(
            "update backfill_chunks set state = 1, server = ?, claimed_at = 1 where grp = ? and day = ?",
            rusqlite::params![chunk.server, GROUP, day],
        )
        .unwrap();
        pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, server, &mut |_| {}))
    };
    let state =
        || -> i64 { conn.query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0)).unwrap() };

    // the 01:00 server is noted as the deepest
    assert!(run(1).unwrap_err().downcast_ref::<atlas::indexer::TooOld>().is_some());
    assert_eq!(atlas::chunks::deepest(&conn, GROUP).unwrap().as_deref(), Some(pool.pool.host(0).as_str()));

    // then drops the group, and finds out on its next chunk
    deep_server.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(run(0).unwrap_err().downcast_ref::<atlas::indexer::NotCarried>().is_some());
    assert_eq!(state(), 0);

    // the 18:00 one goes back furthest now: 18:00 to midnight, plus the hour after
    let saved = run(1).expect("the next deepest carrier does the oldest day");
    assert_eq!(saved.articles, 7);
    assert_eq!(state(), 2);
    assert_eq!(atlas::chunks::deepest(&conn, GROUP).unwrap().as_deref(), Some(pool.pool.host(1).as_str()));
}

/// The server noted as going back furthest still carries the group, but its
/// retention moved past the oldest day: it gives the day back, the note goes,
/// and the next deepest carrier does the oldest day instead of being turned
/// away by the stale note for good.
#[test]
fn the_oldest_day_moves_on_when_the_deepest_server_no_longer_keeps_it() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 3 days from 2026-01-01, from `from_hour` on
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |from_hour: u64, offset: u64| -> Vec<common::Post> {
        (from_hour..72)
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    // "127.0.0.1" keeps the first day from 01:00, "localhost" from 18:00
    let deep_server = Server::new(posts(1, 1));
    let deep = spawn_server(deep_server.clone());
    let shallow = spawn_server(Server::new(posts(18, 1001)));
    let mut later = mock(shallow, "secret", 2, 1);
    later.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(deep, "secret", 2, 1), later]);
    pool.connect().unwrap();

    let day = atlas::chunks::unix_day(start.timestamp());
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, day + 2, day).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let conn = atlas::db::open_at(&main).unwrap();
    let run = |server: usize| {
        let chunk = atlas::chunks::Claim { group: GROUP.into(), day, server: pool.pool.host(server), claimed_at: 1 };
        conn.execute(
            "update backfill_chunks set state = 1, server = ?, claimed_at = 1 where grp = ? and day = ?",
            rusqlite::params![chunk.server, GROUP, day],
        )
        .unwrap();
        pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, server, &mut |_| {}))
    };
    let state =
        || -> i64 { conn.query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0)).unwrap() };

    // the 01:00 server is noted as the deepest
    assert!(run(1).unwrap_err().downcast_ref::<atlas::indexer::TooOld>().is_some());
    assert_eq!(atlas::chunks::deepest(&conn, GROUP).unwrap().as_deref(), Some(pool.pool.host(0).as_str()));

    // then keeps only from the next day on, and finds out on its next chunk
    deep_server.posts.lock().unwrap().retain(|p| p.number > 25);
    assert!(run(0).unwrap_err().downcast_ref::<atlas::indexer::TooOld>().is_some());
    assert_eq!(state(), 0);
    assert_eq!(atlas::chunks::deepest(&conn, GROUP).unwrap(), None, "the stale note is forgotten");

    // the 18:00 one goes back furthest now: 18:00 to midnight, plus the hour after
    let saved = run(1).expect("the next deepest carrier does the oldest day");
    assert_eq!(saved.articles, 7);
    assert_eq!(state(), 2);
    assert_eq!(atlas::chunks::deepest(&conn, GROUP).unwrap().as_deref(), Some(pool.pool.host(1).as_str()));
}

/// A split made while the servers kept 5 days, then a server that keeps 10
/// joins: the older days get chunks, the old oldest day (done only from
/// where the deepest server then started) is pending again, and the new
/// deepest server does the new oldest day.
#[test]
fn a_split_reaches_back_when_a_deeper_server_joins() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 10 days from 2026-01-01, from `from_hour` on
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |from_hour: u64, offset: u64| -> Vec<common::Post> {
        (from_hour..240)
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    let short = spawn_server(Server::new(posts(121, 1)));
    let mut deep = mock(spawn_server(Server::new(posts(0, 10_001))), "secret", 2, 2);
    deep.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(short, "secret", 2, 1), deep]);
    pool.connect().unwrap();

    // the split from before: days 5 to 9, the oldest done by the home server
    let day0 = atlas::chunks::unix_day(start.timestamp());
    let conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&conn, GROUP, day0 + 9, day0 + 5).unwrap();
    conn.execute(
        "update backfill_chunks set state = 2, server = ?, done_at = 1 where day = ?",
        rusqlite::params![pool.pool.host(0), day0 + 5],
    )
    .unwrap();
    atlas::chunks::set_deepest(&conn, GROUP, &pool.pool.host(0), start.timestamp() + 121 * 3600).unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(atlas::db::open_at(&main).unwrap());
    let settings = atlas::indexer::PassSettings {
        mode: "backfill".into(),
        batch_size: 10,
        request_size: 10,
        split_min_backlog: 10,
        chunk_safety: 0,
    };
    pool.block_on(atlas::indexer::run_pass(&ctx, &settings, &db, GROUP, 0, &mut |_| {})).unwrap();

    assert_eq!(
        atlas::chunks::oldest_day(&conn, GROUP).unwrap(),
        Some(day0),
        "chunks back to the new server's first day"
    );
    let state = |day: i64| -> i64 {
        conn.query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0)).unwrap()
    };
    assert_eq!(state(day0 + 5), 0, "the old oldest day is to do again");
    // asked again (a chunk the pass ran asks who keeps the day): the new one
    assert_eq!(
        atlas::chunks::deepest(&conn, GROUP).unwrap().as_deref(),
        Some(pool.pool.host(1).as_str()),
        "the deepest server is asked again"
    );

    // and the new days get indexed: the new oldest by the server that has it
    for day in [day0 + 4, day0] {
        let chunk = atlas::chunks::Claim { group: GROUP.into(), day, server: pool.pool.host(1), claimed_at: 1 };
        conn.execute(
            "update backfill_chunks set state = 1, server = ?, claimed_at = 1 where grp = ? and day = ?",
            rusqlite::params![chunk.server, GROUP, day],
        )
        .unwrap();
        let saved = pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 1, &mut |_| {})).unwrap();
        assert!(saved.articles >= 24, "day {day}: {} articles", saved.articles);
        assert_eq!(state(day), 2);
    }
}

/// A split whose home cursor stayed where the split was made, and home's
/// retention has since moved past it (nothing is left at the cursor): a
/// deeper server joining still gets the older days chunks.
#[test]
fn a_split_reaches_back_when_home_no_longer_keeps_its_cursor() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 10 days from 2026-01-01, from `from_hour` on
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |from_hour: u64, offset: u64| -> Vec<common::Post> {
        (from_hour..240)
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    // home keeps 1122.. now, its backfill cursor stayed at 5
    let short = spawn_server(Server::new(posts(121, 1001)));
    let mut deep = mock(spawn_server(Server::new(posts(0, 10_001))), "secret", 2, 2);
    deep.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(short, "secret", 2, 1), deep]);
    pool.connect().unwrap();

    let day0 = atlas::chunks::unix_day(start.timestamp());
    let conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&conn, GROUP, day0 + 9, day0 + 5).unwrap();
    atlas::chunks::set_deepest(&conn, GROUP, &pool.pool.host(0), start.timestamp() + 121 * 3600).unwrap();
    let state = atlas::db::GroupState { live_cursor: 1240, backfill_cursor: 5 };
    atlas::db::save_group_state(&conn, &format!("{GROUP}@{}", pool.pool.host(0).to_lowercase()), state).unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(atlas::db::open_at(&main).unwrap());
    let settings = atlas::indexer::PassSettings {
        mode: "backfill".into(),
        batch_size: 10,
        request_size: 10,
        split_min_backlog: 10,
        chunk_safety: 0,
    };
    pool.block_on(atlas::indexer::run_pass(&ctx, &settings, &db, GROUP, 0, &mut |_| {})).unwrap();

    assert_eq!(
        atlas::chunks::oldest_day(&conn, GROUP).unwrap(),
        Some(day0),
        "chunks back to the new server's first day"
    );
}

/// A split reaches back to the oldest day of every carrier, also one whose
/// GROUP low mark sits 5,000 numbers before its first article (retention
/// has moved on, the mark hasnt): its older days get chunks.
#[test]
fn a_split_reaches_a_carrier_whose_low_mark_lags() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 10 days from 2026-01-01, from `from_hour` on
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |from_hour: u64, offset: u64| -> Vec<common::Post> {
        (from_hour..240)
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    // home keeps the last 5 days, "localhost" all 10 with a low mark 5,000 behind
    let short = spawn_server(Server::new(posts(120, 1)));
    let mut deep = Server::new(posts(0, 10_001));
    std::sync::Arc::get_mut(&mut deep).unwrap().reported_low = Some(5_001);
    let mut lagging = mock(spawn_server(deep), "secret", 2, 2);
    lagging.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(short, "secret", 2, 1), lagging]);
    pool.connect().unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(atlas::db::open_at(&main).unwrap());
    let settings = atlas::indexer::PassSettings {
        mode: "backfill".into(),
        batch_size: 10,
        request_size: 10,
        split_min_backlog: 10,
        chunk_safety: 0,
    };
    pool.block_on(atlas::indexer::run_pass(&ctx, &settings, &db, GROUP, 0, &mut |_| {})).unwrap();

    let conn = atlas::db::open_at(&main).unwrap();
    let day = atlas::chunks::unix_day(start.timestamp());
    assert_eq!(
        atlas::chunks::oldest_day(&conn, GROUP).unwrap(),
        Some(day),
        "chunks back to the deep server's first day"
    );
}

/// The article at the home cursor has a forged Date from 1990: no split is
/// made of it (it would be one chunk for 2000-01-01 that no server keeps,
/// and the group's history would never be indexed), the normal backfill goes on.
#[test]
fn a_forged_old_date_at_the_cursor_doesnt_split() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour for 10 days from 2026-01-01, the newest dated 1990
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |offset: u64| -> Vec<common::Post> {
        (0..240u64)
            .map(|h| {
                let when = match h {
                    239 => "Mon, 01 Jan 1990 00:00:00 +0000".to_string(),
                    _ => (start + chrono::Duration::hours(h as i64)).to_rfc2822(),
                };
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when)
            })
            .collect()
    };
    let mut other = mock(spawn_server(Server::new(posts(10_001))), "secret", 2, 2);
    other.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(spawn_server(Server::new(posts(1))), "secret", 2, 1), other]);
    pool.connect().unwrap();

    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(atlas::db::open_at(&main).unwrap());
    let settings = atlas::indexer::PassSettings {
        mode: "backfill".into(),
        batch_size: 10,
        request_size: 10,
        split_min_backlog: 10,
        chunk_safety: 0,
    };
    let saved = pool.block_on(atlas::indexer::run_pass(&ctx, &settings, &db, GROUP, 0, &mut |_| {})).unwrap();

    let conn = atlas::db::open_at(&main).unwrap();
    assert!(!atlas::chunks::is_split(&conn, GROUP).unwrap(), "split from a forged date");
    assert!(saved.articles > 0, "the normal backfill goes on");
}

/// A carrier that comes to keep more of the split's oldest day (from 01:00
/// where the day was done from 18:00): the day isnt older, but it is to do
/// again, by that carrier, soo 01:00 to 18:00 gets indexed.
#[test]
fn a_split_redoes_its_oldest_day_when_a_server_goes_back_further_in_it() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // posts every hour from 2026-01-01 00:00, the hours in `hours`
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = |hours: std::ops::Range<u64>, offset: u64| -> Vec<common::Post> {
        hours
            .map(|h| {
                let when = start + chrono::Duration::hours(h as i64);
                post_at(offset + h, &format!(r#""p{h}.bin" yEnc (1/1)"#), &when.to_rfc2822())
            })
            .collect()
    };
    // both keep the group from Jan 1 18:00 at first
    let deep = Server::new(posts(18..240, 10_001));
    let short = spawn_server(Server::new(posts(18..240, 1)));
    let mut later = mock(spawn_server(deep.clone()), "secret", 2, 2);
    later.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(short, "secret", 2, 1), later]);
    pool.connect().unwrap();

    let ctx = || atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(atlas::db::open_at(&main).unwrap());
    let settings = atlas::indexer::PassSettings {
        mode: "backfill".into(),
        batch_size: 10,
        request_size: 10,
        split_min_backlog: 10,
        chunk_safety: 0,
    };
    let conn = atlas::db::open_at(&main).unwrap();
    let day0 = atlas::chunks::unix_day(start.timestamp());
    let state = |day: i64| -> i64 {
        conn.query_row("select state from backfill_chunks where day = ?", [day], |r| r.get(0)).unwrap()
    };
    let run_day = |day: i64, server: usize| {
        let host = if server == 0 { "127.0.0.1" } else { "localhost" };
        let chunk = atlas::chunks::Claim { group: GROUP.into(), day, server: host.into(), claimed_at: 1 };
        conn.execute(
            "update backfill_chunks set state = 1, server = ?, claimed_at = 1 where grp = ? and day = ?",
            rusqlite::params![host, GROUP, day],
        )
        .unwrap();
        pool.block_on(atlas::indexer::run_chunk(&ctx(), &exact(), &db, &chunk, server, &mut |_| {})).unwrap()
    };

    // the split, and its oldest day done from 18:00 by the home server
    pool.block_on(atlas::indexer::run_pass(&ctx(), &settings, &db, GROUP, 0, &mut |_| {})).unwrap();
    assert_eq!(atlas::chunks::oldest_day(&conn, GROUP).unwrap(), Some(day0));
    assert_eq!(run_day(day0, 0).articles, 7, "18:00 to midnight and the hour after");
    assert_eq!(state(day0), 2);

    // "localhost" keeps the day from 01:00 now
    let mut older = posts(1..18, 10_001);
    older.extend(deep.posts.lock().unwrap().drain(..));
    *deep.posts.lock().unwrap() = older;

    pool.block_on(atlas::indexer::run_pass(&ctx(), &settings, &db, GROUP, 0, &mut |_| {})).unwrap();
    assert_eq!(atlas::chunks::oldest_day(&conn, GROUP).unwrap(), Some(day0), "no older day");
    assert_eq!(state(day0), 0, "the oldest day is to do again");
    // asked again (a chunk the pass ran asks who keeps the day): the new one
    assert_eq!(
        atlas::chunks::deepest(&conn, GROUP).unwrap().as_deref(),
        Some(pool.pool.host(1).as_str()),
        "the deepest server is asked again"
    );
    assert_eq!(run_day(day0, 1).articles, 24, "01:00 to midnight and the hour after");
    assert_eq!(state(day0), 2);
}

/// (number, unix seconds) of a post
type Slot = (u64, i64);

fn jan_1() -> i64 {
    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap().timestamp()
}

fn slot_posts(list: &[Slot]) -> Vec<common::Post> {
    list.iter()
        .map(|&(n, t)| {
            let when = chrono::DateTime::from_timestamp(t, 0).unwrap();
            post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &when.to_rfc2822())
        })
        .collect()
}

/// numbers `gap` apart, somewhere in 100..=500 (a fixed pseudo random walk)
fn sparse_step(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    100 + (*state >> 33) % 401
}

/// `count` posts a minute apart from `(n, t)` on, `step` numbers apart
fn run(list: &mut Vec<Slot>, (mut n, mut t): Slot, count: usize, mut step: impl FnMut() -> u64) -> Slot {
    for _ in 0..count {
        list.push((n, t));
        n += step();
        t += 60;
    }
    (n, t)
}

/// 1..=50 from Jan 1, a hole to 3,000,000, 3,000 posts with 100 to 500
/// numbers between them from Jan 3, then 100,000 posts side by side
fn hole_then_sparse_then_dense() -> Vec<Slot> {
    let mut list = Vec::new();
    let end = run(&mut list, (1, jan_1()), 50, || 1);
    assert_eq!(end.0, 51);
    let (mut seed, start) = (7, (3_000_001, jan_1() + 2 * 86_400));
    let next = run(&mut list, start, 3_000, || sparse_step(&mut seed));
    run(&mut list, next, 100_000, || 1);
    list
}

/// 20,000 posts side by side from Jan 1, 3,000 with 100 to 500 numbers
/// between them, a hole of 3,000,000, then 100,000 posts side by side
fn dense_then_sparse_then_hole() -> Vec<Slot> {
    let mut list = Vec::new();
    let next = run(&mut list, (1, jan_1()), 20_000, || 1);
    let mut seed = 11;
    let (n, t) = run(&mut list, next, 3_000, || sparse_step(&mut seed));
    run(&mut list, (n + 3_000_000, t), 100_000, || 1);
    list
}

/// Every time in `whens` finds exactly the first post at or after it.
fn finds_the_first_post_for_each(list: &[Slot], lows: &[u64], whens: &[i64]) {
    let server = Server::new(slot_posts(list));
    let port = spawn_server(server.clone());
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();
    let high = list.last().unwrap().0;
    for &low in lows {
        for &when in whens {
            let before = server.xovers.load(std::sync::atomic::Ordering::SeqCst);
            let got = pool.block_on(pool.pool.article_at(0, GROUP, low, high, when)).unwrap();
            let used = server.xovers.load(std::sync::atomic::Ordering::SeqCst) - before;
            let want = list.iter().find(|&&(n, t)| n >= low && t >= when).map_or(high + 1, |&(n, _)| n);
            assert_eq!(got, want, "low {low}, time {when}");
            assert!(used <= 150, "{used} requests to find {got}");
        }
    }
}

/// Indexing each of `days` saves every post of the day and the hour around it.
fn chunks_save_every_post_of(list: &[Slot], days: &[i64]) {
    for &day in days {
        let home = tempfile::tempdir().unwrap();
        let main = home.path().join("atlas.db");
        atlas::db::create_db_at(&main).unwrap();
        let port = spawn_server(Server::new(slot_posts(list)));
        let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
        pool.connect().unwrap();
        let main_conn = atlas::db::open_at(&main).unwrap();
        // between two other days: the oldest and newest reach the marks
        atlas::chunks::add(&main_conn, GROUP, day + 1, atlas::chunks::unix_day(list[0].1).min(day - 1)).unwrap();
        let ctx = atlas::indexer::PassContext {
            pool: pool.pool.clone(),
            states: Default::default(),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            verbose: false,
        };
        let db = atlas::indexer::shared_db(main_conn);
        let chunk = claimed(&main, GROUP, day);
        let saved = pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {})).unwrap();
        let (from, to) = (day * 86_400 - 3600, (day + 1) * 86_400 + 3600);
        let want = list.iter().filter(|&&(_, t)| t >= from && t < to).count() as i64;
        assert_eq!(saved.articles, want, "day {day}");
    }
}

fn days_from(list: &[Slot], first: usize, count: usize) -> Vec<i64> {
    let mut days: Vec<i64> = list[first..].iter().map(|&(_, t)| atlas::chunks::unix_day(t)).collect();
    days.dedup();
    days.truncate(count);
    days
}

/// A hole, then articles hundreds of numbers apart, then a crowd: one empty
/// window must not make the bisect skip the sparse articles after the hole.
#[test]
fn a_sparse_region_after_a_hole_is_not_skipped() {
    let list = hole_then_sparse_then_dense();
    let whens: Vec<i64> = list.iter().skip(50).take(3_200).step_by(7).map(|&(_, t)| t).collect();
    finds_the_first_post_for_each(&list, &[1, 1_500_000], &whens);
    chunks_save_every_post_of(&list, &days_from(&list, 50, 5));
}

/// A crowd, articles hundreds of numbers apart, then a hole: the last article
/// before the hole is the sparse one, not one a window further back.
#[test]
fn a_sparse_region_before_a_hole_is_not_skipped() {
    let list = dense_then_sparse_then_hole();
    let whens: Vec<i64> = list.iter().skip(19_000).take(4_500).step_by(7).map(|&(_, t)| t).collect();
    finds_the_first_post_for_each(&list, &[1], &whens);
    chunks_save_every_post_of(&list, &days_from(&list, 19_000, 5));
}

/// One article above the first midpoint dated far too early (a forged Date)
/// doesnt move the search past the articles below it.
#[test]
fn a_forged_early_date_at_a_midpoint_doesnt_skip_the_articles_below_it() {
    // the search for hour 700 probes 501 (older), then 751: forged to the start
    let posts: Vec<common::Post> = with_gap(1, 1000, 0..=0)
        .into_iter()
        .map(|mut p| {
            if p.number == 751 {
                p.date = "Thu, 01 Jan 2026 00:00:00 +0000".into();
            }
            p
        })
        .collect();
    assert_eq!(search(posts, 1, 1000, hour_of(700)), 700);
}

/// posts 1..=240 one an hour from 2026-01-01, 100..=200 forged to `forged`;
/// day 2026-01-10 is 217..=240
fn forged_run(forged: &str) -> Vec<common::Post> {
    with_gap(1, 240, 0..=0)
        .into_iter()
        .map(|mut p| {
            if (100..=200).contains(&p.number) {
                p.date = forged.into();
            }
            p
        })
        .collect()
}

/// The day chunk for 2026-01-10 over `posts`: the numbers of it that were saved
fn chunk_of_jan_10(posts: Vec<common::Post>) -> Vec<u64> {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let port = spawn_server(Server::new(posts));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    let day = atlas::chunks::unix_day(hour_of(217));
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, day, day).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    let chunk = claimed(&main, GROUP, day);
    pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {})).unwrap();

    let conn = atlas::db::open_with_shards(&main).unwrap();
    assert_eq!(atlas::chunks::progress(&conn, GROUP).unwrap(), (1, 1));
    let shard = atlas::store::shard_of(GROUP);
    let mut q = conn.prepare(&format!("select subject from s{shard}.files")).unwrap();
    // subjects are "pN.bin" yEnc (1/1)
    let mut saved: Vec<u64> = q
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| s.unwrap().trim_start_matches("\"p").split('.').next().unwrap().parse().unwrap())
        .collect();
    saved.sort();
    saved
}

/// A run of posts with Dates from the future (later than now) is passed over
/// by the search: the day's posts after it are all fetched.
#[test]
fn a_run_of_future_dates_doesnt_empty_the_day() {
    let saved = chunk_of_jan_10(forged_run("Mon, 01 Jan 2035 00:00:00 +0000"));
    assert!((216..=240).all(|n| saved.contains(&n)), "the day's posts are missing: {saved:?}");
}

/// A run of posts with Dates after the day but not in the future: the search
/// finds the day empty, its posts are in the chunk next to it.
#[test]
fn a_run_of_later_dates_doesnt_empty_the_day() {
    let saved = chunk_of_jan_10(forged_run("Mon, 01 Jun 2026 00:00:00 +0000"));
    assert!((216..=240).all(|n| saved.contains(&n)), "the day's posts are missing: {saved:?}");
}

/// posts 1..=23 hourly on 2026-01-01, 24..=2023 forged to 2026-06-01, then
/// 2024..=2047 hourly on 2026-01-02 and 2048..=2071 on 2026-01-03: the date
/// search for 2026-01-02 and 2026-01-03 both land on 24
fn long_forged_run() -> Vec<Slot> {
    let mut list: Vec<Slot> = (1..=23).map(|n| (n, jan_1() + n as i64 * 3600)).collect();
    list.extend((24..=2023).map(|n| (n, unix("2026-06-01T00:00:00+00:00"))));
    list.extend((2024..=2071).map(|n| (n, jan_1() + 86_400 + (n as i64 - 2024) * 3600)));
    list
}

/// The numbers saved by indexing every day chunk of a split of `days`
/// (oldest first) over `list` on one server.
fn saved_by_every_chunk(list: &[Slot], days: std::ops::RangeInclusive<i64>) -> Vec<u64> {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let port = spawn_server(Server::new(slot_posts(list)));
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();
    let main_conn = atlas::db::open_at(&main).unwrap();
    atlas::chunks::add(&main_conn, GROUP, *days.end(), *days.start()).unwrap();
    let ctx = atlas::indexer::PassContext {
        pool: pool.pool.clone(),
        states: Default::default(),
        stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        verbose: false,
    };
    let db = atlas::indexer::shared_db(main_conn);
    for day in days.clone() {
        let chunk = claimed(&main, GROUP, day);
        pool.block_on(atlas::indexer::run_chunk(&ctx, &exact(), &db, &chunk, 0, &mut |_| {})).unwrap();
    }
    let conn = atlas::db::open_with_shards(&main).unwrap();
    let total = days.end() - days.start() + 1;
    assert_eq!(atlas::chunks::progress(&conn, GROUP).unwrap(), (total, total));
    let shard = atlas::store::shard_of(GROUP);
    let mut q = conn.prepare(&format!("select subject from s{shard}.files")).unwrap();
    let mut saved: Vec<u64> = q
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| s.unwrap().trim_start_matches("\"p").split('.').next().unwrap().parse().unwrap())
        .collect();
    saved.sort();
    saved
}

/// 2,000 forged later Dates in a row, more than any look around a day's
/// range reaches: every post of the split is still fetched by some chunk.
#[test]
fn a_long_run_of_later_dates_doesnt_lose_the_days_after_it() {
    let list = long_forged_run();
    let day = atlas::chunks::unix_day(jan_1());
    let saved = saved_by_every_chunk(&list, day..=day + 2);
    let missing: Vec<u64> = list.iter().map(|&(n, _)| n).filter(|n| !saved.contains(n)).collect();
    assert!(missing.is_empty(), "{} posts missing, from {:?}", missing.len(), missing.first());
}

/// Over posts with runs of forged Dates at random (earlier, later, and
/// future), the ranges of a split's day chunks leave no number from the
/// low mark to the high mark out.
#[test]
fn day_chunk_ranges_cover_every_number_whatever_the_forged_runs() {
    let mut seed = 0x5eed_u64;
    let mut next = |below: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % below
    };
    for case in 0..60 {
        // six days of posts 20 minutes apart, numbers 1 to 3 apart
        let mut list: Vec<Slot> = Vec::new();
        let (mut n, mut t) = (1 + next(50), jan_1());
        while t < jan_1() + 6 * 86_400 {
            list.push((n, t));
            n += 1 + next(3);
            t += 1200;
        }
        let (oldest_day, newest_day) = (atlas::chunks::unix_day(list[0].1), atlas::chunks::unix_day(t - 1));
        // up to four runs of forged Dates, up to 1,500 posts long
        for _ in 0..=next(4) {
            let (at, len) = (next(list.len() as u64) as usize, 1 + next(1500) as usize);
            let forged = match next(3) {
                0 => unix("2025-03-01T00:00:00+00:00"),
                1 => jan_1() + next(6 * 86_400) as i64,
                _ => unix("2035-01-01T00:00:00+00:00"),
            };
            for slot in list.iter_mut().skip(at).take(len) {
                slot.1 = forged;
            }
        }

        let port = spawn_server(Server::new(slot_posts(&list)));
        let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
        pool.connect().unwrap();
        let (low, high) = (list[0].0, list.last().unwrap().0);
        let mut ranges: Vec<(u64, u64)> = (oldest_day..=newest_day)
            .map(|day| {
                let range = atlas::indexer::chunk_range(
                    &pool.pool,
                    0,
                    GROUP,
                    (low, high),
                    day,
                    (oldest_day, newest_day),
                    (high, 0),
                );
                pool.block_on(range).unwrap()
            })
            .filter(|(start, end)| start <= end)
            .collect();
        ranges.sort();
        let mut covered = low - 1;
        for (start, end) in ranges {
            assert!(start <= covered + 1, "case {case}: {} to {} in no chunk", covered + 1, start - 1);
            covered = covered.max(end);
        }
        assert_eq!(covered, high, "case {case}: the chunks stop short of the high mark");
    }
}

/// Adjacent day chunks done on two servers whose numbering differs (one has
/// a hole near the day boundary) with a run of forged Dates across the
/// boundary: every article between them is in one or the other.
#[test]
fn adjacent_days_on_two_servers_leave_no_article_between_them() {
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    // a post a minute for three days; a run before the day 1 / day 2
    // boundary dated a day after the last
    let boundary = 2 * 1440u64;
    let forged = (boundary - 400)..(boundary - 250);
    let date = |i: u64| {
        let when = if forged.contains(&i) {
            start + chrono::Duration::hours(3 * 24 + 12)
        } else {
            start + chrono::Duration::minutes(i as i64)
        };
        when.to_rfc2822()
    };
    // "127.0.0.1" numbers them from 1, "localhost" from 50_001 with a hole
    // of a billion numbers before the run
    let number_b = |i: u64| if i < boundary - 500 { 50_001 + i } else { 1_000_050_001 + i };
    let posts = |number: &dyn Fn(u64) -> u64| -> Vec<common::Post> {
        (0..3 * 1440u64)
            .map(|i| {
                let mut p = post_at(number(i), &format!(r#""p{i}.bin" yEnc (1/1)"#), &date(i));
                p.message_id = format!("<m{i}@x>");
                p
            })
            .collect()
    };
    let (pa, pb) = (spawn_server(Server::new(posts(&|i| i + 1))), spawn_server(Server::new(posts(&number_b))));
    let mut b = mock(pb, "secret", 2, 2);
    b.host = "localhost".into();
    let pool = BlockingPool::new(&[mock(pa, "secret", 2, 1), b]);
    pool.connect().unwrap();

    let day0 = atlas::chunks::unix_day(start.timestamp());
    let days = (day0, day0 + 2);
    let (a_marks, b_marks) = ((1, 3 * 1440), (50_001, number_b(3 * 1440 - 1)));
    let range = |server: usize, marks: (u64, u64), day: i64, safety: u64| {
        pool.block_on(atlas::indexer::chunk_range(&pool.pool, server, GROUP, marks, day, days, (marks.1, safety)))
            .unwrap()
    };
    // day 1 on "localhost", day 2 on "127.0.0.1"
    let lost = |safety: u64| -> Vec<u64> {
        let (b_start, b_end) = range(1, b_marks, day0 + 1, safety);
        let (a_start, a_end) = range(0, a_marks, day0 + 2, safety);
        (1440 + 120..3 * 1440)
            .filter(|&i| !(b_start..=b_end).contains(&number_b(i)) && !(a_start..=a_end).contains(&(i + 1)))
            .collect()
    };
    // the date searches alone leave a gap: the forged run moves them differently
    assert!(!lost(0).is_empty());
    let lost = lost(atlas::indexer::CHUNK_SAFETY);
    assert!(lost.is_empty(), "{} lost, from {:?}", lost.len(), lost.first());
}
