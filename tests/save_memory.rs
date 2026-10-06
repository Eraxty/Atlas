//! Nothing on the fetch, parse and save path keeps anything per header: the
//! heap after indexing more and more headers stays where it was. Counts the
//! live bytes of every allocation in this test binary.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use atlas::indexer::{PassContext, PassSettings, RunStates, run_pass, shared_db};
use atlas::nntp::Pool;
use common::{Post, Server, mock, spawn_server};

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE.fetch_add(n as isize - l.size() as isize, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const POSTS: u64 = 40_000;
const ROUNDS: usize = 6;

/// releases of 50 parts with obfuscated names, message-ids on 40 domains
fn posts() -> Vec<Post> {
    (1..=POSTS)
        .map(|n| {
            let (rel, part) = (n / 50, n % 50 + 1);
            Post {
                number: n,
                subject: format!(r#"[{rel}/999] - "{:016x}.part{part:02}.rar" yEnc ({part}/50)"#, rel * 7919),
                message_id: format!("<{:024x}@d{}.example>", n.wrapping_mul(0x9e37_79b9_7f4a_7c15), n % 40),
                bytes: 700_000,
                body: vec![],
                date: "Fri, 02 Oct 2026 10:11:12 +0000".into(),
            }
        })
        .collect()
}

#[test]
fn the_heap_doesnt_grow_with_headers_saved() {
    let mut s = Server::new(posts());
    Arc::get_mut(&mut s).unwrap().any_group = true;
    let port = spawn_server(s);
    let pool = Arc::new(Pool::new(&[mock(port, "secret", 8, 1)]));

    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let db = shared_db(atlas::db::open_at(&main).unwrap());
    let ctx = PassContext {
        pool: pool.clone(),
        states: RunStates::default(),
        stop: Arc::new(AtomicBool::new(false)),
        verbose: false,
    };
    let settings = PassSettings {
        mode: "backfill".into(),
        batch_size: 20_000,
        request_size: 2_000,
        split_min_backlog: i64::MAX,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(pool.connect()).unwrap();

    // a group per round, all on one shard: one writer saves them all
    let groups: Vec<String> = (0..)
        .map(|g| format!("alt.binaries.heap{g}"))
        .filter(|g| atlas::store::shard_of(g) == 0)
        .take(ROUNDS)
        .collect();
    let mut live = Vec::new();
    for group in &groups {
        rt.block_on(async {
            for _ in 0..20 {
                run_pass(&ctx, &settings, &db, group, 0, &mut |_| {}).await.unwrap();
                if ctx.states.is_idle(group) {
                    return;
                }
            }
            panic!("{group} never went idle");
        });
        live.push(LIVE.load(Ordering::Relaxed));
    }

    let conn = atlas::db::open_with_shards(&main).unwrap();
    let (_, articles) = atlas::store::totals(&conn).unwrap();
    assert_eq!(articles, ROUNDS as i64 * POSTS as i64);

    // after the first rounds warm up (caches, connections, statements), more
    // headers saved leave the heap as it was: 400 bytes kept per header would
    // be 64MB over these rounds
    let grew = live[ROUNDS - 1] - live[1];
    let headers = (ROUNDS - 2) as isize * POSTS as isize;
    assert!(grew < 4 << 20, "the heap grew {} bytes over {headers} headers: {live:?}", grew);
}
