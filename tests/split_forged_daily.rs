//! A split group on two servers, one with an article dated in every day
//! of the six years before its real ones (forged): each of those thousands of
//! days looks corroborated by its one article. Only a window of the days
//! older than the other server goes back is serviced as chunks at once, and
//! one article a day doesnt earn more: the rest is left to the deep
//! server's sweep, which still indexes every article.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post_at, spawn_server};

/// 40 posts a day for 30 days from 2026-01-01, numbered from `offset`
fn posts(offset: u64) -> Vec<common::Post> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    (0..1200u64)
        .map(|i| {
            let when = start + chrono::Duration::minutes(i as i64 * 36);
            let mut p = post_at(
                offset + i,
                &format!(r#""rel{}.part{}.rar" yEnc (1/1)"#, i / 10, i % 10 + 1),
                &when.to_rfc2822(),
            );
            p.message_id = format!("<post{i}@daily>");
            p
        })
        .collect()
}

#[test]
fn forged_dates_one_a_day_for_six_years_service_only_a_window_of_chunks() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    // one article noon of every day from 2020-01-05 to 2025-12-31, then the
    // real ones: thousands of days, few enough articles for a slow runner
    let first = chrono::DateTime::parse_from_rfc3339("2020-01-05T12:00:00+00:00").unwrap();
    let split_start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let forged_days = (split_start - first).num_days() as u64 + 1;
    let mut deep: Vec<common::Post> = (0..forged_days)
        .map(|d| {
            let when = first + chrono::Duration::days(d as i64);
            let mut p = post_at(400_001 + d, &format!(r#""old{d}.rar" yEnc (1/1)"#), &when.to_rfc2822());
            p.message_id = format!("<old{d}@daily>");
            p
        })
        .collect();
    deep.extend(posts(400_001 + forged_days));
    let (pc, pd) = (spawn_server(Server::new(posts(1))), spawn_server(Server::new(deep)));
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": pc, "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": pd, "ssl": false, "connections": 4, "priority": 1}
        ],
        "groups": [GROUP],
        "index_mode": "backfill",
        "batch_size": 100,
        "request_size": 50,
        "split_min_backlog": 200
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    let keys = atlas::nntp::server_keys(&cfg.servers);
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    let main = home.path().join("atlas.db");
    let every = 1200 + forged_days as i64;
    // a 2-core runner services its window of chunks many times slower
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        let (_, articles) = atlas::store::totals(&conn).unwrap();
        let (done, total) = atlas::chunks::progress(&conn, GROUP).unwrap_or((0, 0));
        let swept = keys.iter().all(|h| atlas::chunks::swept(&conn, GROUP, h).unwrap());
        if total > 0 && done == total && swept && articles == every {
            break;
        }
        assert!(Instant::now() < deadline, "articles {articles} of {every}, chunks {done}/{total}, swept {swept}");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);

    let conn = atlas::db::open_with_shards(&main).unwrap();
    let speculative: i64 = conn
        .query_row(
            "select count(*) from backfill_chunks where grp = ? and state = 2 and day < ?",
            rusqlite::params![GROUP, atlas::chunks::unix_day(split_start.timestamp())],
            |r| r.get(0),
        )
        .unwrap();
    assert!(speculative <= 60, "{speculative} days older than the other server goes back were serviced");
}
