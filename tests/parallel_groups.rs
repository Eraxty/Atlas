//! The background indexer running many groups at once on every usenet server:
//! all indexing servers take part whatever their priority, each server stays
//! within its connections, a server with `index: false` is left alone, groups
//! stick to one server, and stopping is clean.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Server, post, spawn_server};

const GROUPS: usize = 12;

fn bench_server(posts: &[common::Post]) -> Arc<Server> {
    let mut s = Server::new(posts.to_vec());
    let s_mut = Arc::get_mut(&mut s).unwrap();
    s_mut.any_group = true;
    // slow enough that passes overlap
    s_mut.xover_delay = Duration::from_millis(150);
    s
}

#[test]
fn every_server_indexes_groups_in_parallel() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    let posts: Vec<common::Post> = (1..=300)
        .map(|n| post(n, &format!(r#""rel{}.part{}.rar" yEnc (1/1)"#, n / 30, n % 30 + 1), 10, vec![]))
        .collect();

    let servers: Vec<Arc<Server>> = (0..4).map(|_| bench_server(&posts)).collect();
    let ports: Vec<u16> = servers.iter().map(|s| spawn_server(s.clone())).collect();

    let groups: Vec<String> = (1..=GROUPS).map(|i| format!("alt.binaries.p{i}")).collect();
    let config = serde_json::json!({
        "usenet_servers": [
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": ports[0], "ssl": false, "connections": 4, "priority": 1},
            {"host": "localhost", "username": "bob", "password": "secret", "port": ports[1], "ssl": false, "connections": 4, "priority": 2},
            {"host": "127.0.0.1", "username": "bob", "password": "secret", "port": ports[2], "ssl": false, "connections": 4, "priority": 4},
            // a block account kept out of indexing
            {"host": "localhost", "username": "bob", "password": "secret", "port": ports[3], "ssl": false, "connections": 4, "priority": 9, "index": false}
        ],
        "groups": groups,
        "index_mode": "backfill",
        "parallel_groups": 6,
        "request_size": 1000
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();

    atlas::db::create_db().unwrap();
    let cfg = atlas::config::load_config().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let runner = {
        let stop = stop.clone();
        std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop))
    };

    // wait for every group to land
    let db = atlas::db::open().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let indexed: i64 =
            db.query_row("select count(distinct group_name) from releases", [], |r| r.get(0)).unwrap_or(0);
        if indexed == GROUPS as i64 {
            break;
        }
        assert!(Instant::now() < deadline, "only {indexed}/{GROUPS} groups indexed");
        std::thread::sleep(Duration::from_millis(100));
    }

    stop.store(true, Ordering::Relaxed);
    assert_eq!(runner.join().unwrap(), 0);

    // every group fully indexed: 11 releases each
    let per_group: Vec<(String, i64)> = {
        let mut stmt = db.prepare("select group_name, count(*) from releases group by group_name").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(per_group.len(), GROUPS);
    assert!(per_group.iter().all(|(_, n)| *n == 11), "{per_group:?}");

    // all three indexing servers took part whatever their priority, each group on one server
    let seen: Vec<BTreeSet<String>> = servers.iter().map(|s| s.groups_seen.lock().unwrap().clone()).collect();
    for (i, s) in seen.iter().take(3).enumerate() {
        assert!(!s.is_empty(), "server {i} never indexed: {seen:?}");
    }
    assert_eq!(seen[0].len() + seen[1].len() + seen[2].len(), GROUPS, "a group moved between servers: {seen:?}");
    assert_eq!(servers[3].xovers.load(Ordering::SeqCst), 0, "index: false keeps a server out of indexing");

    // one cursor row per group
    let keys: i64 = db.query_row("select count(*) from groups", [], |r| r.get(0)).unwrap();
    assert_eq!(keys, GROUPS as i64);

    // every row knows its article range and the dated ends of what was indexed
    // (the stats dashboard's progress and history numbers)
    for row in atlas::db::group_progress(&db).unwrap() {
        assert_eq!((row.first, row.last), (Some(1), Some(300)), "{row:?}");
        let (low, high) = (row.low.expect("low end"), row.high.expect("high end"));
        assert!(low.0 <= high.0 && (1..=300).contains(&low.0) && (1..=300).contains(&high.0), "{row:?}");
        assert!(low.1 > 0 && low.1 == high.1, "the mock posts everything at one moment: {row:?}");
    }

    // several groups at once per server, never more connections than allowed
    let overlap = servers.iter().take(3).map(|s| s.xover_peak.load(Ordering::SeqCst)).max().unwrap();
    assert!(overlap >= 2, "groups never overlapped on a server (peak {overlap})");
    for (i, s) in servers.iter().enumerate() {
        assert!(s.peak.load(Ordering::SeqCst) <= 4, "server {i} went over its connections");
    }

    let status = std::fs::read_to_string(home.path().join("status.json")).unwrap();
    assert!(status.contains(r#""status":"stopped""#), "{status}");
}
