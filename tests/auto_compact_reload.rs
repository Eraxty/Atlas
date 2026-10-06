//! auto_run_compact applies while indexing, like the other settings: turned
//! on in config.json the indexer starts compacting on schedule, turned off it
//! stops.
//!
//! Unix only: it tells a compaction by the shard files' inodes.
//!
//! One test, and the environment is set before any thread starts.

#![cfg(unix)]

mod common;

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post, spawn_server};

/// longer than the indexer takes to re-read config.json (every 5s)
const RELOAD: Duration = Duration::from_secs(7);

#[test]
fn turning_auto_compaction_on_and_off_applies_while_indexing() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
        std::env::set_var("ATLAS_COMPACT_EVERY_SECS", "1");
    }
    let posts: Vec<common::Post> = (1..=100)
        .map(|n| post(n, &format!(r#""r{}.part{}.rar" yEnc (1/1)"#, n / 20, n % 20 + 1), 10, vec![]))
        .collect();
    let port = spawn_server(Server::new(posts));
    let config = |auto: bool| {
        let config = serde_json::json!({
            "usenet_servers": [{"host": "127.0.0.1", "username": "bob", "password": "secret", "port": port, "ssl": false, "connections": 2, "priority": 1}],
            "groups": [GROUP], "index_mode": "backfill", "request_size": 50, "auto_run_compact": auto
        });
        std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    };
    config(false);
    atlas::db::create_db().unwrap();
    let main = home.path().join("atlas.db");
    let shards = atlas::store::shard_paths(&main);
    let inodes = || shards.iter().map(|p| std::fs::metadata(p).unwrap().ino()).collect::<Vec<_>>();
    let compacted = || atlas::store::get_meta(&atlas::db::open_at(&main).unwrap(), "last_compact").unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    // off: the interval passes and nothing is compacted
    let before = inodes();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(compacted(), None, "compacted with auto_run_compact off");
    assert_eq!(inodes(), before);

    // turned on: it compacts
    config(true);
    let deadline = Instant::now() + RELOAD + Duration::from_secs(10);
    while compacted().is_none() {
        assert!(Instant::now() < deadline, "turned on, but never compacted");
        std::thread::sleep(Duration::from_millis(100));
    }

    // turned off again: once that's read, it stops
    config(false);
    std::thread::sleep(RELOAD);
    let settled = inodes();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(inodes(), settled, "turned off, but still compacting");

    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);
}
