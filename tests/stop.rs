//! Stopping while a request is stuck: a provider that stops answering mid
//! XOVER must not hold up stopping (SIGTERM / "Stop Indexing") for the read
//! timeout, the stuck connection must never be handed out again, and the
//! unfinished pass must not move its cursor.
//!
//! One test, and the environment is set before any thread starts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use atlas::nntp::{BlockingPool, Pool};
use common::{GROUP, Server, mock, post, spawn_server};

/// way longer than stopping is allowed to take
const STALL: Duration = Duration::from_secs(20);
const PROMPT: Duration = Duration::from_secs(3);

fn stalling_server(stalls: usize) -> Arc<Server> {
    let posts =
        (1..=200).map(|n| post(n, &format!(r#""rel{}.part{}.rar" yEnc (1/1)"#, n / 20, n % 20 + 1), 10, vec![]));
    let mut s = Server::new(posts.collect());
    let s_mut = Arc::get_mut(&mut s).unwrap();
    s_mut.compress = true;
    s_mut.stall = STALL;
    s_mut.stalls_left.store(stalls, Ordering::SeqCst);
    s
}

/// set `stop` once a request is stuck on the server, returns when it was set
fn stop_when_stalled(server: Arc<Server>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Instant> {
    std::thread::spawn(move || {
        while server.stalled.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        // let the other slices finish first
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
        Instant::now()
    })
}

#[test]
fn stop_does_not_wait_for_a_stuck_request() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary, set before any thread is spawned
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
        std::env::set_var("ATLAS_SAB_DIR", home.path().join("no-sabnzbd"));
    }

    // the pool on its own: stop returns right away, and the stuck connection
    // is thrown away instead of going back to the idle list
    let state = stalling_server(1);
    let port = spawn_server(state.clone());
    let pool = BlockingPool::from_pool(Pool::new(&[mock(port, "secret", 2, 1)]).with_timeout(Duration::from_secs(5)));
    pool.connect().unwrap();

    let slices: Vec<(u64, u64)> = (0..10).map(|i| (i * 20 + 1, i * 20 + 20)).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let stopper = stop_when_stalled(state.clone(), stop.clone());
    let got = pool.block_on(async {
        let mut rx = pool.pool.stream_headers(GROUP, 0, slices.clone(), stop.clone());
        let mut got = 0;
        while let Some(slice) = rx.recv().await {
            got += slice.result.is_ok() as usize;
        }
        got
    });
    let waited = stopper.join().unwrap().elapsed();
    assert!(waited < PROMPT, "stream took {waited:?} to stop with a request stuck");
    assert!(got < slices.len(), "the stuck slice cant have come back");

    // everything again on the same pool: a reused stuck connection would read
    // nothing until the stalled reply shows up (and fail on the 5s timeout)
    let started = Instant::now();
    let rows = pool.fetch_headers(GROUP, 1, 200).unwrap();
    assert_eq!(rows.len(), 200);
    assert!(started.elapsed() < PROMPT, "took {:?}, the stuck connection was reused", started.elapsed());

    // the whole background indexer: stopping with a request stuck
    let state = stalling_server(1);
    let port = spawn_server(state.clone());
    let config = serde_json::json!({
        "usenet_servers": [{"host": "127.0.0.1", "username": "bob", "password": "secret", "port": port, "ssl": false, "connections": 2, "priority": 1}],
        "group": GROUP,
        "groups": [GROUP],
        "index_mode": "backfill",
        "batch_size": 200,
        "request_size": 20
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    atlas::db::create_db().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stopper = stop_when_stalled(state.clone(), stop.clone());
    let cfg = atlas::config::load_config().unwrap();
    let runner = std::thread::spawn(move || atlas::bg_indexer::run_until(cfg, stop));

    let stopped_at = stopper.join().unwrap();
    assert_eq!(runner.join().unwrap(), 0);
    let waited = stopped_at.elapsed();
    assert!(waited < PROMPT, "indexer took {waited:?} to stop with a request stuck");

    // the pass never finished: what was saved stays, the cursor didnt move
    let db = atlas::db::open().unwrap();
    let cursor: i64 = db.query_row("select backfill_cursor from groups where name = ?", [GROUP], |r| r.get(0)).unwrap();
    assert_eq!(cursor, 200, "an unfinished pass moved the backfill cursor");
    let saved: i64 = db.query_row("select count(*) from releases", [], |r| r.get(0)).unwrap();
    assert!(saved > 0, "the slices that came back were saved");

    let status = std::fs::read_to_string(home.path().join("status.json")).unwrap();
    assert!(status.contains(r#""status":"stopped""#), "{status}");
}
