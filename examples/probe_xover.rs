//! How header request size changes speed on one connection: times XOVER for
//! ranges of different sizes on each usenet server in config.json, plus the
//! round trip of a DATE (what pipelining could hide at most).
//!
//!     ATLAS_HOME=. cargo run --release --example probe_xover -- --group alt.binaries.boneless \
//!         [--sizes 1000,10000,50000] [--only host] [--back articles] [--rounds n]
//!
//! Read only (GROUP, XOVER, DATE). Prints no passwords.

use std::time::{Duration, Instant};

use atlas::config::load_config;
use atlas::nntp::Conn;

fn arg(name: &str) -> Option<String> {
    std::env::args().skip_while(|a| a != name).nth(1)
}

fn main() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(run());
}

async fn run() {
    let group = arg("--group").unwrap_or_else(|| "alt.binaries.boneless".into());
    let sizes: Vec<u64> =
        arg("--sizes").unwrap_or_else(|| "1000,10000,50000".into()).split(',').filter_map(|s| s.parse().ok()).collect();
    let only = arg("--only");
    // start this many articles back from the newest (backfill reads old ones)
    let back: u64 = arg("--back").and_then(|v| v.parse().ok()).unwrap_or(0);
    let rounds: usize = arg("--rounds").and_then(|v| v.parse().ok()).unwrap_or(2);

    let cfg = load_config().expect("no config.json (set ATLAS_HOME)");
    for server in cfg.servers.iter().filter(|s| s.indexes()) {
        if only.as_ref().is_some_and(|o| !server.host.contains(o.as_str())) {
            continue;
        }
        let mut conn = match Conn::open(server, Duration::from_secs(60), server.compress.unwrap_or(true)).await {
            Ok(c) => c,
            Err(e) => {
                println!("{:32} couldnt connect: {e}", server.host);
                continue;
            }
        };
        let Ok((_, _, last, _)) = conn.select_group(&group).await else {
            println!("{:32} no {group}", server.host);
            continue;
        };

        let mut rtts = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let _ = conn.command("DATE", None).await;
            rtts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        rtts.sort_by(|a, b| a.total_cmp(b));
        println!("{:32} DATE round trip {:.0}ms (median of 5), compressed {}", server.host, rtts[2], conn.compressed);

        // each size on its own range, going back from the newest articles
        let mut end = last.saturating_sub(back);
        for &size in &sizes {
            for round in 0..rounds {
                let start = end.saturating_sub(size - 1);
                let t = Instant::now();
                match conn.xover(start, end).await {
                    Ok(rows) => {
                        let secs = t.elapsed().as_secs_f64();
                        println!(
                            "{:32}   {size:>7} articles (round {round}): {:>6.0}ms, {:>6} rows, {:>8.0} headers/s",
                            "",
                            secs * 1000.0,
                            rows.len(),
                            rows.len() as f64 / secs
                        );
                    }
                    Err(e) => println!("{:32}   {size:>7} articles: {e}", ""),
                }
                end = start.saturating_sub(1);
            }
        }
        conn.quit().await;
    }
}
