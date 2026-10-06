//! Where indexing time goes, for tuning. On with `ATLAS_PROFILE=1`; the
//! background indexer then logs a breakdown every 30 seconds.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Counter(AtomicU64::new(0))
    }

    /// add the time since `start`
    pub fn add_since(&self, start: Instant) {
        if enabled() {
            self.0.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    pub fn add(&self, n: u64) {
        if enabled() {
            self.0.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

pub static PARSE: Counter = Counter::new();
pub static NAMES: Counter = Counter::new();
pub static DB_WAIT: Counter = Counter::new();
pub static UPSERT: Counter = Counter::new();
pub static ARTICLES: Counter = Counter::new();
pub static STATS: Counter = Counter::new();
pub static COMMIT: Counter = Counter::new();
pub static SLICES: Counter = Counter::new();
pub static HEADERS: Counter = Counter::new();

pub fn enabled() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("ATLAS_PROFILE").is_some_and(|v| !v.is_empty()));
    *ON
}

/// One line summary since the last call, then reset.
pub fn report(window_secs: f64) -> String {
    let ms = |c: &Counter| c.take() as f64 / 1e6;
    let (parse, names, wait, upsert, articles, stats, commit) =
        (ms(&PARSE), ms(&NAMES), ms(&DB_WAIT), ms(&UPSERT), ms(&ARTICLES), ms(&STATS), ms(&COMMIT));
    let (slices, headers) = (SLICES.take(), HEADERS.take());
    let db = upsert + articles + stats + commit;

    format!(
        "[profile {window_secs:.0}s] {headers} headers in {slices} slices ({:.0}/s) | parse {parse:.0}ms  names {names:.0}ms | \
         db busy {db:.0}ms ({:.0}% of the window): upsert {upsert:.0}  articles {articles:.0}  stats {stats:.0}  commit {commit:.0} | \
         waiting for the db {wait:.0}ms",
        headers as f64 / window_secs,
        db / (window_secs * 10.0),
    )
}

/// Always-on load counters for the stats dashboard's bottleneck page. They
/// only ever add up; the dashboard works out rates from two readings.
pub struct Load {
    /// time the db writer spent saving (one writer, soo at most 1s per second)
    pub writer_busy_ns: AtomicU64,
    /// transactions and the slices saved in them
    pub writer_batches: AtomicU64,
    pub writer_slices: AtomicU64,
    /// slices waiting for the writer right now
    pub writer_queued: AtomicU64,
    /// part of writer_busy spent finishing checkpoints between transactions
    pub writer_checkpoint_ns: AtomicU64,
    /// part of writer_busy spent sealing files between transactions
    pub writer_seal_ns: AtomicU64,
    /// files (or whole seal ticks) that failed to seal
    pub writer_seal_errors: AtomicU64,
    /// time header requests spent on the wire, and how many there were
    pub xover_ns: AtomicU64,
    pub xovers: AtomicU64,
    /// time spent waiting for a free connection to a server
    pub lease_wait_ns: AtomicU64,
    /// time spent parsing headers into releases
    pub parse_ns: AtomicU64,
}

pub static LOAD: Load = Load {
    writer_busy_ns: AtomicU64::new(0),
    writer_batches: AtomicU64::new(0),
    writer_slices: AtomicU64::new(0),
    writer_queued: AtomicU64::new(0),
    writer_checkpoint_ns: AtomicU64::new(0),
    writer_seal_ns: AtomicU64::new(0),
    writer_seal_errors: AtomicU64::new(0),
    xover_ns: AtomicU64::new(0),
    xovers: AtomicU64::new(0),
    lease_wait_ns: AtomicU64::new(0),
    parse_ns: AtomicU64::new(0),
};

impl Load {
    pub fn add_since(counter: &AtomicU64, start: Instant) {
        counter.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// For stats.json: every counter, and when it was read.
    pub fn snapshot(&self) -> serde_json::Value {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        serde_json::json!({
            "at": at,
            // one writer per shard, their times add up
            "writers": crate::store::SHARDS,
            "writer_busy_ns": get(&self.writer_busy_ns),
            "writer_batches": get(&self.writer_batches),
            "writer_slices": get(&self.writer_slices),
            "writer_queued": get(&self.writer_queued),
            "writer_checkpoint_ns": get(&self.writer_checkpoint_ns),
            "writer_seal_ns": get(&self.writer_seal_ns),
            "writer_seal_errors": get(&self.writer_seal_errors),
            "xover_ns": get(&self.xover_ns),
            "xovers": get(&self.xovers),
            "lease_wait_ns": get(&self.lease_wait_ns),
            "parse_ns": get(&self.parse_ns),
        })
    }
}
