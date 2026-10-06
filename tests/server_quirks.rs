//! Provider behaviour seen in the wild: fewer connections allowed than
//! configured, article only servers that reject GROUP, and empty ranges
//! answered with 420.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use atlas::indexer::Indexer;
use atlas::nntp::BlockingPool;
use common::{GROUP, Server, index_until_idle, mock, post, spawn_server};

fn posts(n: u64) -> Vec<common::Post> {
    (1..=n).map(|i| post(i, &format!(r#""rel{}.part{}.rar" yEnc (1/1)"#, i / 20, i % 20 + 1), 10, vec![])).collect()
}

/// Configured for 8, the provider only takes 3: atlas lowers itself to what the
/// provider allows and every slice still comes back, nothing fails.
#[test]
fn lowers_connections_to_what_the_provider_allows() {
    let mut state = Server::new(posts(400));
    {
        let s = Arc::get_mut(&mut state).unwrap();
        s.max_conns = 3;
        s.xover_delay = Duration::from_millis(50);
    }
    let port = spawn_server(state.clone());

    let pool = BlockingPool::new(&[mock(port, "secret", 8, 1)]);
    pool.connect().unwrap();

    let slices: Vec<(u64, u64)> = (0..20).map(|i| (i * 20 + 1, i * 20 + 20)).collect();
    let (ok, failed) = pool.block_on(async {
        let mut rx = pool.pool.stream_headers(GROUP, 0, slices, Arc::new(AtomicBool::new(false)));
        let (mut ok, mut failed) = (0, 0);
        while let Some(slice) = rx.recv().await {
            if slice.result.is_ok() { ok += 1 } else { failed += 1 }
        }
        (ok, failed)
    });

    assert_eq!((ok, failed), (20, 0), "every slice should succeed");
    assert!(state.refused.load(Ordering::SeqCst) >= 1, "the mock should have refused some logins");
    assert!(pool.pool.connections(0) <= 3, "lowered to the provider's limit, got {}", pool.pool.connections(0));
    assert!(pool.pool.connections(0) >= 1);
}

/// A server that answers GROUP with 501 (fill / bonus servers) is switched to
/// article lookups only and its groups go to a server that can index.
#[test]
fn article_only_servers_stop_indexing() {
    let mut fill = Server::new(posts(50));
    Arc::get_mut(&mut fill).unwrap().no_group = true;
    let normal = Server::new(posts(50));

    let (pf, pn) = (spawn_server(fill.clone()), spawn_server(normal.clone()));
    let pool = BlockingPool::new(&[mock(pf, "secret", 4, 1), mock(pn, "secret", 4, 1)]);

    assert_eq!(pool.pool.indexing_servers(), vec![0, 1]);

    let (server, info) = pool.block_on(pool.pool.select_group_on(0, GROUP)).unwrap();
    assert_eq!(server, 1, "the group should move to the server that has GROUP");
    assert_eq!(info.2, 50);
    // one refusal can be a group name a real server doesnt like
    assert_eq!(pool.pool.indexing_servers(), vec![0, 1], "one refusal isnt enough");

    // refused again and again: an article only server
    for _ in 0..2 {
        pool.block_on(pool.pool.select_group_on(0, GROUP)).unwrap();
    }
    assert_eq!(pool.pool.indexing_servers(), vec![1], "the fill server is out of indexing");
    assert_eq!(pool.pool.pick_server("alt.binaries.anything"), 1);
}

/// A real server that rejects one odd group name keeps indexing the others.
#[test]
fn one_rejected_group_doesnt_take_a_server_out() {
    let normal = Server::new(posts(50));
    let port = spawn_server(normal.clone());
    let pool = BlockingPool::new(&[mock(port, "secret", 4, 1)]);

    // the mock answers 411 for unknown groups; a 501 for one name, then GROUP works
    for _ in 0..2 {
        let _ = pool.block_on(pool.pool.select_group_on(0, "alt.binaries.missing"));
    }
    assert!(pool.block_on(pool.pool.select_group_on(0, GROUP)).is_ok());
    assert_eq!(pool.pool.indexing_servers(), vec![0]);
}

/// A provider answering an empty range with `420 No Articles Selected`
/// instead of 423: the range is empty, the pass goes on and finishes.
#[test]
fn an_empty_range_answered_420_is_just_empty() {
    // articles 1-100 and 301-400, nothing in between
    let posts = posts(400).into_iter().filter(|p| !(101..=300).contains(&p.number)).collect();
    let mut state = Server::new(posts);
    Arc::get_mut(&mut state).unwrap().empty_is_420 = true;
    let port = spawn_server(state.clone());

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("atlas.db");
    atlas::db::create_db_at(&db_path).unwrap();
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();
    let mut indexer = Indexer::new(pool, "backfill", atlas::db::open_at(&db_path).unwrap());
    indexer.request_size = 50;
    index_until_idle(&mut indexer);

    let conn = atlas::db::open_with_shards(&db_path).unwrap();
    let (_, articles) = atlas::store::totals(&conn).unwrap();
    assert_eq!(articles, 200);
    let cursor: i64 =
        conn.query_row("select backfill_cursor from groups where name = ?", [GROUP], |r| r.get(0)).unwrap();
    assert_eq!(cursor, 0, "the backfill went all the way down");
}
