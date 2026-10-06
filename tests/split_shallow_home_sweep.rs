//! A split group on two servers, its home the one that keeps only the last
//! 20 days with forged Dates from the first of the 30 in each of its first
//! windows: it looks as deep as the other and its day chunks miss what it
//! doesnt keep (here: every chunk done before indexing starts, as such a
//! server finishes them empty). The home's sweep stops at its own first
//! article; once the chunks are done the deep server sweeps its own
//! numbers too, every post is indexed and the split is complete once both
//! servers' sweeps are.
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
fn deep_only_posts_a_shallow_home_missed_are_swept_by_the_deep_server() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    // "127.0.0.1" keeps all 30 days; "localhost" (the group's home) the last
    // 20, with 4 posts in each of its first 3 windows of 100 numbers dated
    // the first day
    let long = Server::new(posts(500_001, 0));
    let mut short_posts = posts(1, 10);
    for window in 0..3 {
        for n in 0..4 {
            short_posts[window * 100 + n * 20].date = "Thu, 01 Jan 2026 00:30:00 +0000".into();
        }
    }
    let short = Server::new(short_posts);
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
    drop(atlas::db::create_db().unwrap());
    let main = home.path().join("atlas.db");
    {
        // the 30 days split and done by the shallow home, nothing indexed
        let conn = atlas::db::open_at(&main).unwrap();
        let first_day = atlas::chunks::unix_day(1_767_225_600);
        atlas::chunks::add(&conn, GROUP, first_day + 29, first_day).unwrap();
        let now = chrono::Utc::now().timestamp();
        while let Some(claim) =
            atlas::chunks::claim(&conn, &[(GROUP.to_string(), i64::MIN)], &format!("localhost:{ps}"), now).unwrap()
        {
            assert!(atlas::chunks::finish(&conn, &claim, now).unwrap());
        }
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
        let complete = atlas::chunks::complete(&conn, GROUP, &keys).unwrap();
        if total > 0 && done == total && articles == 1200 && complete {
            break;
        }
        assert!(Instant::now() < deadline, "articles {articles}, chunks {done}/{total}, complete {complete}");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);
    let conn = atlas::db::open_with_shards(&main).unwrap();
    assert_eq!(atlas::store::totals(&conn).unwrap().1, 1200, "every post once");
}
