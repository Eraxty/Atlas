//! A split group on two servers, one with its first 300 articles dated in
//! 2000 (forged): the split reaches back thousands of days no article is
//! dated in. Chunks are only serviced down to where an article was found,
//! the rest dropped, soo the split completes (its sweeps run, which index
//! the forged-dated articles) after a handful of chunks, not thousands.
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
fn forged_dates_in_2000_make_only_a_few_chunks() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    let clean = Server::new(posts(1, 0));
    let mut forged_posts = posts(500_001, 0);
    for p in forged_posts.iter_mut().take(300) {
        p.date = "Wed, 05 Jan 2000 00:00:00 +0000".into();
    }
    let (pc, pf) = (spawn_server(clean), spawn_server(Server::new(forged_posts)));
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": pc, "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": pf, "ssl": false, "connections": 4, "priority": 1}
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
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        let (_, articles) = atlas::store::totals(&conn).unwrap();
        let (done, total) = atlas::chunks::progress(&conn, GROUP).unwrap_or((0, 0));
        let swept = keys.iter().all(|h| atlas::chunks::swept(&conn, GROUP, h).unwrap());
        if total > 0 && done == total && swept && articles >= 1200 {
            break;
        }
        assert!(Instant::now() < deadline, "articles {articles}, chunks {done}/{total}, swept {swept}");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);
    let conn = atlas::db::open_with_shards(&main).unwrap();
    let chunks: i64 = conn.query_row("select count(*) from backfill_chunks", [], |r| r.get(0)).unwrap();
    assert!(chunks < 100, "{chunks} chunks left of a split reaching back to 2000");
}
