//! Stats dashboard: pages of numbers about the indexer, the backfill, the
//! indexed content and the usenet servers. Switch pages with ←/→ or 1-4.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Cell, Chart, Dataset, Gauge, GraphType, Paragraph, Row, Table, Tabs};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::config::load_config;
use crate::dashboard::{human_bytes, human_time};
use crate::db;
use crate::paths;

const PAGES: [&str; 5] = ["Overview", "Backfill", "Content", "Servers", "Bottleneck"];
/// file types counted on the content page
const FILE_TYPES: [&str; 12] = ["mkv", "mp4", "avi", "m4v", "iso", "m4a", "flac", "mp3", "epub", "pdf", "exe", "nzb"];

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn load_json(path: &std::path::Path) -> Value {
    fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// 533179307 -> "533,179,307"
pub fn count(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// seconds -> "3d 4h", "5h 12m", "42m", "30s"
pub fn span(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "-".into();
    }
    let s = secs as u64;
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    if d >= 365 {
        format!("{:.1} years", d as f64 / 365.0)
    } else if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

fn pct(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { (part / whole * 100.0).clamp(0.0, 100.0) } else { 0.0 }
}

// ---------------------------------------------------------------- data

/// Cheap numbers, refreshed every few seconds.
#[derive(Default, Clone)]
struct Quick {
    progress: Vec<db::GroupProgress>,
    articles_approx: i64,
    releases_approx: i64,
    db_bytes: u64,
    wal_bytes: u64,
    /// split group stats: (groups, splitting, done, total, done_last_hour)
    chunks: (i64, i64, i64, i64, i64),
    /// split group -> (chunks done, chunks): their cursors stand still, the chunks move
    group_chunks: BTreeMap<String, (i64, i64)>,
}

fn load_group_chunks(conn: &rusqlite::Connection) -> BTreeMap<String, (i64, i64)> {
    let Ok(mut stmt) =
        conn.prepare("select grp, coalesce(sum(state = 2), 0), count(*) from backfill_chunks group by grp")
    else {
        return BTreeMap::new();
    };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))));
    rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
}

fn load_chunks_stats(conn: &rusqlite::Connection) -> (i64, i64, i64, i64, i64) {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let hour_ago = now - 3600;
    let sql = "select count(*), coalesce(sum(pending > 0), 0), coalesce(sum(done), 0), coalesce(sum(total), 0), \
               coalesce(sum(recent), 0)
               from (select grp, sum(state != 2) as pending, sum(state = 2) as done, count(*) as total, \
                     sum(state = 2 and done_at > ?) as recent from backfill_chunks group by grp)";
    conn.query_row(sql, [hour_ago], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap_or_default()
}

fn load_quick() -> Quick {
    // the main database and its shards
    let main = paths::database();
    let mut q = Quick {
        db_bytes: crate::store::database_bytes(&main),
        wal_bytes: crate::store::wal_bytes(&main),
        ..Quick::default()
    };

    let Ok(conn) = db::open() else { return q };
    q.progress = db::group_progress(&conn).unwrap_or_default();
    // running totals the shards keep, instant
    (q.releases_approx, q.articles_approx) = crate::store::totals(&conn).unwrap_or_default();
    q.chunks = load_chunks_stats(&conn);
    q.group_chunks = load_group_chunks(&conn);
    q
}

/// The expensive numbers (full scans), cached on disk between runs.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Content {
    computed_at: f64,
    took_secs: f64,
    releases: i64,
    complete: i64,
    obfuscated: i64,
    named: i64,
    total_bytes: i64,
    total_parts: i64,
    groups_with_releases: i64,
    /// releases with at least one saved article: an nzb can be built for them
    #[serde(default)]
    nzb_ready: i64,
    /// of those, the ones whose files all have every part
    #[serde(default)]
    nzb_complete: i64,
    /// (group, releases, bytes)
    top_groups: Vec<(String, i64, i64)>,
    biggest: Option<(String, i64, String)>,
    file_types: Vec<(String, i64)>,
    framestor: i64,
}

fn cache_path() -> std::path::PathBuf {
    paths::app_dir().join("stats_cache.json")
}

fn compute_content() -> rusqlite::Result<Content> {
    let started = Instant::now();
    let conn = db::open()?;
    let mut c = Content::default();

    // one pass over each shard's releases for every per group total (a group
    // lives in one shard)
    let mut stmt = conn.prepare(&crate::store::each_shard(|i| {
        format!(
            "select group_name, count(*), coalesce(sum(size), 0), coalesce(sum(complete), 0),
                coalesce(sum(is_obfuscated), 0), count(display_name), coalesce(sum(parts), 0),
                coalesce(sum(parts > 0), 0), coalesce(sum(parts > 0 and complete = 1), 0)
             from s{i}.releases group by group_name"
        )
    }))?;
    let mut groups: Vec<(String, i64, i64)> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let (name, n, bytes): (Option<String>, i64, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
        c.releases += n;
        c.total_bytes += bytes;
        c.complete += r.get::<_, i64>(3)?;
        c.obfuscated += r.get::<_, i64>(4)?;
        c.named += r.get::<_, i64>(5)?;
        c.total_parts += r.get::<_, i64>(6)?;
        c.nzb_ready += r.get::<_, i64>(7)?;
        c.nzb_complete += r.get::<_, i64>(8)?;
        groups.push((name.unwrap_or_default(), n, bytes));
    }
    drop(rows);
    drop(stmt);

    c.groups_with_releases = groups.len() as i64;
    groups.sort_by_key(|g| std::cmp::Reverse(g.1));
    c.top_groups = groups.into_iter().take(8).collect();

    // a lone max() picks its row's other columns too
    let biggest = format!(
        "select * from ({}) order by 1 desc limit 1",
        crate::store::each_shard(|i| format!(
            "select * from (select max(size), coalesce(display_name, name), group_name from s{i}.releases)"
        ))
    );
    c.biggest = conn
        .query_row(&biggest, [], |r| {
            Ok(r.get::<_, Option<i64>>(0)?
                .map(|size| (r.get::<_, String>(1).unwrap_or_default(), size, r.get(2).unwrap_or_default())))
        })
        .ok()
        .flatten()
        .map(|(name, size, group): (String, i64, String)| (name, size, group));

    // file types and names through the full text index: fast, and it covers the
    // posted name and the real name from par2/nfo
    let fts_sql = format!(
        "select coalesce(sum(c), 0) from ({})",
        crate::store::each_shard(|i| format!(
            "select count(*) as c from s{i}.releases_fts where releases_fts match ?1"
        ))
    );
    let fts = |q: &str| -> i64 { conn.query_row(&fts_sql, [q], |r| r.get(0)).unwrap_or(0) };
    c.file_types = FILE_TYPES.iter().map(|t| (t.to_string(), fts(&format!("\"{t}\"")))).collect();
    c.framestor = fts("\"framestor\"*");

    c.computed_at = now();
    c.took_secs = started.elapsed().as_secs_f64();
    Ok(c)
}

#[derive(Default)]
struct ContentState {
    data: Option<Content>,
    running: bool,
    started: Option<Instant>,
    error: Option<String>,
}

fn start_content(state: &Arc<Mutex<ContentState>>) {
    {
        let mut s = state.lock().unwrap();
        if s.running {
            return;
        }
        s.running = true;
        s.started = Some(Instant::now());
        s.error = None;
    }

    let state = state.clone();
    thread::spawn(move || {
        let result = compute_content();
        let mut s = state.lock().unwrap();
        s.running = false;
        match result {
            Ok(c) => {
                if let Ok(json) = serde_json::to_string(&c) {
                    let _ = crate::atomic::write_atomic(&cache_path(), json);
                }
                s.data = Some(c);
            }
            Err(e) => s.error = Some(e.to_string()),
        }
    });
}

/// CPU / memory of the indexer and of this process
#[derive(Default, Clone)]
struct ProcInfo {
    cpu: f32,
    cpu_time_ms: u64,
    rss: u64,
    run_secs: u64,
    /// bytes read from / written to disk since the process started
    disk_read: u64,
    disk_written: u64,
}

struct Sys {
    sys: System,
    me: Pid,
}

impl Sys {
    fn new() -> Self {
        Sys { sys: System::new(), me: Pid::from_u32(std::process::id()) }
    }

    fn sample(&mut self, indexer: Option<u32>) -> (Option<ProcInfo>, ProcInfo, u64, u64) {
        let mut pids = vec![self.me];
        if let Some(p) = indexer {
            pids.push(Pid::from_u32(p));
        }
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage(),
        );
        self.sys.refresh_memory();

        let info = |pid: Pid| {
            self.sys.process(pid).map(|p| ProcInfo {
                cpu: p.cpu_usage(),
                cpu_time_ms: p.accumulated_cpu_time(),
                rss: p.memory(),
                run_secs: p.run_time(),
                disk_read: p.disk_usage().total_read_bytes,
                disk_written: p.disk_usage().total_written_bytes,
            })
        };

        let indexer = indexer.and_then(|p| info(Pid::from_u32(p)));
        let me = info(self.me).unwrap_or_default();
        (indexer, me, self.sys.used_memory(), self.sys.total_memory())
    }
}

// ---------------------------------------------------------------- app

struct App {
    page: usize,
    long_graph: bool,
    stats: Value,
    status: Value,
    quick: Arc<Mutex<Quick>>,
    content: Arc<Mutex<ContentState>>,
    sys: Sys,
    indexer: Option<ProcInfo>,
    me: ProcInfo,
    mem_used: u64,
    mem_total: u64,
    /// (time, wire bytes, text bytes) at the previous refresh, for MB/s
    traffic_prev: Option<(Instant, u64, u64)>,
    traffic_rate: f64,
    /// the indexer's load counters over the last few seconds, for rates
    readings: std::collections::VecDeque<Reading>,
}

/// One reading of the indexer's load counters (stats.json `load`) and its disk use.
#[derive(Clone)]
struct Reading {
    load: Value,
    /// (when, bytes read, bytes written)
    disk: Option<(f64, u64, u64)>,
}

/// readings this far apart give the rates on the bottleneck page
const LOAD_WINDOW: f64 = 10.0;

/// The indexer's load, per second over the last `LOAD_WINDOW`.
#[derive(Clone, Debug, Default, PartialEq)]
struct LoadRates {
    /// share of the time the db writer was saving (0..1)
    writer_busy: f64,
    /// slices waiting for the writer now
    queued: u64,
    /// headers fetched and not saved yet now, and the cap on them
    unsaved: u64,
    max_unsaved: u64,
    /// slices saved per transaction
    per_batch: f64,
    /// share of the time the writer spent finishing checkpoints (part of writer_busy)
    writer_checkpoint: f64,
    /// seconds a header request takes on average
    latency: f64,
    /// header requests in flight on average
    in_flight: f64,
    /// requests waiting for a free connection on average
    waiting: f64,
    /// cores busy parsing headers
    parse_cores: f64,
    /// indexer disk bytes per second
    disk_read: f64,
    disk_write: f64,
}

fn load_rates(old: &Reading, new: &Reading) -> Option<LoadRates> {
    let num = |r: &Reading, k: &str| r.load[k].as_f64().unwrap_or(0.0);
    let dt = num(new, "at") - num(old, "at");
    if dt <= 0.0 {
        return None;
    }
    let delta = |k: &str| (num(new, k) - num(old, k)).max(0.0);
    let ns = |k: &str| delta(k) / 1e9;
    // the writers' times add up (one per shard): busy is the average of them
    let writers = new.load["writers"].as_f64().unwrap_or(1.0).max(1.0);
    let (xovers, batches) = (delta("xovers"), delta("writer_batches"));
    let (disk_read, disk_write) = match (old.disk, new.disk) {
        (Some((t0, r0, w0)), Some((t1, r1, w1))) if t1 > t0 => {
            (r1.saturating_sub(r0) as f64 / (t1 - t0), w1.saturating_sub(w0) as f64 / (t1 - t0))
        }
        _ => (0.0, 0.0),
    };
    Some(LoadRates {
        writer_busy: (ns("writer_busy_ns") / dt / writers).min(1.0),
        queued: new.load["writer_queued"].as_u64().unwrap_or(0),
        unsaved: new.load["unsaved_headers"].as_u64().unwrap_or(0),
        max_unsaved: new.load["max_unsaved_headers"].as_u64().unwrap_or(0),
        per_batch: if batches > 0.0 { delta("writer_slices") / batches } else { 0.0 },
        writer_checkpoint: (ns("writer_checkpoint_ns") / dt / writers).min(1.0),
        latency: if xovers > 0.0 { ns("xover_ns") / xovers } else { 0.0 },
        in_flight: ns("xover_ns") / dt,
        waiting: ns("lease_wait_ns") / dt,
        parse_cores: ns("parse_ns") / dt,
        disk_read,
        disk_write,
    })
}

/// What limits indexing right now, and why: the busiest of the db writer,
/// the usenet connections and the CPU.
fn diagnose(r: &LoadRates, conns: (u64, u64), cpu_cores: f64, cores: usize) -> (String, String) {
    let (in_use, allowed) = conns;
    let conn_use = if allowed > 0 { in_use as f64 / allowed as f64 } else { 0.0 };
    let cpu_use = cpu_cores / cores.max(1) as f64;

    if r.writer_busy >= 0.85 {
        let why = if r.disk_read > 50e6 {
            format!(
                "and it mostly waits on the disk ({}/s read): the database is bigger than what stays cached",
                human_bytes(r.disk_read)
            )
        } else {
            "and the disk isnt busy, soo it is the work of saving itself".into()
        };
        return (
            "the database writers".into(),
            format!(
                "saving slices takes {:.0}% of the time, {} slices are waiting, {why}",
                r.writer_busy * 100.0,
                r.queued
            ),
        );
    }
    if conn_use >= 0.9 || r.waiting >= 1.0 {
        return (
            "usenet connections".into(),
            format!(
                "{in_use} of {allowed} connections are busy and {:.1} requests wait for one: more connections or servers would help",
                r.waiting
            ),
        );
    }
    if cpu_use >= 0.85 {
        return ("CPU".into(), format!("{cpu_cores:.1} of {cores} cores are busy"));
    }
    (
        "nothing is maxed out".into(),
        format!(
            "connections are {:.0}% busy and the writer {:.0}%: more groups at once (parallel_groups) would keep them busier. \
             a request takes {:.0}ms",
            conn_use * 100.0,
            r.writer_busy * 100.0,
            r.latency * 1000.0
        ),
    )
}

impl App {
    fn refresh(&mut self) {
        self.stats = load_json(&paths::stats_file());
        self.status = load_json(&paths::status_file());

        let running = self.status["running"].as_bool().unwrap_or(false);
        let pid = self.status["pid"].as_u64().map(|p| p as u32).filter(|_| running);
        (self.indexer, self.me, self.mem_used, self.mem_total) = self.sys.sample(pid);

        let (wire, text) = servers(&self.stats).iter().fold((0, 0), |(w, t), s| {
            (w + s["wire_bytes"].as_u64().unwrap_or(0), t + s["text_bytes"].as_u64().unwrap_or(0))
        });
        if let Some((at, prev_wire, _)) = self.traffic_prev {
            let dt = at.elapsed().as_secs_f64();
            if dt > 0.0 && wire >= prev_wire {
                self.traffic_rate = (wire - prev_wire) as f64 / dt;
            }
        }
        self.traffic_prev = Some((Instant::now(), wire, text));

        // a new load reading each time the indexer writes stats.json
        let load = self.stats["load"].clone();
        let fresh = !load.is_null() && self.readings.back().is_none_or(|r| r.load["at"] != load["at"]);
        if fresh {
            let disk = self.indexer.as_ref().map(|p| (now(), p.disk_read, p.disk_written));
            self.readings.push_back(Reading { load, disk });
            let newest = self.readings.back().and_then(|r| r.load["at"].as_f64()).unwrap_or(0.0);
            while self.readings.len() > 2
                && self.readings.get(1).and_then(|r| r.load["at"].as_f64()).is_some_and(|t| newest - t >= LOAD_WINDOW)
            {
                self.readings.pop_front();
            }
        }
    }

    fn load_rates(&self) -> Option<LoadRates> {
        load_rates(self.readings.front()?, self.readings.back()?)
    }

    /// (headers/s over the last minute, average over the window, peak bin)
    fn rates(&self) -> (f64, f64, f64) {
        let bin = self.stats["rate_bin"].as_f64().unwrap_or(10.0);
        let bins = rate_bins(&self.stats);
        let t = now();
        let recent: f64 = bins.iter().filter(|(b, _)| *b as f64 >= t - 60.0 - bin).map(|(_, h)| *h as f64).sum();
        let recent_rate = recent / (60.0 + bin);
        let avg = match (bins.first(), bins.last()) {
            (Some(f), Some(_)) => bins.iter().map(|b| b.1 as f64).sum::<f64>() / (t - f.0 as f64).max(bin),
            _ => 0.0,
        };
        let peak = bins.iter().map(|b| b.1 as f64 / bin).fold(0.0, f64::max);
        (recent_rate, avg, peak)
    }
}

fn servers(stats: &Value) -> Vec<Value> {
    stats["servers"].as_array().cloned().unwrap_or_default()
}

/// (bin start, headers) pairs from stats.json
fn rate_bins(stats: &Value) -> Vec<(i64, i64)> {
    stats["rate"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| Some((r.get(0)?.as_i64()?, r.get(1)?.as_i64()?))).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- drawing

fn panel(title: &str) -> Block<'_> {
    Block::bordered().title(Span::raw(format!(" {title} ")).bold()).border_style(Style::new().fg(Color::DarkGray))
}

fn kv<'a>(key: &str, value: impl Into<String>) -> Line<'a> {
    Line::from(vec![Span::raw(format!("{key:<18}")).dim(), Span::raw(value.into()).fg(Color::Cyan)])
}

fn note<'a>(text: &str) -> Line<'a> {
    Line::from(Span::raw(text.to_string()).dim().italic())
}

fn draw(f: &mut Frame, app: &App) {
    let [tabs, body, foot] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());

    let titles: Vec<Line> = PAGES.iter().enumerate().map(|(i, p)| Line::from(format!(" {} {p} ", i + 1))).collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.page)
            .block(panel("Atlas stats"))
            .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        tabs,
    );

    match app.page {
        0 => draw_overview(f, app, body),
        1 => draw_backfill(f, app, body),
        2 => draw_content(f, app, body),
        3 => draw_servers(f, app, body),
        _ => draw_bottleneck(f, app, body),
    }

    let keys = match app.page {
        0 => "  ←/→ or 1-5 pages   w 30m/6h graph   q back",
        2 => "  ←/→ or 1-5 pages   r recount   q back",
        _ => "  ←/→ or 1-5 pages   q back",
    };
    f.render_widget(Paragraph::new(Line::raw(keys).dim()), foot);
}

fn draw_overview(f: &mut Frame, app: &App, area: Rect) {
    let [top, graph] = Layout::vertical([Constraint::Length(12), Constraint::Min(6)]).areas(area);
    let [a, b, c] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(top);

    let s = &app.stats;
    let st = &app.status;
    let running = st["running"].as_bool().unwrap_or(false);
    let (state, color) = match (running, st["status"].as_str().unwrap_or("stopped")) {
        (false, _) => ("stopped", Color::Red),
        (true, "warning") => ("running, with errors", Color::Yellow),
        (true, "idle") => ("idle, caught up", Color::Yellow),
        _ => ("running", Color::Green),
    };

    let indexer = vec![
        Line::from(vec![Span::raw("● ").fg(color), Span::raw(state).fg(color).bold()]),
        Line::raw(""),
        kv("mode", s["mode"].as_str().unwrap_or("-")),
        kv("uptime", human_time(s["uptime"].as_i64().unwrap_or(0))),
        kv("groups", format!("{} configured", count(s["groups_configured"].as_i64().unwrap_or(0)))),
        kv("indexing at once", count(s["workers"].as_i64().unwrap_or(0))),
        kv("errors this run", count(s["error_count"].as_i64().unwrap_or(0))),
    ];
    f.render_widget(Paragraph::new(indexer).block(panel("indexer")), a);

    let (now_rate, avg, peak) = app.rates();
    let run_avg = s["total_articles"].as_f64().unwrap_or(0.0) / s["uptime"].as_f64().unwrap_or(1.0).max(1.0);
    let quick = app.quick.lock().unwrap().clone();
    let headers = vec![
        kv("now", format!("{} headers/s", count(now_rate as i64))),
        kv("average (this run)", format!("{} headers/s", count(run_avg as i64))),
        kv("average (graph)", format!("{} headers/s", count(avg as i64))),
        kv("peak (10s)", format!("{} headers/s", count(peak as i64))),
        kv("this run", count(s["total_articles"].as_i64().unwrap_or(0))),
        kv("per day at this", count((run_avg * 86_400.0) as i64)),
        kv("all time ≈", count(quick.articles_approx)),
        kv("releases ≈", count(quick.releases_approx)),
        kv(
            "database",
            format!("{} (+{} wal)", human_bytes(quick.db_bytes as f64), human_bytes(quick.wal_bytes as f64)),
        ),
    ];
    f.render_widget(Paragraph::new(headers).block(panel("headers")), b);

    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut system = match &app.indexer {
        Some(p) => vec![
            kv("indexer CPU", format!("{:.0}%  ({:.1} of {cores} cores)", p.cpu, p.cpu / 100.0)),
            kv("indexer CPU time", human_time((p.cpu_time_ms / 1000) as i64)),
            kv("CPU time / uptime", format!("{:.2}x", p.cpu_time_ms as f64 / 1000.0 / p.run_secs.max(1) as f64)),
            kv("indexer RAM", human_bytes(p.rss as f64)),
        ],
        None => vec![note("indexer not running")],
    };
    let wire: u64 = servers(s).iter().map(|v| v["wire_bytes"].as_u64().unwrap_or(0)).sum();
    let text: u64 = servers(s).iter().map(|v| v["text_bytes"].as_u64().unwrap_or(0)).sum();
    system.extend([
        kv("this menu RAM", human_bytes(app.me.rss as f64)),
        kv("system RAM", format!("{} of {}", human_bytes(app.mem_used as f64), human_bytes(app.mem_total as f64))),
        kv("network now", format!("{}/s", human_bytes(app.traffic_rate))),
        kv("downloaded", human_bytes(wire as f64)),
        kv("compression saved", format!("{:.0}%", 100.0 - pct(wire as f64, text as f64))),
    ]);
    f.render_widget(Paragraph::new(system).block(panel("system")), c);

    // headers/s graph, missing bins are zero
    let bin = s["rate_bin"].as_i64().unwrap_or(10).max(1);
    let window = if app.long_graph { 6 * 3600 } else { 30 * 60 };
    let t_now = now() as i64 / bin * bin;
    let bins: HashMap<i64, i64> = rate_bins(s).into_iter().collect();
    let points: Vec<(f64, f64)> = (0..=window / bin)
        .map(|i| {
            let t = t_now - window + i * bin;
            (((t - t_now) as f64) / 60.0, *bins.get(&t).unwrap_or(&0) as f64 / bin as f64)
        })
        .collect();
    let max_y = points.iter().map(|p| p.1).fold(1.0, f64::max);
    let label = if app.long_graph { "-6h" } else { "-30m" };
    let title = format!("headers/s, last {}", if app.long_graph { "6 hours" } else { "30 minutes" });

    let chart =
        Chart::new(vec![Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).cyan().data(&points)])
            .block(panel(&title))
            .x_axis(
                Axis::default()
                    .bounds([-(window as f64) / 60.0, 0.0])
                    .labels(vec![Span::raw(label).dim(), Span::raw("now").dim()]),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, max_y])
                    .labels(vec![Span::raw("0").dim(), Span::raw(count(max_y as i64)).dim()]),
            );
    f.render_widget(chart, graph);
}

/// a group's posting rate needs indexed posts at least this far apart
const MIN_DATED_SPAN: i64 = 30 * 60;

/// Backfill totals, one cursor row per group. A group has a row per server it
/// was indexed on (article numbers differ between providers); the row that got
/// furthest stands for the group, soo nothing is counted twice.
#[derive(Default)]
struct Backfill {
    total: i64,
    covered: i64,
    remaining: i64,
    measured: usize,
    /// groups whose rows have no article range yet
    unmeasured: usize,
    not_started: usize,
    done: usize,
    /// groups with indexed posts far enough apart in time to tell their posting rate
    dated: usize,
    /// articles posted per day over all measured groups, from the dated ones
    per_day: Option<f64>,
    /// how far back the backfill has reached in the middle group (unix time)
    reached: Option<i64>,
    /// (group, remaining, percent)
    behind: Vec<(String, i64, f64)>,
    nearly: Vec<(String, i64, f64)>,
}

/// (article numbers on the server, how many of them are indexed)
fn row_numbers(row: &db::GroupProgress) -> Option<(i64, i64)> {
    let (first, last) = (row.first?, row.last?);
    let total = (last - first + 1).max(0);
    let covered = (row.live_cursor.min(last) - row.backfill_cursor.max(first - 1)).clamp(0, total);
    Some((total, covered))
}

/// no usenet history goes back further than this (binary retention starts in the 2000s)
const EARLIEST_POST: i64 = 631_152_000; // 1990-01-01

/// Articles posted per day, from the dated ends of what was indexed. None
/// when they are too close together, or the dates cant be right: posted in
/// the future, before 1990, or a rate that would put the group's first
/// article (`total` back) before 1990.
fn posting_rate(row: &db::GroupProgress, total: i64, now: i64) -> Option<f64> {
    let (low, high) = (row.low?, row.high?);
    let (articles, secs) = (high.0 - low.0, high.1 - low.1);
    let plausible = |t: i64| (EARLIEST_POST..=now + 86_400).contains(&t);
    if articles <= 0 || secs < MIN_DATED_SPAN || !plausible(low.1) || !plausible(high.1) {
        return None;
    }
    let per_day = articles as f64 / (secs as f64 / 86_400.0);
    let history_days = total as f64 / per_day;
    (history_days <= (now - EARLIEST_POST) as f64 / 86_400.0).then_some(per_day)
}

fn backfill(quick: &Quick) -> Backfill {
    backfill_at(quick, now() as i64)
}

fn backfill_at(quick: &Quick, now: i64) -> Backfill {
    let mut b = Backfill::default();
    // group -> (total, covered, its row)
    let mut best: BTreeMap<String, (i64, i64, &db::GroupProgress)> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();

    for row in &quick.progress {
        let group = row.key.split('@').next().unwrap_or(&row.key).to_string();
        seen.insert(group.clone());
        let Some((total, covered)) = row_numbers(row) else { continue };
        let better = best.get(&group).is_none_or(|(t, c, _)| (covered, total) > (*c, *t));
        if better {
            best.insert(group, (total, covered, row));
        }
    }
    b.unmeasured = seen.iter().filter(|g| !best.contains_key(*g)).count();

    // a split group's cursors stand still: its day chunks say how far it is,
    // as that share of its article numbers. once they are all done its home
    // cursor sweeps on for what they missed, and the cursor says it again
    for (group, (total, covered, _)) in best.iter_mut() {
        if let Some(&(done, chunks)) = quick.group_chunks.get(group).filter(|c| c.1 > 0 && c.0 < c.1) {
            *covered = (*total as i128 * done.clamp(0, chunks) as i128 / chunks as i128) as i64;
        }
    }

    let mut rows: Vec<(String, i64, f64)> = Vec::new();
    let (mut rate_dated, mut total_dated, mut reached) = (0.0, 0i64, Vec::new());
    for (group, (total, covered, row)) in best {
        b.measured += 1;
        b.total += total;
        b.covered += covered;
        let left = total - covered;
        b.remaining += left;
        if left == 0 {
            b.done += 1;
        }
        if let Some(rate) = posting_rate(row, total, now) {
            b.dated += 1;
            rate_dated += rate;
            total_dated += total;
            reached.extend(row.low.map(|l| l.1));
        }
        rows.push((group, left, pct(covered as f64, total as f64)));
    }

    // the dated groups' rate, scaled up to every measured group by their article numbers
    if rate_dated > 0.0 && total_dated > 0 {
        b.per_day = Some(rate_dated * b.total as f64 / total_dated as f64);
    }
    reached.sort_unstable();
    b.reached = reached.get(reached.len() / 2).copied();

    let configured = load_config().map(|c| c.tracked_groups().len()).unwrap_or(0);
    b.not_started = configured.saturating_sub(seen.len());

    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    b.behind = rows.iter().take(8).cloned().collect();
    let mut nearly: Vec<_> = rows.into_iter().filter(|r| r.1 > 0).collect();
    nearly.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.1.cmp(&b.1)));
    b.nearly = nearly.into_iter().take(8).collect();
    b
}

fn draw_backfill(f: &mut Frame, app: &App, area: Rect) {
    let quick = app.quick.lock().unwrap().clone();
    let b = backfill(&quick);
    let (now_rate, _, _) = app.rates();
    let s = &app.stats;
    let run_avg = s["total_articles"].as_f64().unwrap_or(0.0) / s["uptime"].as_f64().unwrap_or(1.0).max(1.0);

    let [gauge, top, lists] =
        Layout::vertical([Constraint::Length(3), Constraint::Length(13), Constraint::Min(4)]).areas(area);
    let [left, right] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);

    let done = pct(b.covered as f64, b.total as f64);
    f.render_widget(
        Gauge::default()
            .block(panel("backfill progress (article numbers on the servers)"))
            .gauge_style(Style::new().fg(Color::Cyan).bg(Color::DarkGray))
            .ratio(done / 100.0)
            .label(format!("{done:.2}%  {} of {}", count(b.covered), count(b.total))),
        gauge,
    );

    let eta = |rate: f64| if rate > 0.0 { span(b.remaining as f64 / rate) } else { "-".into() };
    let progress = vec![
        kv("articles in total", count(b.total)),
        kv("processed", count(b.covered)),
        kv("left to process", count(b.remaining)),
        kv("done", format!("{done:.2}%")),
        kv("ETA at current rate", eta(now_rate)),
        kv("ETA at run average", eta(run_avg)),
        kv("groups finished", format!("{} of {}", count(b.done as i64), count(b.measured as i64))),
        kv("groups not started", count(b.not_started as i64)),
        note("counts are article numbers, gaps on the server included"),
    ];
    let mut progress = progress;
    let (groups, splitting, done, total, done_last_hour) = quick.chunks;
    if groups > 0 {
        progress.push(kv(
            "split groups",
            format!(
                "{} ({} in progress), day chunks {} of {} done, {} in the last hour",
                groups,
                splitting,
                count(done),
                count(total),
                count(done_last_hour)
            ),
        ));
    }
    if b.unmeasured > 0 {
        progress.push(note(&format!("{} groups get their range on their next pass", count(b.unmeasured as i64))));
    }
    f.render_widget(Paragraph::new(progress).block(panel("progress")), left);

    // usenet history: how many days of posts the article numbers stand for
    let history = match b.per_day {
        Some(per_day) => {
            let days = |articles: i64| span(articles as f64 / per_day * 86_400.0);
            let reached = b
                .reached
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "-".into());
            vec![
                kv("posts per day", format!("{} articles", count(per_day as i64))),
                kv("history indexed", days(b.covered)),
                kv("history in total", days(b.total)),
                kv("history left", days(b.remaining)),
                kv("days per day", format!("{:.2} days of usenet per day of indexing", run_avg * 86_400.0 / per_day)),
                kv("backfill reached", format!("{reached} (middle group)")),
                note(&format!("posting rates from {} of {} groups", b.dated, b.measured)),
                note("history = articles ÷ posts per day"),
            ]
        }
        None => vec![
            note("working out posting rates: each group needs indexed"),
            note("posts at least 30 minutes apart, which takes a few"),
            note("passes after starting the indexer"),
        ],
    };
    f.render_widget(Paragraph::new(history).block(panel("usenet history")), right);

    let [behind, nearly] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(lists);
    let table = |rows: &[(String, i64, f64)], title: &'static str| {
        Table::new(
            rows.iter().map(|(g, left, p)| {
                Row::new(vec![Cell::from(g.clone()).cyan(), Cell::from(count(*left)), Cell::from(format!("{p:.1}%"))])
            }),
            [Constraint::Fill(1), Constraint::Length(14), Constraint::Length(7)],
        )
        .header(Row::new(vec!["group", "left", "done"]).bold())
        .block(panel(title))
    };
    f.render_widget(table(&b.behind, "most left"), behind);
    f.render_widget(table(&b.nearly, "nearly done"), nearly);
}

fn draw_content(f: &mut Frame, app: &App, area: Rect) {
    let state = app.content.lock().unwrap();
    let [status, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).areas(area);

    let line = match (&state.data, state.running, &state.error) {
        (_, true, _) => Line::from(format!(
            " counting... {} so far (reads every release, can take a few minutes on a big database)",
            span(state.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0))
        ))
        .yellow(),
        (_, _, Some(e)) => Line::from(format!(" count failed: {e}")).red(),
        (Some(c), _, _) => {
            Line::from(format!(" counted {} ago in {}  (r to recount)", span(now() - c.computed_at), span(c.took_secs)))
                .dim()
        }
        (None, _, _) => Line::from(" press r to count").dim(),
    };
    f.render_widget(Paragraph::new(line).block(panel("content")), status);

    let Some(c) = state.data.clone() else { return };
    drop(state);

    let [left, mid, right] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(body);
    let r = c.releases as f64;
    let configured =
        app.stats["groups_configured"].as_i64().or_else(|| load_config().map(|c| c.tracked_groups().len() as i64));
    let releases = vec![
        kv("groups configured", configured.map(count).unwrap_or_else(|| "-".into())),
        kv("groups with releases", count(c.groups_with_releases)),
        kv("releases", count(c.releases)),
        kv("complete", format!("{} ({:.1}%)", count(c.complete), pct(c.complete as f64, r))),
        kv("obfuscated", format!("{} ({:.1}%)", count(c.obfuscated), pct(c.obfuscated as f64, r))),
        kv("real names found", format!("{} ({:.1}%)", count(c.named), pct(c.named as f64, r))),
        kv("NZBs available", format!("{} ({:.1}%)", count(c.nzb_ready), pct(c.nzb_ready as f64, r))),
        kv("complete NZBs", format!("{} ({:.1}%)", count(c.nzb_complete), pct(c.nzb_complete as f64, r))),
        kv("articles", count(c.total_parts)),
        kv("articles / release", format!("{:.0}", c.total_parts as f64 / r.max(1.0))),
        kv("they represent", human_bytes(c.total_bytes as f64)),
        kv("average release", human_bytes(c.total_bytes as f64 / r.max(1.0))),
    ];
    let mut releases = releases;
    if let Some((name, size, group)) = &c.biggest {
        releases.push(Line::raw(""));
        releases.push(kv("biggest release", human_bytes(*size as f64)));
        releases.push(Line::raw(format!("  {name}")).cyan());
        releases.push(Line::raw(format!("  in {group}")).dim());
    }
    f.render_widget(Paragraph::new(releases).block(panel("releases")), left);

    let mut types: Vec<Line> = c
        .file_types
        .iter()
        .map(|(t, n)| kv(&format!(".{t}"), format!("{} ({:.2}%)", count(*n), pct(*n as f64, r))))
        .collect();
    types.push(Line::raw(""));
    types.push(kv("\"framestor\"", count(c.framestor)));
    types.push(Line::raw(""));
    types.push(note("releases whose posted or real name"));
    types.push(note("contains the word"));
    f.render_widget(Paragraph::new(types).block(panel("names")), mid);

    let rows = c.top_groups.iter().map(|(g, n, bytes)| {
        Row::new(vec![Cell::from(g.clone()).cyan(), Cell::from(count(*n)), Cell::from(human_bytes(*bytes as f64))])
    });
    f.render_widget(
        Table::new(rows, [Constraint::Fill(1), Constraint::Length(12), Constraint::Length(9)])
            .header(Row::new(vec!["group", "releases", "size"]).bold())
            .block(panel("biggest groups")),
        right,
    );
}

fn draw_servers(f: &mut Frame, app: &App, area: Rect) {
    let list = servers(&app.stats);
    let total_headers: u64 = list.iter().map(|s| s["headers"].as_u64().unwrap_or(0)).sum();

    let rows: Vec<Row> = list
        .iter()
        .map(|s| {
            let state = s["state"].as_str().unwrap_or("-").to_string();
            let color = match state.as_str() {
                "ok" => Color::Green,
                "resting" | "article only" => Color::Yellow,
                _ => Color::Red,
            };
            let (wire, text) =
                (s["wire_bytes"].as_u64().unwrap_or(0) as f64, s["text_bytes"].as_u64().unwrap_or(0) as f64);
            let headers = s["headers"].as_u64().unwrap_or(0);
            Row::new(vec![
                Cell::from(s["host"].as_str().unwrap_or("").to_string()).cyan(),
                Cell::from(s["priority"].as_i64().unwrap_or(0).to_string()),
                Cell::from(state).fg(color),
                Cell::from(if s["indexing"].as_bool().unwrap_or(false) { "yes" } else { "no" }),
                Cell::from(format!(
                    "{}/{}/{}",
                    s["in_use"].as_u64().unwrap_or(0),
                    s["limit"].as_u64().unwrap_or(0),
                    s["connections"].as_u64().unwrap_or(0)
                )),
                Cell::from(count(headers as i64)),
                Cell::from(format!("{:.1}%", pct(headers as f64, total_headers as f64))),
                Cell::from(human_bytes(wire)),
                Cell::from(if text > 0.0 { format!("{:.0}%", 100.0 - pct(wire, text)) } else { "-".into() }),
            ])
        })
        .collect();

    let [table, help] = Layout::vertical([Constraint::Min(5), Constraint::Length(6)]).areas(area);
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(note(" start indexing to see per server numbers")).block(panel("servers")),
            table,
        );
    } else {
        f.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Fill(1),
                    Constraint::Length(5),
                    Constraint::Length(15),
                    Constraint::Length(6),
                    Constraint::Length(13),
                    Constraint::Length(15),
                    Constraint::Length(7),
                    Constraint::Length(10),
                    Constraint::Length(9),
                ],
            )
            .header(
                Row::new(vec!["server", "prio", "state", "index", "conns", "headers", "share", "traffic", "saved"])
                    .bold(),
            )
            .block(panel("servers (this run)")),
            table,
        );
    }

    let help_text = vec![
        note("conns: in use / allowed now / configured. allowed drops when a provider refuses more"),
        note("saved: traffic compression saved (XFEATURE COMPRESS GZIP)"),
        note("resting: couldnt connect, retried within a minute. article only: no GROUP support"),
        note("login rejected: fix the username/password in config.json"),
    ];
    f.render_widget(Paragraph::new(help_text).block(panel("key")), help);
}

/// `width` cells of bar, `share` (0..1) of them filled
fn bar_text(share: f64, width: usize) -> String {
    let filled = ((share.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn draw_bottleneck(f: &mut Frame, app: &App, area: Rect) {
    let [verdict, table, help] =
        Layout::vertical([Constraint::Length(5), Constraint::Length(11), Constraint::Min(4)]).areas(area);

    let Some(r) = app.load_rates() else {
        let msg = if app.indexer.is_none() { " start indexing to see what limits it" } else { " measuring..." };
        f.render_widget(Paragraph::new(note(msg)).block(panel("bottleneck")), verdict);
        return;
    };

    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let cpu_cores = app.indexer.as_ref().map(|p| p.cpu as f64 / 100.0).unwrap_or(0.0);
    let (in_use, allowed) = servers(&app.stats)
        .iter()
        .filter(|s| s["indexing"].as_bool().unwrap_or(false) && s["state"] == "ok")
        .fold((0, 0), |(u, a), s| (u + s["in_use"].as_u64().unwrap_or(0), a + s["limit"].as_u64().unwrap_or(0)));

    let (what, why) = diagnose(&r, (in_use, allowed), cpu_cores, cores);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::raw("limited by: ").dim(), Span::raw(what).fg(Color::Yellow).bold()]),
            Line::raw(""),
            Line::raw(why),
        ])
        .wrap(ratatui::widgets::Wrap { trim: true })
        .block(panel(&format!("bottleneck (last {LOAD_WINDOW:.0}s)"))),
        verdict,
    );

    let db_bytes = app.quick.lock().unwrap().db_bytes;
    let share = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.0 };
    let conn_share = share(in_use as f64, allowed as f64);
    let mem_share = share(app.mem_used as f64, app.mem_total as f64);
    let rows: Vec<(&str, Option<f64>, String)> = vec![
        (
            "database writers",
            Some(r.writer_busy),
            format!(
                "{} slices waiting, {:.1} slices per transaction, {:.0}% of the time checkpointing",
                r.queued,
                r.per_batch,
                r.writer_checkpoint * 100.0
            ),
        ),
        (
            "usenet connections",
            Some(conn_share),
            format!("{in_use} of {allowed} busy, {:.1} requests waiting for one", r.waiting),
        ),
        (
            "CPU",
            Some(share(cpu_cores, cores as f64)),
            format!("{cpu_cores:.1} of {cores} cores, {:.1} parsing", r.parse_cores),
        ),
        (
            "memory",
            Some(mem_share),
            format!(
                "{} of {} used, {} of {} headers fetched and not saved yet",
                human_bytes(app.mem_used as f64),
                human_bytes(app.mem_total as f64),
                r.unsaved,
                r.max_unsaved
            ),
        ),
        (
            "provider latency",
            None,
            format!("{:.0}ms per header request, {:.0} requests in flight", r.latency * 1000.0, r.in_flight),
        ),
        ("network", None, format!("{}/s from the servers", human_bytes(app.traffic_rate))),
        (
            "disk",
            None,
            format!("{}/s read, {}/s written by the indexer", human_bytes(r.disk_read), human_bytes(r.disk_write)),
        ),
        (
            "database size",
            None,
            format!(
                "{} on disk, {:.1}x the RAM",
                human_bytes(db_bytes as f64),
                share(db_bytes as f64, app.mem_total as f64)
            ),
        ),
    ];
    let rows = rows.into_iter().map(|(name, used, detail)| {
        let (bar, pct_text, color) = match used {
            Some(u) => {
                let color = if u >= 0.85 {
                    Color::Red
                } else if u >= 0.6 {
                    Color::Yellow
                } else {
                    Color::Green
                };
                (bar_text(u, 20), format!("{:.0}%", u * 100.0), color)
            }
            None => (String::new(), String::new(), Color::DarkGray),
        };
        Row::new(vec![Cell::from(name).cyan(), Cell::from(bar).fg(color), Cell::from(pct_text), Cell::from(detail)])
    });
    f.render_widget(
        Table::new(rows, [Constraint::Length(20), Constraint::Length(21), Constraint::Length(5), Constraint::Fill(1)])
            .header(Row::new(vec!["", "busy", "", ""]).bold())
            .block(panel("resources")),
        table,
    );

    let help_text = vec![
        note(
            "database writers: one thread per shard saves its groups' slices; busy is their average, at 100% the rest waits",
        ),
        note("usenet connections: requests in flight against what the providers allow"),
        note("provider latency: how long a header request takes, from sending it to the last line"),
    ];
    f.render_widget(Paragraph::new(help_text).block(panel("key")), help);
}

// ---------------------------------------------------------------- loop

/// Run until q / esc / ctrl+c.
pub fn run() -> std::io::Result<()> {
    let content = Arc::new(Mutex::new(ContentState::default()));
    if let Ok(cached) = fs::read_to_string(cache_path()) {
        content.lock().unwrap().data = serde_json::from_str(&cached).ok();
    }

    // cheap db numbers on their own thread soo a busy db never stalls the screen
    let quick = Arc::new(Mutex::new(Quick::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let quick_thread = {
        let (quick, stop) = (quick.clone(), stop.clone());
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let q = load_quick();
                *quick.lock().unwrap() = q;
                for _ in 0..50 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })
    };

    let mut app = App {
        page: 0,
        long_graph: false,
        stats: Value::Null,
        status: Value::Null,
        quick,
        content,
        sys: Sys::new(),
        indexer: None,
        me: ProcInfo::default(),
        mem_used: 0,
        mem_total: 0,
        traffic_prev: None,
        traffic_rate: 0.0,
        readings: Default::default(),
    };

    let mut terminal = ratatui::init();
    let mut last_refresh = Instant::now() - Duration::from_secs(10);

    let result = (|| -> std::io::Result<()> {
        loop {
            if last_refresh.elapsed() >= Duration::from_secs(1) {
                app.refresh();
                last_refresh = Instant::now();
            }

            // the content page counts on first visit when nothing is cached
            if app.page == 2 {
                let needs = {
                    let s = app.content.lock().unwrap();
                    s.data.is_none() && !s.running && s.error.is_none()
                };
                if needs {
                    start_content(&app.content);
                }
            }

            terminal.draw(|f| draw(f, &app))?;

            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    _ if ctrl_c => return Ok(()),
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => app.page = (app.page + 1) % PAGES.len(),
                    KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => {
                        app.page = (app.page + PAGES.len() - 1) % PAGES.len()
                    }
                    KeyCode::Char(c @ '1'..='5') => app.page = c as usize - '1' as usize,
                    KeyCode::Char('w') => app.long_graph = !app.long_graph,
                    KeyCode::Char('r') if app.page == 2 => start_content(&app.content),
                    _ => {}
                }
            }
        }
    })();

    ratatui::restore();
    stop.store(true, Ordering::Relaxed);
    let _ = quick_thread.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(count(533_179_307), "533,179,307");
        assert_eq!(count(999), "999");
        assert_eq!(count(-1234), "-1,234");
        assert_eq!(span(30.0), "30s");
        assert_eq!(span(3_700.0), "1h 1m");
        assert_eq!(span(90_000.0), "1d 1h");
        assert_eq!(span(f64::INFINITY), "-");
    }

    #[test]
    fn backfill_math() {
        let row = |key: &str, live, back, first, last| db::GroupProgress {
            key: key.into(),
            live_cursor: live,
            backfill_cursor: back,
            first: Some(first),
            last: Some(last),
            ..Default::default()
        };
        // dates in 2026
        let day = 86_400;
        let base = 1_780_000_000;
        let quick = Quick {
            progress: vec![
                // 1000 numbers, 401..1000 indexed: 600 done, 400 left. 600 articles
                // posted over 2 days = 300 a day
                db::GroupProgress {
                    low: Some((400, base + 10 * day)),
                    high: Some((1000, base + 12 * day)),
                    ..row("alt.binaries.a@news.x", 1000, 400, 1, 1000)
                },
                // the same group on another server got less far: not counted again
                row("alt.binaries.a@news.y", 5000, 4900, 1, 5000),
                // done, and indexed posts only minutes apart: no rate from it
                db::GroupProgress {
                    low: Some((1, base + 10 * day)),
                    high: Some((500, base + 10 * day + 60)),
                    ..row("alt.binaries.b", 500, 0, 1, 500)
                },
                // nearly done
                row("alt.binaries.c", 100, 1, 1, 100),
                // no range yet
                db::GroupProgress {
                    key: "alt.binaries.d".into(),
                    live_cursor: 5,
                    backfill_cursor: 5,
                    ..Default::default()
                },
                // empty group (first past last)
                row("alt.binaries.e", 9, 9, 10, 9),
            ],
            ..Quick::default()
        };

        let b = backfill_at(&quick, base + 13 * day);
        assert_eq!((b.total, b.covered, b.remaining), (1600, 1199, 401));
        assert_eq!((b.measured, b.unmeasured, b.done, b.dated), (4, 1, 2, 1));
        assert_eq!(b.behind[0], ("alt.binaries.a".into(), 400, 60.0));
        assert_eq!(b.nearly.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["alt.binaries.c", "alt.binaries.a"]);

        // 300 a day for 1000 numbers, scaled up to all 1600
        let per_day = b.per_day.unwrap();
        assert!((per_day - 480.0).abs() < 1e-9, "{per_day}");
        assert_eq!(b.reached, Some(base + 10 * day));
    }

    #[test]
    fn split_groups_progress_follows_their_chunks_then_their_sweep() {
        let row = |key: &str| db::GroupProgress {
            key: key.into(),
            live_cursor: 1000,
            backfill_cursor: 1000, // the cursor stands still on a split group
            first: Some(1),
            last: Some(1000),
            ..Default::default()
        };
        let mut quick = Quick {
            progress: vec![row("alt.binaries.s"), row("alt.binaries.t"), row("alt.binaries.u")],
            ..Quick::default()
        };
        let b = backfill_at(&quick, 1_780_000_000);
        assert_eq!((b.total, b.covered, b.remaining, b.done), (3000, 0, 3000, 0), "no chunks: the cursors");

        // s: 1 of 4 days done, t: all 6 (its sweep from the cursor to go), u is not split
        quick.group_chunks = BTreeMap::from([("alt.binaries.s".into(), (1, 4)), ("alt.binaries.t".into(), (6, 6))]);
        let b = backfill_at(&quick, 1_780_000_000);
        assert_eq!((b.total, b.covered, b.remaining, b.done), (3000, 250, 2750, 0));
        assert!(b.behind.contains(&("alt.binaries.t".into(), 1000, 0.0)));

        // t's sweep halfway down
        quick.progress[1].backfill_cursor = 500;
        let b = backfill_at(&quick, 1_780_000_000);
        assert_eq!((b.covered, b.remaining), (250 + 500, 2250));
        assert_eq!(b.behind[0], ("alt.binaries.u".into(), 1000, 0.0));
        assert!(b.behind.contains(&("alt.binaries.s".into(), 750, 25.0)));
    }

    #[test]
    fn group_chunks_are_loaded_per_group() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        assert!(load_group_chunks(&conn).is_empty(), "no table: nothing");
        crate::chunks::create(&conn).unwrap();
        crate::chunks::add(&conn, "g1", 100, 98).unwrap();
        crate::chunks::add(&conn, "g2", 200, 199).unwrap();
        let claim = crate::chunks::claim(&conn, &[("g2".to_string(), i64::MIN)], "s", 1000).unwrap().unwrap();
        assert!(crate::chunks::finish(&conn, &claim, 1100).unwrap());
        assert_eq!(load_group_chunks(&conn), BTreeMap::from([("g1".into(), (0, 3)), ("g2".into(), (1, 2))]));
    }

    #[test]
    fn implausible_dates_give_no_rate() {
        let (day, now) = (86_400, 1_780_000_000);
        let row = |low: db::Dated, high: db::Dated| db::GroupProgress {
            key: "g".into(),
            live_cursor: 1_000_000,
            backfill_cursor: 0,
            first: Some(1),
            last: Some(1_000_000),
            low: Some(low),
            high: Some(high),
        };
        let rate = |r: &db::GroupProgress| posting_rate(r, 1_000_000, now);

        // 1000 articles a day: a million is 1000 days of history, fine
        assert_eq!(rate(&row((0, now - 10 * day), (10_000, now))), Some(1000.0));
        // forged 1970 dates
        assert_eq!(rate(&row((0, 0), (10_000, now))), None);
        // posted next year
        assert_eq!(rate(&row((0, now), (10_000, now + 365 * day))), None);
        // 10 articles a day would put the first article 270 years back
        assert_eq!(rate(&row((0, now - 1000 * day), (10_000, now))), None);
        // newer articles dated earlier
        assert_eq!(rate(&row((0, now), (10_000, now - day))), None);
    }

    #[test]
    fn load_rates_and_diagnosis() {
        let reading = |at: f64,
                       busy_s: f64,
                       slices: u64,
                       batches: u64,
                       xover_s: f64,
                       xovers: u64,
                       wait_s: f64,
                       disk: u64| {
            Reading {
                load: serde_json::json!({
                    "at": at, "writer_busy_ns": (busy_s * 1e9) as u64, "writer_slices": slices, "writer_batches": batches,
                    "writer_queued": 7, "xover_ns": (xover_s * 1e9) as u64, "xovers": xovers,
                    "unsaved_headers": 120_000, "max_unsaved_headers": 500_000,
                    "lease_wait_ns": (wait_s * 1e9) as u64, "parse_ns": 0
                }),
                disk: Some((at, disk, 0)),
            }
        };
        // over 10s: writer busy 9.5s, 40 slices in 5 transactions, 200 requests
        // taking 400s in total (40 in flight), 1MB/s read
        let r = load_rates(
            &reading(100.0, 0.0, 0, 0, 0.0, 0, 0.0, 0),
            &reading(110.0, 9.5, 40, 5, 400.0, 200, 0.0, 10_000_000),
        )
        .unwrap();
        assert!((r.writer_busy - 0.95).abs() < 1e-9);
        assert_eq!((r.queued, r.per_batch), (7, 8.0));
        assert_eq!((r.unsaved, r.max_unsaved), (120_000, 500_000));
        assert!((r.latency - 2.0).abs() < 1e-9 && (r.in_flight - 40.0).abs() < 1e-9);
        assert!((r.disk_read - 1e6).abs() < 1e-3);

        let cpu_idle = 1.0;
        assert_eq!(diagnose(&r, (40, 400), cpu_idle, 10).0, "the database writers");

        let quiet_writer = LoadRates { writer_busy: 0.3, ..r.clone() };
        assert_eq!(diagnose(&quiet_writer, (395, 400), cpu_idle, 10).0, "usenet connections");
        assert_eq!(
            diagnose(&LoadRates { waiting: 3.0, ..quiet_writer.clone() }, (100, 400), cpu_idle, 10).0,
            "usenet connections"
        );
        assert_eq!(diagnose(&quiet_writer, (100, 400), 9.5, 10).0, "CPU");
        assert_eq!(diagnose(&quiet_writer, (100, 400), cpu_idle, 10).0, "nothing is maxed out");

        // no time between readings: no rates
        assert_eq!(
            load_rates(&reading(5.0, 0.0, 0, 0, 0.0, 0, 0.0, 0), &reading(5.0, 1.0, 0, 0, 0.0, 0, 0.0, 0)),
            None
        );
    }

    #[test]
    fn no_rate_without_dates() {
        let quick = Quick {
            progress: vec![db::GroupProgress {
                key: "g".into(),
                live_cursor: 100,
                backfill_cursor: 50,
                first: Some(1),
                last: Some(100),
                ..Default::default()
            }],
            ..Quick::default()
        };
        let b = backfill(&quick);
        assert_eq!((b.per_day, b.dated, b.covered), (None, 0, 50));
    }

    #[test]
    fn old_cache_still_loads() {
        // stats_cache.json written before the nzb counts existed
        let c: Content = serde_json::from_str(
            r#"{"computed_at":1.0,"took_secs":2.0,"releases":10,"complete":5,"obfuscated":0,"named":0,
                "total_bytes":0,"total_parts":0,"groups_with_releases":1,"top_groups":[],"biggest":null,
                "file_types":[],"framestor":0}"#,
        )
        .unwrap();
        assert_eq!((c.releases, c.nzb_ready, c.nzb_complete), (10, 0, 0));
    }

    #[test]
    fn pages_render() {
        let backend = ratatui::backend::TestBackend::new(160, 50);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        let mut app = App {
            page: 0,
            long_graph: false,
            stats: serde_json::json!({
                "mode": "backfill", "uptime": 3600, "total_articles": 1_000_000, "rate_bin": 10,
                "rate": [[now() as i64 / 10 * 10 - 10, 50_000, 0]],
                "servers": [{"host": "news.x", "priority": 1, "state": "ok", "indexing": true, "in_use": 3,
                             "limit": 10, "connections": 10, "headers": 900, "wire_bytes": 100, "text_bytes": 400}]
            }),
            status: serde_json::json!({"running": false}),
            quick: Arc::new(Mutex::new(Quick { chunks: (1, 0, 3, 10, 0), ..Quick::default() })),
            content: Arc::new(Mutex::new(ContentState {
                data: Some(Content {
                    releases: 10,
                    complete: 5,
                    nzb_ready: 9,
                    nzb_complete: 5,
                    file_types: vec![("mkv".into(), 3)],
                    ..Content::default()
                }),
                ..ContentState::default()
            })),
            sys: Sys::new(),
            indexer: None,
            me: ProcInfo::default(),
            mem_used: 0,
            mem_total: 0,
            traffic_prev: None,
            traffic_rate: 0.0,
            readings: Default::default(),
        };

        for (page, name) in PAGES.iter().enumerate() {
            app.page = page;
            term.draw(|f| draw(f, &app)).unwrap();
            let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
            assert!(text.contains(name), "page {page}");
            match page {
                0 => assert!(text.contains("headers/s")),
                1 => assert!(text.contains("day chunks 3 of 10 done")),
                2 => assert!(
                    text.contains(".mkv")
                        && text.contains("NZBs available    9 (90.0%)")
                        && text.contains("complete NZBs     5 (50.0%)")
                ),
                3 => assert!(text.contains("news.x") && text.contains("75%")),
                _ => {}
            }
        }
    }

    #[test]
    fn chunks_stats_aggregation() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        crate::chunks::create(&conn).unwrap();
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);

        let claim = |group: &str, at: i64| {
            crate::chunks::claim(&conn, &[(group.to_string(), i64::MIN)], "s", at).unwrap().unwrap()
        };

        // g1: 3 chunks, all finished two hours ago
        crate::chunks::add(&conn, "g1", 100, 98).unwrap();
        for _ in 0..3 {
            assert!(crate::chunks::finish(&conn, &claim("g1", now - 7300), now - 7200).unwrap());
        }
        // g2: 1 chunk claimed two hours ago and finished just now, 1 claimed just now
        // (2 chunks total, 1 done, 1 in progress)
        crate::chunks::add(&conn, "g2", 200, 199).unwrap();
        assert!(crate::chunks::finish(&conn, &claim("g2", now - 7200), now).unwrap());
        claim("g2", now);

        // 2 groups, 1 in progress (g2 has pending), 4 done total, 5 total, 1 done in last hour
        let (groups, splitting, done, total, done_last_hour) = load_chunks_stats(&conn);
        assert_eq!(groups, 2);
        assert_eq!(splitting, 1); // 1 group with pending > 0 (g2)
        assert_eq!(done, 4); // g1 has 3 done, g2 has 1 done
        assert_eq!(total, 5); // 3 chunks for g1, 2 chunks for g2
        assert_eq!(done_last_hour, 1, "finished in the last hour, though claimed before it");
    }
}
