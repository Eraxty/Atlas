//! A split group on two servers that reaches back 200 days before either
//! keeps anything, as one made from forged Date headers in a carrier's
//! first windows would: once every carrier said it doesnt keep those days
//! they are dropped, soo the split completes (its sweeps run) and every
//! post is indexed, instead of the days waiting for a server forever.
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
            p.message_id = format!("<post{i}@forged>");
            p
        })
        .collect()
}

#[test]
fn days_no_carrier_keeps_are_dropped() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    let (pa, pb) = (spawn_server(Server::new(posts(1, 0))), spawn_server(Server::new(posts(500_001, 0))));
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": pa, "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": pb, "ssl": false, "connections": 4, "priority": 1}
        ],
        "groups": [GROUP],
        "index_mode": "backfill",
        "batch_size": 100,
        "request_size": 50,
        "split_min_backlog": 200
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    drop(atlas::db::create_db().unwrap());
    let main = home.path().join("atlas.db");
    let first_day = atlas::chunks::unix_day(1_767_225_600);
    {
        // the 30 days and 200 before them that no server keeps
        let conn = atlas::db::open_at(&main).unwrap();
        atlas::chunks::add(&conn, GROUP, first_day + 29, first_day - 200).unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    let keys = atlas::nntp::server_keys(&cfg.servers);
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        let (_, articles) = atlas::store::totals(&conn).unwrap();
        let (done, total) = atlas::chunks::progress(&conn, GROUP).unwrap_or((0, 0));
        let swept = keys.iter().any(|h| atlas::chunks::swept(&conn, GROUP, h).unwrap());
        if total > 0 && done == total && articles == 1200 && swept {
            break;
        }
        assert!(Instant::now() < deadline, "articles {articles}, chunks {done}/{total}, swept {swept}");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);
    let conn = atlas::db::open_with_shards(&main).unwrap();
    assert_eq!(atlas::store::totals(&conn).unwrap().1, 1200, "every post once");
    let oldest = atlas::chunks::oldest_day(&conn, GROUP).unwrap().unwrap();
    assert!(oldest >= first_day - 1, "the days nobody keeps are gone: {oldest} vs {first_day}");
}
