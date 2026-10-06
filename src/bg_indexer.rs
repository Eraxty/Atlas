//! The headless indexing loop, run as `atlas --bg-indexer`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;

use crate::atomic::write_atomic;
use crate::compact::WriteGuard;
use crate::config::{Config, UsenetServer, load_config};
use crate::db;
use crate::indexer::{Db, NotCarried, PassContext, PassSettings, Progress, RunStates, TooOld, run_pass, shared_db};
use crate::nntp::{Pool, unless_stopped};
use crate::paths;
use crate::sab;
use crate::ui;

const HISTORY_LEN: f64 = 60.0;
/// a group that failed 3 times in a row gets another go after this long
const FAILED_RETRY: Duration = Duration::from_secs(300);
/// an idle group (nothing new) is looked at again after this long
const IDLE_RECHECK: Duration = Duration::from_secs(10);
/// how often config.json is re-read while indexing
const CONFIG_RELOAD: Duration = Duration::from_secs(5);

/// how often auto_run_compact compacts the database
pub const COMPACT_EVERY: Duration = Duration::from_secs(24 * 3600);
/// a compaction that couldnt take the lock (a `--compact` or a menu write
/// holds it) is tried again after this long
const COMPACT_RETRY: Duration = Duration::from_secs(10 * 60);

/// servers with a host and password, in priority order
fn usable_servers(config: &crate::config::Config) -> Vec<UsenetServer> {
    config.servers.iter().filter(|s| !s.host.is_empty() && !s.password.is_empty()).cloned().collect()
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn write_status(running: bool, group: &str, mode: &str, idle: bool, status: &str, error: bool, errors: u32) {
    let data = json!({
        "running": running,
        "group": group,
        "error": error,
        "idle": idle,
        "mode": mode,
        "status": status,
        "error_count": errors,
        "pid": std::process::id(),
    });

    if let Err(e) = write_atomic(&paths::status_file(), data.to_string()) {
        // windows can refuse the rename while the menu reads it, write in place
        if let Err(e2) = fs::write(paths::status_file(), data.to_string()) {
            println!("couldnt write status: {e} / {e2}");
        }
    }
}

#[derive(Serialize, Clone)]
struct HistoryPoint {
    t: f64,
    a: i64,
    b: i64,
}

#[derive(Default)]
struct GroupStats {
    articles: i64,
    releases: i64,
    last_indexed: f64,
}

struct Stats {
    history: Vec<HistoryPoint>,
    total_articles: i64,
    total_bytes: i64,
    total_releases: i64,
    start_time: f64,
    groups_indexed: HashSet<String>,
    error_count: u32,
    group_stats: BTreeMap<String, GroupStats>,
    /// headers and post bytes per RATE_BIN seconds, oldest first, RATE_KEEP long
    rate: std::collections::VecDeque<(i64, i64, i64)>,
    /// extra fields for stats.json (servers, groups configured, workers)
    extra: serde_json::Map<String, serde_json::Value>,
}

/// headers/s is kept in buckets this many seconds wide...
pub const RATE_BIN: i64 = 10;
/// ...for this long
const RATE_KEEP: i64 = 6 * 3600;

impl Stats {
    fn new() -> Self {
        Stats {
            history: Vec::new(),
            total_articles: 0,
            total_bytes: 0,
            total_releases: 0,
            start_time: now(),
            groups_indexed: HashSet::new(),
            error_count: 0,
            group_stats: BTreeMap::new(),
            rate: std::collections::VecDeque::new(),
            extra: serde_json::Map::new(),
        }
    }

    fn tick(&mut self, articles: i64, bytes: i64, releases: i64, group: &str) {
        let t = now();

        let bin = t as i64 / RATE_BIN * RATE_BIN;
        match self.rate.back_mut() {
            Some(last) if last.0 == bin => {
                last.1 += articles;
                last.2 += bytes;
            }
            _ => self.rate.push_back((bin, articles, bytes)),
        }
        while self.rate.front().is_some_and(|f| f.0 < bin - RATE_KEEP) {
            self.rate.pop_front();
        }

        self.total_articles += articles;
        self.total_bytes += bytes;
        self.total_releases += releases;
        self.history.push(HistoryPoint { t, a: articles, b: bytes });

        if !group.is_empty() {
            self.groups_indexed.insert(group.to_string());
            let gs = self.group_stats.entry(group.to_string()).or_default();
            gs.articles += articles;
            gs.releases += releases;
            gs.last_indexed = t;
        }

        let cutoff = t - HISTORY_LEN;
        self.history.retain(|h| h.t >= cutoff);
    }

    /// (peak articles/s, peak bytes/s, avg articles/s, avg bytes/s)
    fn speeds(&self) -> (f64, f64, f64, f64) {
        let mut a_speeds = Vec::new();
        let mut b_speeds = Vec::new();

        for w in self.history.windows(2) {
            let dt = w[1].t - w[0].t;
            if dt > 0.0 {
                a_speeds.push(w[1].a as f64 / dt);
                b_speeds.push(w[1].b as f64 / dt);
            }
        }

        if a_speeds.is_empty() {
            return (0.0, 0.0, 0.0, 0.0);
        }

        let max = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max);
        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        (max(&a_speeds), max(&b_speeds), avg(&a_speeds), avg(&b_speeds))
    }

    fn write(&self, group: &str, mode: &str, running: bool, idle: bool) {
        let (peak_a, peak_b, avg_a, avg_b) = self.speeds();
        let db_size = crate::store::database_bytes(&paths::database());

        let groups: serde_json::Map<String, serde_json::Value> = self
            .group_stats
            .iter()
            .map(|(name, g)| {
                (name.clone(), json!({"articles": g.articles, "releases": g.releases, "last_indexed": g.last_indexed}))
            })
            .collect();

        let data = json!({
            "running": running,
            "idle": idle,
            "group": group,
            "mode": mode,
            "uptime": (now() - self.start_time) as i64,
            "total_articles": self.total_articles,
            "total_bytes": self.total_bytes,
            "total_releases": self.total_releases,
            "history": self.history,
            "groups_indexed": self.groups_indexed.len(),
            "error_count": self.error_count,
            "peak_art_speed": peak_a,
            "peak_byte_speed": peak_b,
            "avg_art_speed": avg_a,
            "avg_byte_speed": avg_b,
            "db_size": db_size,
            "groups": groups,
            "rate_bin": RATE_BIN,
            "rate": self.rate.iter().map(|(t, a, b)| [*t, *a, *b]).collect::<Vec<_>>(),
        });
        let mut data = data;
        if let Some(map) = data.as_object_mut() {
            map.extend(self.extra.clone());
        }

        let _ = write_atomic(&paths::stats_file(), data.to_string());
    }
}

fn backoff(errors: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(errors).min(30))
}

/// sleep up to `d`, waking early once `done` says so
async fn nap(d: Duration, done: impl Fn() -> bool) {
    let deadline = Instant::now() + d;
    while !done() && Instant::now() < deadline {
        tokio::time::sleep((deadline - Instant::now()).min(Duration::from_millis(250))).await;
    }
}

/// how often auto_run_compact compacts (ATLAS_COMPACT_EVERY_SECS for tests)
fn compact_every() -> Duration {
    std::env::var("ATLAS_COMPACT_EVERY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(COMPACT_EVERY)
}

fn settings_of(config: &Config) -> PassSettings {
    PassSettings {
        mode: config.index_mode.clone(),
        batch_size: config.batch_size() as i64,
        request_size: config.request_size(),
        split_min_backlog: config.split_min_backlog(),
        chunk_safety: crate::indexer::CHUNK_SAFETY,
    }
}

/// `alt.binaries.a, alt.binaries.b` or `3197 groups, 20 at once`
/// Groups indexed at once on each indexing server, as (server index, workers),
/// from config.json alone (pool indexes follow `usable_servers` order). Runtime
/// changes, like a server found to be article only or a lowered connection
/// limit, dont change the plan, soo they never trigger a rebuild.
fn plan_workers(config: &Config) -> Vec<(usize, usize)> {
    let servers = usable_servers(config);
    let mut indexing: Vec<usize> = (0..servers.len()).filter(|&i| servers[i].indexes()).collect();
    if indexing.is_empty() {
        indexing = (0..servers.len()).collect();
    }
    let conns: Vec<usize> = indexing.iter().map(|&i| servers[i].connections() as usize).collect();
    indexing.into_iter().zip(config.workers_per_server(&conns)).collect()
}

fn groups_label(groups: &[String], workers: usize) -> String {
    if groups.len() <= 3 { groups.join(", ") } else { format!("{} groups, {workers} at once", groups.len()) }
}

/// State shared by the scheduler's workers for one set of servers.
struct Scheduler {
    ctx: PassContext,
    settings: RwLock<PassSettings>,
    groups: RwLock<Vec<String>>,
    /// (server, groups at once) for every indexing server
    plan: Vec<(usize, usize)>,
    workers: usize,
    /// round robin position in `groups`, per server
    next: Mutex<HashMap<usize, usize>>,
    /// groups a worker is on right now
    busy: Mutex<HashSet<String>>,
    /// (group, server) pairs whose day chunks the server leaves alone: for
    /// good (None) when it doesnt carry the group, until then after chunk errors
    skip: Mutex<HashMap<(String, usize), Option<Instant>>>,
    /// consecutive day chunk errors per (group, server)
    chunk_errors: Mutex<HashMap<(String, usize), u32>>,
    /// servers one of whose workers sweeps a split group (see `take_sweep`)
    sweeping: Mutex<HashSet<usize>>,
    /// groups resting after going idle or after an error
    wait_until: Mutex<HashMap<String, Instant>>,
    /// consecutive errors per group
    errors: Mutex<HashMap<String, u32>>,
    /// groups parked after 3 errors in a row, retried after FAILED_RETRY
    failed: Mutex<HashMap<String, Instant>>,
    stats: Arc<Mutex<Stats>>,
    /// wind down (servers changed or stopping): finish current passes, start no new ones
    wind_down: AtomicBool,
    /// the compaction interval passed (set together with `wind_down`)
    compact_due: AtomicBool,
    /// no compaction before this (one couldnt take the lock)
    compact_after: Option<SystemTime>,
    /// auto_run_compact as config.json has it now
    auto_compact: AtomicBool,
    passes: AtomicUsize,
}

impl Scheduler {
    fn stopping(&self) -> bool {
        self.wind_down.load(Ordering::Relaxed) || self.ctx.stop.load(Ordering::Relaxed)
    }

    /// Next group due on `server`: one that maps to this server and isnt being
    /// indexed, resting or parked. Each server pulls its own groups soo every
    /// server's connections stay busy.
    fn take_group(&self, server: usize) -> Option<String> {
        let groups = self.groups.read().unwrap();
        if groups.is_empty() {
            return None;
        }

        let now = Instant::now();
        let pool = &self.ctx.pool;
        let tier = pool.indexing_tier();
        if !tier.contains(&server) {
            // this server is resting after a failure, its groups went elsewhere
            return None;
        }

        let mut busy = self.busy.lock().unwrap();
        let mut wait = self.wait_until.lock().unwrap();
        let mut failed = self.failed.lock().unwrap();
        let mut next = self.next.lock().unwrap();
        let next = next.entry(server).or_insert(0);

        failed.retain(|_, since| since.elapsed() < FAILED_RETRY);
        wait.retain(|_, until| *until > now);

        for step in 0..groups.len() {
            let idx = (*next + step) % groups.len();
            let g = &groups[idx];

            if busy.contains(g)
                || wait.contains_key(g)
                || failed.contains_key(g)
                || pool.pick_server_in(&tier, g) != server
            {
                continue;
            }

            *next = idx + 1;
            busy.insert(g.clone());
            return Some(g.clone());
        }

        None
    }

    fn finish(&self, group: &str, result: anyhow::Result<Progress>) {
        let states = &self.ctx.states;

        match result {
            Ok(_) => {
                self.errors.lock().unwrap().remove(group);
                // caught up: give it a rest instead of hammering GROUP every loop
                if states.is_idle(group) && !states.is_backfilling(group) {
                    self.wait_until.lock().unwrap().insert(group.to_string(), Instant::now() + IDLE_RECHECK);
                }
            }
            Err(_) if self.ctx.stop.load(Ordering::Relaxed) => {}
            Err(e) => {
                self.stats.lock().unwrap().error_count += 1;
                let count = {
                    let mut errors = self.errors.lock().unwrap();
                    let c = errors.entry(group.to_string()).or_insert(0);
                    *c += 1;
                    *c
                };

                if count >= 3 {
                    ui::error(&format!(
                        "Too many errors on {group}, parking it for {}m: {e}",
                        FAILED_RETRY.as_secs() / 60
                    ));
                    self.errors.lock().unwrap().remove(group);
                    self.failed.lock().unwrap().insert(group.to_string(), Instant::now());
                } else {
                    ui::error(&format!("Indexing error ({group}): {e}"));
                    self.wait_until.lock().unwrap().insert(group.to_string(), Instant::now() + backoff(count));
                }
            }
        }

        self.busy.lock().unwrap().remove(group);
        self.passes.fetch_add(1, Ordering::Relaxed);
    }

    /// After a day chunk of `group` on `server`. Errors count like a pass error
    /// and rest the pair; a server that doesnt carry the group skips its chunks for good.
    fn finish_chunk(&self, group: String, server: usize, result: anyhow::Result<Progress>) {
        let key = (group, server);
        match result {
            Ok(_) => {
                self.chunk_errors.lock().unwrap().remove(&key);
            }
            Err(_) if self.ctx.stop.load(Ordering::Relaxed) => {}
            // the day went back for another server, and this one wont take one that old again soon
            Err(e) if e.downcast_ref::<TooOld>().is_some() => {}
            Err(e) if e.downcast_ref::<NotCarried>().is_some() => {
                println!("{e}, leaving its day chunks to the other servers");
                self.skip.lock().unwrap().insert(key, None);
            }
            Err(e) => {
                self.stats.lock().unwrap().error_count += 1;
                let count = {
                    let mut errors = self.chunk_errors.lock().unwrap();
                    let c = errors.entry(key.clone()).or_insert(0);
                    *c += 1;
                    *c
                };
                ui::error(&format!("Indexing error ({}, day chunk on {}): {e}", key.0, self.ctx.pool.host(server)));
                self.skip.lock().unwrap().insert(key, Some(Instant::now() + backoff(count)));
            }
        }
    }

    /// groups not parked
    fn active_groups(&self) -> Vec<String> {
        let failed = self.failed.lock().unwrap();
        self.groups.read().unwrap().iter().filter(|g| !failed.contains_key(*g)).cloned().collect()
    }

    fn idle(&self) -> bool {
        let active = self.active_groups();
        let states = &self.ctx.states;
        states.all_idle(&active) && !active.iter().any(|g| states.is_backfilling(g))
    }

    fn write_status(&self) {
        let groups = self.groups.read().unwrap().clone();
        let mode = self.settings.read().unwrap().mode.clone();
        let idle = self.idle();
        let errors: u32 = self.errors.lock().unwrap().values().sum();
        let status = if errors > 0 {
            "warning"
        } else if idle {
            "idle"
        } else {
            "running"
        };

        write_status(true, &groups_label(&groups, self.workers), &mode, idle, status, false, errors);
        let mut stats = self.stats.lock().unwrap();
        stats.extra.insert("servers".into(), json!(self.ctx.pool.server_stats()));
        stats.extra.insert("groups_configured".into(), json!(groups.len()));
        stats.extra.insert("workers".into(), json!(self.workers));
        stats.extra.insert("pid".into(), json!(std::process::id()));
        let mut load = crate::profile::LOAD.snapshot();
        load["unsaved_headers"] = json!(self.ctx.pool.unsaved_headers());
        load["max_unsaved_headers"] = json!(self.ctx.pool.max_unsaved());
        stats.extra.insert("load".into(), load);
        stats.write("", &mode, true, idle);
    }
}

/// The groups whose day chunks a worker may take: ones still in config.json,
/// except `skip`, and none when the mode never backfills.
fn chunk_groups(mode: &str, groups: &[String], skip: &HashSet<String>) -> HashSet<String> {
    if mode == "live" {
        return HashSet::new();
    }
    groups.iter().filter(|g| !skip.contains(*g)).cloned().collect()
}

/// A day chunk of a split group for an idle worker on `server`: the newest
/// one pending, among the tracked groups this server hasnt said it lacks.
async fn take_chunk(sched: &Scheduler, server: usize, db: &Db) -> Option<crate::chunks::Claim> {
    // a server resting after a failure takes no chunks either
    if !sched.ctx.pool.indexing_tier().contains(&server) {
        return None;
    }

    let host = sched.ctx.pool.host(server);
    let skip: HashSet<String> = {
        let now = Instant::now();
        let mut skip = sched.skip.lock().unwrap();
        skip.retain(|_, until| until.is_none_or(|t| t > now));
        skip.keys().filter(|(_, s)| *s == server).map(|(g, _)| g.clone()).collect()
    };
    let mode = sched.settings.read().unwrap().mode.clone();
    let groups = chunk_groups(&mode, &sched.groups.read().unwrap(), &skip);
    if groups.is_empty() {
        return None;
    }

    let oldest = sched.ctx.states.kept_days(&host);
    match crate::indexer::claim_chunk(db, host, groups, oldest).await {
        Ok(chunk) => chunk,
        Err(e) => {
            ui::error(&format!("couldnt claim a day chunk: {e}"));
            None
        }
    }
}

/// A split group whose day chunks are done for an idle worker on `server`
/// to sweep (see `indexer::run_sweep`): one this server is not the home of
/// and hasnt swept, none while another of its workers sweeps.
async fn take_sweep(sched: &Scheduler, server: usize, db: &Db) -> Option<String> {
    let tier = sched.ctx.pool.indexing_tier();
    if !tier.contains(&server) || !sched.sweeping.lock().unwrap().insert(server) {
        return None;
    }

    let skip: HashSet<String> = {
        let now = Instant::now();
        let mut skip = sched.skip.lock().unwrap();
        skip.retain(|_, until| until.is_none_or(|t| t > now));
        skip.keys().filter(|(_, s)| *s == server).map(|(g, _)| g.clone()).collect()
    };
    let mode = sched.settings.read().unwrap().mode.clone();
    let groups: Vec<String> = chunk_groups(&mode, &sched.groups.read().unwrap(), &skip)
        .into_iter()
        .filter(|g| sched.ctx.pool.pick_server_in(&tier, g) != server)
        .collect();
    let found = match crate::indexer::sweep_to_take(db, sched.ctx.pool.host(server), groups).await {
        Ok(found) => found,
        Err(e) => {
            ui::error(&format!("couldnt look for a split group to sweep: {e}"));
            None
        }
    };
    if found.is_none() {
        sched.sweeping.lock().unwrap().remove(&server);
    }
    found
}

/// What a worker does next.
enum Work {
    Group(String),
    Chunk(crate::chunks::Claim),
    Sweep(String),
}

/// Next work for a worker on `server`. A due sweep of a split group whose
/// chunks are done (see `take_sweep`) comes first, before its normal groups,
/// soo home groups that keep every worker busy dont hold it off until what
/// it would find ages out. One worker per server sweeps at a time, the
/// others index their groups or take day chunks. `after_sweep`: the worker's
/// last pick was a sweep, soo a due group of its own goes before the next.
async fn next_work(sched: &Scheduler, server: usize, db: &Db, after_sweep: bool) -> Option<Work> {
    // right after a sweep a due normal group goes first, soo a worker that
    // is alone on its server alternates instead of sweeping forever
    if after_sweep && let Some(group) = sched.take_group(server) {
        return Some(Work::Group(group));
    }
    if let Some(group) = take_sweep(sched, server, db).await {
        return Some(Work::Sweep(group));
    }
    if let Some(group) = sched.take_group(server) {
        return Some(Work::Group(group));
    }
    take_chunk(sched, server, db).await.map(Work::Chunk)
}

/// One worker on `server`: index whichever of its groups is due next, again and again.
/// All workers share one db connection, soo their writes queue up in order
/// instead of racing for sqlite's write lock (and timing out on a busy db).
async fn worker(sched: Arc<Scheduler>, server: usize, db: Db) {
    let mut after_sweep = false;
    while !sched.stopping() {
        let work = next_work(&sched, server, &db, after_sweep).await;
        after_sweep = matches!(work, Some(Work::Sweep(_)));
        let group = match work {
            Some(Work::Group(group)) => group,
            // help with a split group's day chunks. not marked busy, a
            // group's chunks run on several servers at once
            Some(Work::Chunk(chunk)) => {
                let settings = sched.settings.read().unwrap().clone();
                let stats = sched.stats.clone();
                let group = chunk.group.clone();
                let mut progress = |p: &Progress| stats.lock().unwrap().tick(p.articles, p.bytes, p.releases, &group);
                let run = crate::indexer::run_chunk(&sched.ctx, &settings, &db, &chunk, server, &mut progress);
                let Some(result) = unless_stopped(&sched.ctx.stop, run).await else { break };
                sched.finish_chunk(chunk.group, server, result);
                continue;
            }
            Some(Work::Sweep(group)) => {
                let settings = sched.settings.read().unwrap().clone();
                let stats = sched.stats.clone();
                let mut progress = |p: &Progress| stats.lock().unwrap().tick(p.articles, p.bytes, p.releases, &group);
                let run = crate::indexer::run_sweep(&sched.ctx, &settings, &db, &group, server, &mut progress);
                let result = unless_stopped(&sched.ctx.stop, run).await;
                sched.sweeping.lock().unwrap().remove(&server);
                let Some(result) = result else { break };
                sched.finish_chunk(group, server, result);
                continue;
            }
            None => {
                nap(Duration::from_secs(1), || sched.stopping()).await;
                continue;
            }
        };

        let settings = sched.settings.read().unwrap().clone();
        let stats = sched.stats.clone();
        let mut progress = |p: &Progress| stats.lock().unwrap().tick(p.articles, p.bytes, p.releases, &group);

        // stopping drops a pass stuck on the network too (GROUP, a login, a name
        // lookup). safe: db writes run whole on their own thread and the cursor
        // only moves once a range is complete
        let pass = run_pass(&sched.ctx, &settings, &db, &group, server, &mut progress);
        let Some(result) = unless_stopped(&sched.ctx.stop, pass).await else { break };
        sched.finish(&group, result);
    }
}

/// Re-read config.json while indexing: new groups / mode / batch settings /
/// auto compaction apply right away. Returns when servers, the parallelism or
/// the unsaved headers cap changed (the caller rebuilds) or when stopping.
async fn watch_config(sched: Arc<Scheduler>, servers: Vec<UsenetServer>) {
    loop {
        nap(CONFIG_RELOAD, || sched.stopping()).await;
        if sched.stopping() {
            return;
        }

        let Some(config) = load_config() else { continue };

        if usable_servers(&config) != servers
            || plan_workers(&config) != sched.plan
            || config.max_unsaved_headers() != sched.ctx.pool.max_unsaved()
        {
            println!("config changed, restarting the indexer");
            sched.wind_down.store(true, Ordering::Relaxed);
            return;
        }

        let auto = config.auto_run_compact;
        if sched.auto_compact.swap(auto, Ordering::Relaxed) != auto {
            println!("auto compaction turned {}", if auto { "on" } else { "off" });
        }

        let settings = settings_of(&config);
        {
            let mut current = sched.settings.write().unwrap();
            if settings.mode != current.mode {
                // switching modes starts every group over in the backfill phase
                sched.ctx.states.reset();
                println!("indexer mode set to {}", settings.mode);
            }
            *current = settings;
        }

        let groups = config.tracked_groups();
        let mut current = sched.groups.write().unwrap();
        if *current != groups {
            println!("groups changed: {} -> {}", current.len(), groups.len());
            *current = groups;
        }
    }
}

/// While auto_run_compact is on (looked at every second, config.json can
/// change it): once the interval has passed since the last compaction (or
/// since the indexer started, when there was none), wind down so `supervise`
/// can compact.
async fn compact_timer(sched: Arc<Scheduler>) {
    let every = compact_every();
    let last = db::open_at(&paths::database()).ok().and_then(|conn| crate::store::get_meta(&conn, "last_compact").ok());
    // no stored time: count from when the indexer started (rebuilds dont reset it)
    let last = last.flatten().unwrap_or_else(|| sched.stats.lock().unwrap().start_time as i64);
    let due =
        (UNIX_EPOCH + Duration::from_secs(last.max(0) as u64) + every).max(sched.compact_after.unwrap_or(UNIX_EPOCH));
    while !sched.stopping() {
        if sched.auto_compact.load(Ordering::Relaxed) && SystemTime::now() >= due {
            println!("compacting the database, indexing resumes after");
            sched.compact_due.store(true, Ordering::Relaxed);
            sched.wind_down.store(true, Ordering::Relaxed);
            return;
        }
        nap(Duration::from_secs(1), || sched.stopping()).await;
    }
}

/// status.json / stats.json once a second for the menu and dashboard
/// (plus a timing breakdown every 30s with ATLAS_PROFILE)
async fn report(sched: Arc<Scheduler>) {
    let mut last_profile = Instant::now();
    while !sched.stopping() {
        sched.write_status();
        if crate::profile::enabled() && last_profile.elapsed() >= Duration::from_secs(30) {
            println!("{}", crate::profile::report(last_profile.elapsed().as_secs_f64()));
            last_profile = Instant::now();
        }
        nap(Duration::from_secs(1), || sched.stopping()).await;
    }
}

/// Index with this set of servers until stopping, the config needs a rebuild
/// or a compaction is due (true), not before `compact_after`.
async fn run_servers(
    config: &Config,
    pool: Arc<Pool>,
    states: RunStates,
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    compact_after: Option<SystemTime>,
) -> bool {
    let servers = usable_servers(config);
    let groups = config.tracked_groups();
    let plan = plan_workers(config);
    let workers: usize = plan.iter().map(|(_, w)| w).sum();

    let per_server: Vec<String> = plan
        .iter()
        .map(|&(i, w)| format!("{} ({} connections, {w} groups at once)", pool.host(i), pool.connections(i)))
        .collect();
    println!("indexing {} groups on {}", groups.len(), per_server.join(" + "));

    let sched = Arc::new(Scheduler {
        ctx: PassContext { pool, states, stop, verbose: false },
        settings: RwLock::new(settings_of(config)),
        groups: RwLock::new(groups),
        plan: plan.clone(),
        workers,
        next: Mutex::new(HashMap::new()),
        busy: Mutex::new(HashSet::new()),
        skip: Mutex::new(HashMap::new()),
        chunk_errors: Mutex::new(HashMap::new()),
        sweeping: Mutex::new(HashSet::new()),
        wait_until: Mutex::new(HashMap::new()),
        errors: Mutex::new(HashMap::new()),
        failed: Mutex::new(HashMap::new()),
        stats,
        wind_down: AtomicBool::new(false),
        compact_due: AtomicBool::new(false),
        compact_after,
        auto_compact: AtomicBool::new(config.auto_run_compact),
        passes: AtomicUsize::new(0),
    });

    let db = match db::open().and_then(|conn| db::tune_for_writing(&conn).map(|_| conn)) {
        Ok(conn) => {
            // before any pass: whose the plain cursors are (see claim_plain_cursors)
            if let Err(e) = crate::indexer::claim_plain_cursors(&conn, &sched.ctx.pool) {
                ui::warn(&format!("couldnt record whose the group cursors are: {e:#}"));
            }
            shared_db(conn)
        }
        Err(e) => {
            ui::error(&format!("couldnt open database: {e}"));
            return false;
        }
    };

    // checkpoints off the writer's path, see db::checkpointer
    let checkpoint_stop = Arc::new(AtomicBool::new(false));
    let checkpointer = db::checkpointer(checkpoint_stop.clone());

    let mut tasks = tokio::task::JoinSet::new();
    for &(server, count) in &plan {
        for _ in 0..count {
            tasks.spawn(worker(sched.clone(), server, db.clone()));
        }
    }
    tasks.spawn(watch_config(sched.clone(), servers));
    tasks.spawn(report(sched.clone()));
    tasks.spawn(compact_timer(sched.clone()));

    // the workers and the config watcher all stop on `stopping()`
    while tasks.join_next().await.is_some() {
        if sched.stopping() {
            sched.wind_down.store(true, Ordering::Relaxed);
        }
    }

    checkpoint_stop.store(true, Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || checkpointer.join()).await;
    // the last clone of the db waits for the writers (one may be sealing)
    let _ = tokio::task::spawn_blocking(move || drop(db)).await;
    sched.compact_due.load(Ordering::Relaxed)
}

/// How an auto compaction went, for what the indexer does next.
enum Compacted {
    /// done, failed or stopped: indexing goes on
    Ran,
    /// another compaction or a menu write held the lock, nothing was touched
    Busy,
    /// a shard is missing afterwards: the indexer stops
    ShardMissing,
}

/// Compact every shard in this process (the indexer is wound down and its
/// writers are gone, soo nothing else writes), then note the time. Noted on
/// failure too, soo a failing compaction is tried again in 24 hours, not every
/// minute. Not noted when it was stopped: that wasnt a failure, and it runs
/// again on the next start. Nor when it couldnt take the lock: it's tried
/// again after COMPACT_RETRY.
async fn compact(config: &Config, stats: &Arc<Mutex<Stats>>, stop: &Arc<AtomicBool>) -> Compacted {
    write_status(true, "compacting the database", &config.index_mode, false, "running", false, 0);
    let main = paths::database();
    let started = Instant::now();
    let (path, flag) = (main.clone(), stop.clone());
    let report = |msg: &str| println!("{msg}");
    let result = tokio::task::spawn_blocking(move || crate::compact::run(&path, &report, &flag)).await;
    let stopped = stop.load(Ordering::Relaxed);
    match result {
        Ok(Ok(_)) => println!("compacted in {}", crate::dashboard::human_time(started.elapsed().as_secs() as i64)),
        Ok(Err(e)) if e.downcast_ref::<crate::compact::Busy>().is_some() => {
            ui::warn(&format!("couldnt compact the database: {e}. trying again in {}m", COMPACT_RETRY.as_secs() / 60));
            return Compacted::Busy;
        }
        Ok(Err(e)) if stopped => println!("compaction stopped, the shards left are as they were: {e:#}"),
        Ok(Err(e)) => {
            ui::error(&format!("auto compaction failed, the originals were kept: {e:#}"));
            stats.lock().unwrap().error_count += 1;
        }
        Err(e) => {
            ui::error(&format!("auto compaction stopped: {e}"));
            stats.lock().unwrap().error_count += 1;
        }
    }

    // a swap that was cut half way leaves its shard missing, and a writer
    // would make an empty one there
    let missing: Vec<String> =
        crate::store::shard_paths(&main).iter().filter(|p| !p.exists()).map(|p| p.display().to_string()).collect();
    if !missing.is_empty() {
        ui::error(&format!(
            "shards missing after compacting, stopping the indexer. restore from the .precompact.db / .compact.db files next to them: {}",
            missing.join(", ")
        ));
        stats.lock().unwrap().error_count += 1;
        return Compacted::ShardMissing;
    }

    if !stopped && let Ok(conn) = db::open_at(&main) {
        let _ = crate::store::set_meta(&conn, "last_compact", chrono::Utc::now().timestamp());
    }
    Compacted::Ran
}

/// Index until stopping. Returns the exit code: not 0 when a compaction held
/// the database at the start. `writing` is the hold on compaction taken
/// before setting up the database, if there was one.
async fn supervise(stop: Arc<AtomicBool>, stats: Arc<Mutex<Stats>>, writing: Option<WriteGuard>) -> i32 {
    let states = RunStates::default();
    let stopping = || stop.load(Ordering::Relaxed);
    let main = paths::database();
    // a compaction that couldnt take the lock waits till then
    let mut compact_after = None;
    // indexing writes the shards: no compaction from elsewhere while it runs
    // (its copy would miss what's written meanwhile). let go only for this
    // indexer's own compaction, taken again after it
    let mut writing = writing;
    let mut started = false;

    while !stopping() {
        let Some(config) = load_config() else {
            let why = crate::config::file_problem().unwrap_or_else(|| "config.json not found".into());
            ui::error(&format!("error with config, stopped: {why}"));
            break;
        };

        let servers = usable_servers(&config);
        if servers.is_empty() {
            ui::error("config missing required fields");
            break;
        }

        if writing.is_none() {
            match crate::compact::hold_off_compaction(&main) {
                Ok(guard) => writing = Some(guard),
                Err(e) if !started => {
                    ui::error(&format!("not indexing: {e:#}"));
                    write_status(false, "", &config.index_mode, false, "stopped", true, 1);
                    return 1;
                }
                Err(e) => {
                    ui::warn(&format!("cant index yet: {e:#}"));
                    nap(Duration::from_secs(30), stopping).await;
                    continue;
                }
            }
        }
        started = true;

        let pool = Arc::new(Pool::new(&servers).with_max_unsaved(config.max_unsaved_headers()));

        // at least one server has to answer before spinning everything up
        if let Err(e) = pool.connect().await {
            ui::error(&format!("couldnt reach any usenet server: {e}"));
            stats.lock().unwrap().error_count += 1;
            write_status(true, "", &config.index_mode, false, "warning", false, 1);
            nap(Duration::from_secs(30), stopping).await;
            continue;
        }

        let compact_due =
            run_servers(&config, pool.clone(), states.clone(), stats.clone(), stop.clone(), compact_after).await;
        pool.close().await;

        if compact_due && !stopping() {
            writing = None;
            match compact(&config, &stats, &stop).await {
                Compacted::Ran => compact_after = None,
                Compacted::Busy => compact_after = Some(SystemTime::now() + COMPACT_RETRY),
                Compacted::ShardMissing => break,
            }
        }
    }
    0
}

pub fn run() -> i32 {
    let Some(config) = load_config().filter(|c| !c.servers.is_empty()) else {
        match crate::config::file_problem() {
            Some(problem) => println!("{problem}"),
            None => println!("no valid config"),
        }
        return 1;
    };

    if usable_servers(&config).is_empty() {
        ui::warn("no password stored for any server, run atlas to set it up first");
        return 1;
    }

    if let Err(e) = write_atomic(&paths::pid_file(), std::process::id().to_string()) {
        ui::error(&format!("couldnt write pid file: {e}"));
        return 1;
    }

    // compaction held off from before setting up: a shard it's swapping
    // would look missing. kept for indexing
    let writing = match db::create_db() {
        Ok(Some(held)) => match crate::compact::refuse_unresolved_backups(&paths::database()) {
            Ok(()) => held,
            Err(e) => {
                ui::error(&format!("not indexing: {e:#}"));
                write_status(false, "", &config.index_mode, false, "stopped", true, 1);
                return 1;
            }
        },
        Ok(None) => {
            ui::error("not indexing: the database is being compacted; try again later");
            write_status(false, "", &config.index_mode, false, "stopped", true, 1);
            return 1;
        }
        Err(e) => {
            ui::error(&format!("couldnt open database: {e:#}"));
            return 1;
        }
    };

    // one time move of an older database into the shards, here and not in the
    // menu because it takes a while on a big one. the menu shows the progress
    let main = paths::database();
    let report = |msg: &str| {
        println!("{msg}");
        write_status(true, msg, &config.index_mode, false, "running", false, 0);
    };
    let writing = match convert_at_start(&main, writing, &report) {
        Ok(held) => held,
        Err(e) => {
            ui::error(&format!("not indexing: {e:#}"));
            write_status(false, "", &config.index_mode, false, "stopped", true, 1);
            return 1;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));

    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone());
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone());
    }

    index(config, stop, Some(writing))
}

/// The one time move into the shards at start, if `main` needs it, with
/// `setup` the hold taken for setting up. Returns the hold to index under.
/// The conversion runs alone, under the exclusive lock (`convert::run_alone`):
/// another indexer starting, or a `--convert`, mustnt convert alongside it.
/// The setup hold is let go for it, and taken again after, setting up the
/// converted database: refused if a compaction got in between.
pub fn convert_at_start(
    main: &std::path::Path,
    setup: WriteGuard,
    report: &dyn Fn(&str),
) -> anyhow::Result<WriteGuard> {
    use anyhow::Context as _;
    if !crate::convert::needed(main) {
        return Ok(setup);
    }
    drop(setup);
    crate::convert::run_alone(main, report).context("couldnt convert the database, nothing was changed")?;
    db::create_db_holding(main)?.ok_or_else(|| anyhow::anyhow!("the database is being compacted; try again later"))
}

/// The indexing loop from `run`, stopping once `stop` is set (tests set it
/// directly, `run` wires it to SIGTERM / SIGINT).
pub fn run_until(config: Config, stop: Arc<AtomicBool>) -> i32 {
    index(config, stop, None)
}

fn index(config: Config, stop: Arc<AtomicBool>, writing: Option<WriteGuard>) -> i32 {
    // start sabnzbd soo its ready when you wanna download. on its own thread
    // soo indexing doesnt sit around for up to 90s waiting on it
    if sab::available() && !sab::is_running() {
        thread::spawn(|| {
            if sab::start() {
                sab::wait_ready(Duration::from_secs(90));
            }
        });
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("atlas-indexer")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            ui::error(&format!("couldnt start the indexer runtime: {e}"));
            return 1;
        }
    };

    let stats = Arc::new(Mutex::new(Stats::new()));
    write_status(true, &groups_label(&config.tracked_groups(), 0), &config.index_mode, false, "running", false, 0);

    let code = runtime.block_on(supervise(stop, stats.clone(), writing));
    if code != 0 {
        return code;
    }

    write_status(false, "", &config.index_mode, false, "stopped", false, 0);
    stats.lock().unwrap().write("", "", false, false);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_only_for_tracked_groups_in_a_backfilling_mode() {
        let groups = vec!["a".to_string(), "b".to_string()];
        let skip = HashSet::from(["b".to_string()]);
        assert_eq!(chunk_groups("backfill", &groups, &skip), HashSet::from(["a".to_string()]));
        assert_eq!(chunk_groups("dynamic", &groups, &HashSet::new()).len(), 2);
        assert!(chunk_groups("live", &groups, &HashSet::new()).is_empty(), "live never backfills");
    }

    fn scheduler(pool: Pool, groups: Vec<String>) -> Scheduler {
        Scheduler {
            ctx: PassContext {
                pool: Arc::new(pool),
                states: RunStates::default(),
                stop: Default::default(),
                verbose: false,
            },
            settings: RwLock::new(PassSettings { mode: "backfill".into(), ..PassSettings::default() }),
            groups: RwLock::new(groups),
            plan: vec![],
            workers: 2,
            next: Default::default(),
            busy: Default::default(),
            skip: Default::default(),
            chunk_errors: Default::default(),
            sweeping: Default::default(),
            wait_until: Default::default(),
            errors: Default::default(),
            failed: Default::default(),
            stats: Arc::new(Mutex::new(Stats::new())),
            wind_down: AtomicBool::new(false),
            compact_due: AtomicBool::new(false),
            compact_after: None,
            auto_compact: AtomicBool::new(false),
            passes: AtomicUsize::new(0),
        }
    }

    /// a sweep due on a server comes before another group's day chunks
    #[test]
    fn a_due_sweep_comes_before_other_groups_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        db::create_db_at(&main).unwrap();
        let pool = Pool::new(&[UsenetServer::new("a.test", "u", "p", 119), UsenetServer::new("b.test", "u", "p", 119)]);
        // "done" split with its chunks all done, "busy" with many to go
        let conn = db::open_at(&main).unwrap();
        crate::chunks::add(&conn, "done", 20_000, 19_990).unwrap();
        conn.execute("update backfill_chunks set state = 2 where grp = 'done'", []).unwrap();
        crate::chunks::add(&conn, "busy", 20_000, 10_000).unwrap();
        // the server that isnt done's home sweeps it
        let server = 1 - pool.pick_server("done");
        let sched = scheduler(pool, vec!["busy".into(), "done".into()]);
        let db = shared_db(conn);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            assert_eq!(take_sweep(&sched, server, &db).await.as_deref(), Some("done"));
            // one sweep per server at a time: the next worker takes a chunk
            assert!(take_sweep(&sched, server, &db).await.is_none());
            assert!(matches!(take_chunk(&sched, server, &db).await, Some(c) if c.group == "busy"));
        });
    }

    /// home groups that keep every worker busy dont hold a due sweep off
    #[test]
    fn a_due_sweep_runs_while_home_groups_keep_workers_busy() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        db::create_db_at(&main).unwrap();
        let pool = Pool::new(&[UsenetServer::new("a.test", "u", "p", 119), UsenetServer::new("b.test", "u", "p", 119)]);
        let conn = db::open_at(&main).unwrap();
        crate::chunks::add(&conn, "done", 20_000, 19_990).unwrap();
        conn.execute("update backfill_chunks set state = 2 where grp = 'done'", []).unwrap();
        let server = 1 - pool.pick_server("done");
        // more home groups on that server than it has workers
        let homes: Vec<String> =
            (0..).map(|i| format!("home{i}")).filter(|g| pool.pick_server(g) == server).take(4).collect();
        let mut groups = homes.clone();
        groups.push("done".into());
        let sched = scheduler(pool, groups);
        let db = shared_db(conn);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            assert!(matches!(next_work(&sched, server, &db, false).await, Some(Work::Sweep(g)) if g == "done"));
            // one sweeper per server: the others keep to their groups
            assert!(matches!(next_work(&sched, server, &db, false).await, Some(Work::Group(_))));
            assert!(matches!(next_work(&sched, server, &db, false).await, Some(Work::Group(_))));
        });
    }

    /// a worker alone on its server alternates a due sweep with its home group
    #[test]
    fn a_lone_worker_alternates_a_due_sweep_with_its_home_group() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        db::create_db_at(&main).unwrap();
        let pool = Pool::new(&[UsenetServer::new("a.test", "u", "p", 119), UsenetServer::new("b.test", "u", "p", 119)]);
        let conn = db::open_at(&main).unwrap();
        crate::chunks::add(&conn, "done", 20_000, 19_990).unwrap();
        conn.execute("update backfill_chunks set state = 2 where grp = 'done'", []).unwrap();
        let server = 1 - pool.pick_server("done");
        let home = (0..).map(|i| format!("home{i}")).find(|g| pool.pick_server(g) == server).unwrap();
        let sched = scheduler(pool, vec![home.clone(), "done".into()]);
        let db = shared_db(conn);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // the sweep first, as before
            assert!(matches!(next_work(&sched, server, &db, false).await, Some(Work::Sweep(g)) if g == "done"));
            sched.sweeping.lock().unwrap().remove(&server);
            // then the due home group, though the sweep is still due
            assert!(matches!(next_work(&sched, server, &db, true).await, Some(Work::Group(g)) if g == home));
            // and the sweep again after the group
            assert!(matches!(next_work(&sched, server, &db, false).await, Some(Work::Sweep(g)) if g == "done"));
        });
    }
}
