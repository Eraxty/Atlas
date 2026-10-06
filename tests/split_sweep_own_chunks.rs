//! Two split groups: one whose day chunks are all done (the forged shallow
//! server of split_forged_sweep finished them empty) and another whose
//! chunk is still running on a server not indexing here. The first group's
//! sweep doesnt wait for the other's chunks, only for its own: every post of
//! it is indexed.
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
fn a_groups_sweep_waits_only_for_its_own_chunks() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    // "localhost" (the group's home) keeps all 30 days; "127.0.0.1" the last
    // 20, with 4 posts in each of its first 3 windows of 100 numbers dated
    // the first day
    let long = Server::new(posts(1, 0));
    let mut short_posts = posts(500_001, 10);
    for window in 0..3 {
        for n in 0..4 {
            short_posts[window * 100 + n * 20].date = "Thu, 01 Jan 2026 00:30:00 +0000".into();
        }
    }
    let short = Server::new(short_posts);
    let (pl, ps) = (spawn_server(long), spawn_server(short));
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": ps, "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": pl, "ssl": false, "connections": 4, "priority": 1}
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
        // the 30 days split and done by the shallow server, nothing indexed
        let conn = atlas::db::open_at(&main).unwrap();
        let first_day = atlas::chunks::unix_day(1_767_225_600);
        atlas::chunks::add(&conn, GROUP, first_day + 29, first_day).unwrap();
        let now = chrono::Utc::now().timestamp();
        while let Some(claim) =
            atlas::chunks::claim(&conn, &[(GROUP.to_string(), i64::MIN)], &format!("127.0.0.1:{ps}"), now).unwrap()
        {
            assert!(atlas::chunks::finish(&conn, &claim, now).unwrap());
        }
        // another split group with a day still running elsewhere
        atlas::chunks::add(&conn, "alt.binaries.other", first_day + 29, first_day + 29).unwrap();
        let elsewhere = [("alt.binaries.other".to_string(), i64::MIN)];
        assert!(atlas::chunks::claim(&conn, &elsewhere, "elsewhere", now).unwrap().is_some());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        let (_, articles) = atlas::store::totals(&conn).unwrap();
        let (done, total) = atlas::chunks::progress(&conn, GROUP).unwrap_or((0, 0));
        if total > 0 && done == total && articles == 1200 {
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
