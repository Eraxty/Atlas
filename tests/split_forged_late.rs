//! A split group on two servers, the one that keeps all 30 days with a
//! forged Date on its first article (days later than the truth): it still
//! counts as going back furthest, and every one of its days is indexed.
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
fn a_forged_late_date_on_the_deepest_servers_first_article_loses_no_days() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    // "127.0.0.1" keeps all 30 days, its first article dated day 25;
    // "localhost" keeps the last 20
    let mut long_posts = posts(1, 0);
    long_posts[0].date = "Mon, 26 Jan 2026 00:00:00 +0000".into();
    let long = Server::new(long_posts);
    let short = Server::new(posts(500_001, 10));
    let (pl, ps) = (spawn_server(long), spawn_server(short));
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
}
