//! With auto_run_compact on, the indexer stops for a compaction once the
//! interval has passed, records it, and goes back to indexing.
//!
//! Unix only: it tells the rewrite by the shard files' inodes.
//!
//! One test, and the environment is set before any thread starts.

#![cfg(unix)]

mod common;

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post, spawn_server};

#[test]
fn compacts_on_schedule_and_keeps_indexing() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
        std::env::set_var("ATLAS_COMPACT_EVERY_SECS", "2");
    }
    let posts: Vec<common::Post> = (1..=200)
        .map(|n| post(n, &format!(r#""r{}.part{}.rar" yEnc (1/1)"#, n / 20, n % 20 + 1), 10, vec![]))
        .collect();
    let port = spawn_server(Server::new(posts));
    let config = serde_json::json!({
        "usenet_servers": [{"host": "127.0.0.1", "username": "bob", "password": "secret", "port": port, "ssl": false, "connections": 2, "priority": 1}],
        "groups": [GROUP], "index_mode": "backfill", "request_size": 50, "auto_run_compact": true
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();

    let main = home.path().join("atlas.db");
    let shards = atlas::store::shard_paths(&main);
    assert!(shards.iter().all(|p| p.exists()), "create_db made the shards");
    let inodes = || shards.iter().map(|p| std::fs::metadata(p).unwrap().ino()).collect::<Vec<_>>();
    let before = inodes();

    let stop = Arc::new(AtomicBool::new(false));
    let cfg = atlas::config::load_config().unwrap();
    assert!(cfg.auto_run_compact);
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    let compacted = loop {
        let conn = atlas::db::open_at(&main).unwrap();
        if let Some(t) = atlas::store::get_meta(&conn, "last_compact").unwrap() {
            break t;
        }
        assert!(Instant::now() < deadline, "never compacted");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(compacted > 0);
    // last_compact is noted on failure too: the compaction really ran when
    // every shard is a new file (compacting writes a copy and swaps it in),
    // and none of the copying is left lying around
    assert!(inodes().iter().zip(&before).all(|(now, was)| now != was), "every shard was rewritten");
    assert!(atlas::compact::hold_off_compaction(&main).is_ok(), "the compaction lock was let go");
    for shard in &shards {
        for suffix in ["compact", "precompact"] {
            let stem = shard.file_stem().unwrap().to_string_lossy().into_owned();
            assert!(!shard.with_file_name(format!("{stem}.{suffix}.db")).exists());
        }
    }

    // indexing goes on after it: everything gets indexed
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let conn = atlas::db::open_with_shards(&main).unwrap();
        if atlas::store::totals(&conn).unwrap().1 >= 200 {
            break;
        }
        assert!(Instant::now() < deadline, "indexing didnt resume");
        std::thread::sleep(Duration::from_millis(200));
    }
    // and holds the lock again (shared: not a compaction of its own, which
    // holds it exclusively)
    let indexer_holds_it = || {
        let lock = std::fs::File::open(atlas::compact::lock_path(&main)).unwrap();
        lock.try_lock().is_err() && lock.try_lock_shared().is_ok()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !indexer_holds_it() {
        assert!(Instant::now() < deadline, "the indexer didnt take the lock back after compacting");
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);
}
