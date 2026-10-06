//! A split group on two servers, one of which only keeps the last 10 of its
//! 30 days: the days older than that are all done by the other server, with
//! every article, and the short server doesnt keep grabbing them.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post_at, spawn_server};

/// 40 posts a day for 30 days from 2026-01-01, numbered from `offset`,
/// only those from day `from_day` on
fn posts(offset: u64, from_day: u64) -> Vec<common::Post> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    (from_day * 40..1200u64)
        .map(|i| {
            let when = start + chrono::Duration::minutes(i as i64 * 36);
            let mut p = post_at(
                offset + i,
                &format!(r#""rel{}.part{}.rar" yEnc (1/1)"#, i / 10, i % 10 + 1),
                &when.to_rfc2822(),
            );
            p.message_id = format!("<post{i}@retention>");
            p
        })
        .collect()
}

#[test]
fn days_older_than_a_servers_retention_go_to_the_server_that_has_them() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    let slow = |mut s: Arc<Server>| {
        Arc::get_mut(&mut s).unwrap().xover_delay = Duration::from_millis(10);
        s
    };
    // "127.0.0.1" keeps all 30 days, "localhost" the last 10
    let long = slow(Server::new(posts(1, 0)));
    let short = slow(Server::new(posts(500_001, 20)));
    let (pl, ps) = (spawn_server(long.clone()), spawn_server(short.clone()));
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": pl, "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": ps, "ssl": false, "connections": 4, "priority": 1}
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
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    let main = home.path().join("atlas.db");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        let (_, articles) = atlas::store::totals(&conn).unwrap();
        let (done, total) = atlas::chunks::progress(&conn, GROUP).unwrap_or((0, 0));
        if total > 0 && done == total {
            break;
        }
        assert!(Instant::now() < deadline, "articles {articles}, chunks {done}/{total}");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);

    let conn = atlas::db::open_with_shards(&main).unwrap();
    assert_eq!(atlas::store::totals(&conn).unwrap().1, 1200, "every post once");
    let first_short_day =
        atlas::chunks::unix_day(chrono::DateTime::parse_from_rfc3339("2026-01-21T00:00:00+00:00").unwrap().timestamp());
    let old_by_short: i64 = conn
        .query_row(
            "select count(*) from backfill_chunks where server = ? and day < ?",
            rusqlite::params![format!("localhost:{ps}"), first_short_day],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_by_short, 0, "the short server finished no day it doesnt keep");
    let short_xovers = short.xovers.load(Ordering::SeqCst);
    assert!(short_xovers < 1500, "the short server kept trying old days: {short_xovers} requests");
}
