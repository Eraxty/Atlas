//! Idle workers only take day chunks of groups the indexer still tracks:
//! chunks left behind by a group removed from config.json stay pending.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post_at, spawn_server};

/// a group split before it was taken out of config.json
const GONE: &str = "alt.binaries.gone";

#[test]
fn chunks_of_a_group_no_longer_in_the_config_are_left_alone() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    // serves any alt.binaries group, GONE included
    let start = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap();
    let posts = (1..=50u64)
        .map(|n| {
            let when = start + chrono::Duration::hours(n as i64);
            post_at(n, &format!(r#""p{n}.bin" yEnc (1/1)"#), &when.to_rfc2822())
        })
        .collect();
    let mut server = Server::new(posts);
    Arc::get_mut(&mut server).unwrap().any_group = true;
    let port = spawn_server(server.clone());

    // three workers for one group: two of them idle, looking for day chunks
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": port, "ssl": false, "connections": 8, "priority": 1}
        ],
        "groups": [GROUP],
        "index_mode": "backfill",
        "parallel_groups": 3,
        "batch_size": 100,
        "request_size": 50
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();
    let main = home.path().join("atlas.db");
    let day = atlas::chunks::unix_day(start.timestamp());
    atlas::chunks::add(&atlas::db::open_at(&main).unwrap(), GONE, day + 1, day).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    // GROUP gets indexed, idle workers keep looking for chunks meanwhile
    let deadline = Instant::now() + Duration::from_secs(60);
    while atlas::store::totals(&atlas::db::open_with_shards(&main).unwrap()).unwrap().1 < 50 {
        assert!(Instant::now() < deadline, "GROUP never got indexed");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);

    let conn = atlas::db::open_at(&main).unwrap();
    let touched: i64 =
        conn.query_row("select count(*) from backfill_chunks where state != 0", [], |r| r.get(0)).unwrap();
    assert_eq!(touched, 0, "no chunk of {GONE} was claimed");
    assert!(!server.groups_seen.lock().unwrap().contains(GONE));
}
