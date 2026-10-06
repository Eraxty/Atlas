//! The date search holds a bounded number of headers whatever the span it
//! reads: a crowd of articles in numbers the spread out windows stepped over
//! isnt read into memory whole. Counts the live bytes of every allocation in
//! this test binary (the mock server's included, it streams its replies).

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use atlas::nntp::BlockingPool;
use common::{GROUP, Server, mock, post_at, spawn_server};

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

fn grew(n: isize) {
    let now = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        grew(l.size() as isize);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        grew(n as isize - l.size() as isize);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn jan(day: i64) -> i64 {
    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00").unwrap().timestamp() + (day - 1) * 86_400
}

/// 1..=50 a minute apart on Jan 1; a crowd of 140,001 posts a second apart
/// from Jan 2 at 150,000..=290,000; nothing till 598,000..=600,000 on Jan 10.
/// Searching from 100 on, the spread out windows step over the whole crowd
/// (they land on 140,100.. and 292,900.., both empty) before they hit the
/// last run.
fn crowd_between_windows() -> Vec<(u64, i64)> {
    let early = (1..=50u64).map(|n| (n, jan(1) + (n as i64 - 1) * 60));
    let crowd = (150_000..=290_000u64).map(|n| (n, jan(2) + (n - 150_000) as i64));
    let late = (598_000..=600_000u64).map(|n| (n, jan(10) + (n - 598_000) as i64 * 60));
    early.chain(crowd).chain(late).collect()
}

#[test]
fn the_date_search_holds_a_bounded_number_of_headers() {
    let list = crowd_between_windows();
    let posts = list
        .iter()
        .map(|&(n, t)| {
            // subjects as long as real ones, soo holding the crowd would show
            let subject = format!(r#"[01/99] - "{n:0>96}.part01.rar" yEnc (1/1)"#);
            post_at(n, &subject, &chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc2822())
        })
        .collect();
    let server = Server::new(posts);
    let port = spawn_server(server.clone());
    let pool = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    pool.connect().unwrap();

    // a search finds `want`, holding little on top of what was live before
    // it (the crowd's headers alone are about 100MB once parsed), in a
    // reasonable number of requests
    let check = |what: &str, want: Option<i64>, search: &dyn Fn() -> Option<i64>| {
        let xovers = server.xovers.load(Ordering::SeqCst);
        let before = LIVE.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        let found = search();
        let held = PEAK.load(Ordering::Relaxed) - before;
        let used = server.xovers.load(Ordering::SeqCst) - xovers;
        eprintln!("{what}: found {found:?}, held {held} bytes, {used} requests");
        assert_eq!(found, want, "{what}");
        assert!(held < 8 << 20, "{what}: held {held} bytes at once");
        assert!(used <= 150, "{what}: {used} requests");
    };
    let first = |low: u64, when: i64| list.iter().find(|&&(n, t)| n >= low && t >= when).map_or(600_001, |&(n, _)| n);
    let posted = |n: u64| list.iter().find(|&&(m, _)| m == n).unwrap().1;

    // how far back the server keeps the group, from a low mark of 100
    check("first post from 100", Some(jan(2)), &|| {
        pool.block_on(pool.pool.first_post(0, GROUP, 100, 600_000)).unwrap()
    });
    // the article at a time: inside the crowd, between the crowd and the last
    // run (the backward search crosses the crowd), before everything
    for (low, when) in [(1, posted(200_000)), (1, jan(5)), (100, jan(1)), (1, posted(290_000) + 1)] {
        check(&format!("article at {when} from {low}"), Some(first(low, when) as i64), &|| {
            Some(pool.block_on(pool.pool.article_at(0, GROUP, low, 600_000, when)).unwrap() as i64)
        });
    }
}
