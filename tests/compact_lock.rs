//! The indexer and a compaction never write the shards at once: the indexer
//! doesnt start while a compaction holds the lock, and a compaction cant
//! start while the indexer runs (its writes would land in an original after
//! the copy was read, and be lost in the swap).
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GROUP, Server, post, spawn_server};

#[test]
fn the_indexer_and_a_compaction_lock_each_other_out() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }
    let posts: Vec<common::Post> = (1..=200)
        .map(|n| post(n, &format!(r#""r{}.part{}.rar" yEnc (1/1)"#, n / 20, n % 20 + 1), 10, vec![]))
        .collect();
    let port = spawn_server(Server::new(posts));
    let config = serde_json::json!({
        "usenet_servers": [{"host": "127.0.0.1", "username": "bob", "password": "secret", "port": port, "ssl": false, "connections": 2, "priority": 1}],
        "groups": [GROUP], "index_mode": "backfill", "request_size": 50
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();
    let main = home.path().join("atlas.db");
    let articles = || atlas::store::totals(&atlas::db::open_with_shards(&main).unwrap()).unwrap().1;
    let cfg = atlas::config::load_config().unwrap();
    let index = |stop: &Arc<AtomicBool>| {
        let (cfg, stop) = (cfg.clone(), stop.clone());
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    // a compaction holds the lock: the indexer refuses to start and writes nothing
    let compacting = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(atlas::compact::lock_path(&main))
        .unwrap();
    compacting.try_lock().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let runner = index(&stop);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runner.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let refused = runner.is_finished();
    stop.store(true, Ordering::Relaxed);
    let code = runner.join().unwrap();
    assert!(refused, "the indexer started while a compaction held the lock");
    assert_ne!(code, 0, "refusing is an error");
    assert_eq!(articles(), 0, "nothing was written");
    compacting.unlock().unwrap();
    drop(compacting);

    // a backup next to a shard that is in place, roles unknown (an older atlas
    // made an empty shard beside it): the indexer refuses, and writes nothing
    let backup = home.path().join("atlas.s0.precompact.db");
    std::fs::copy(atlas::store::shard_path(&main, 0), &backup).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let runner = index(&stop);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runner.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let refused = runner.is_finished();
    stop.store(true, Ordering::Relaxed);
    let code = runner.join().unwrap();
    assert!(refused, "the indexer started with a backup next to a shard");
    assert_ne!(code, 0, "refusing is an error");
    assert_eq!(articles(), 0, "nothing was written");
    std::fs::remove_file(&backup).unwrap();

    // the indexer runs: a compaction is refused, and touches nothing
    let stop = Arc::new(AtomicBool::new(false));
    let runner = index(&stop);
    let deadline = Instant::now() + Duration::from_secs(60);
    while articles() == 0 {
        assert!(Instant::now() < deadline, "never indexed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let never = Arc::new(AtomicBool::new(false));
    let err = atlas::compact::run(&main, &|_| {}, &never).unwrap_err();
    assert!(err.downcast_ref::<atlas::compact::Busy>().is_some(), "refused while indexing: {err:#}");
    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);

    // and once it stopped, a compaction runs
    atlas::compact::run(&main, &|_| {}, &never).unwrap();
}
