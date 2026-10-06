//! Find how many simultaneous connections each usenet server in config.json
//! accepts: keep opening (and logging in) connections, holding them all open,
//! until the server refuses one or `--max` is reached.
//!
//! Servers that share an account are checked against each other too: with one
//! server's connections all held, can the other still open any?
//!
//!     ATLAS_HOME=. cargo run --release --example probe_connections -- --max 150 [--only host]
//!
//! Prints a JSON report, never the passwords.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use atlas::config::{UsenetServer, load_config};
use atlas::nntp::{Conn, NntpError};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(20);
/// connections opened at a time while probing
const STEP: usize = 10;

/// provider + account, e.g. ("newsgroupdirect.com", "nyy...")
fn account_of(s: &UsenetServer) -> (String, String) {
    let labels: Vec<&str> = s.host.trim().trim_end_matches('.').split('.').collect();
    let domain = labels[labels.len().saturating_sub(2)..].join(".").to_lowercase();
    let user = s.username.split('@').next().unwrap_or("").to_lowercase();
    (domain, user)
}

/// Open connections until refused or `max`. Returns the open connections and
/// why it stopped.
async fn fill(server: &UsenetServer, max: usize) -> (Vec<Conn>, String) {
    let mut open = Vec::new();

    while open.len() < max {
        let batch = STEP.min(max - open.len());
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..batch {
            let s = server.clone();
            tasks.spawn(async move { Conn::open(&s, TIMEOUT, false).await });
        }

        let mut stop = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(c)) => open.push(c),
                Ok(Err(e)) => {
                    stop.get_or_insert(describe(&e));
                }
                Err(e) => {
                    stop.get_or_insert(format!("task failed: {e}"));
                }
            }
        }

        if let Some(why) = stop {
            return (open, why);
        }
    }

    (open, format!("reached --max {max}"))
}

fn describe(e: &NntpError) -> String {
    match e {
        NntpError::Reply { code, message } => format!("refused: {code} {message}"),
        other => format!("refused: {other}"),
    }
}

async fn close(conns: Vec<Conn>) {
    for mut c in conns {
        c.quit().await;
    }
}

fn main() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(run());
}

async fn run() {
    let max: usize = std::env::args().skip_while(|a| a != "--max").nth(1).and_then(|v| v.parse().ok()).unwrap_or(150);

    let cfg = load_config().expect("no config.json (set ATLAS_HOME)");
    let only: Option<String> = std::env::args().skip_while(|a| a != "--only").nth(1);
    let servers: Vec<UsenetServer> = cfg
        .servers
        .into_iter()
        .filter(|s| !s.password.is_empty())
        .filter(|s| only.as_ref().is_none_or(|o| s.host.eq_ignore_ascii_case(o)))
        .collect();

    let mut report = BTreeMap::new();

    // each server on its own
    for s in &servers {
        let started = Instant::now();
        let (conns, why) = fill(s, max).await;
        let n = conns.len();
        eprintln!("{:32} max {:3}  ({why}, {:.1}s)", s.host, n, started.elapsed().as_secs_f64());
        report.insert(s.host.clone(), json!({"max": n, "stopped": why, "configured": s.connections()}));
        close(conns).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // servers on the same account: hold one full, try the others
    let mut by_account: BTreeMap<(String, String), Vec<&UsenetServer>> = BTreeMap::new();
    for s in &servers {
        by_account.entry(account_of(s)).or_default().push(s);
    }

    let mut shared = Vec::new();
    for ((domain, _), group) in by_account.iter().filter(|(_, g)| g.len() > 1) {
        let first = group[0];
        let (held, _) = fill(first, max).await;
        for other in &group[1..] {
            let extra = Conn::open(other, TIMEOUT, false).await;
            let refused = extra.is_err();
            eprintln!(
                "{domain}: holding {} on {}, {} {}",
                held.len(),
                first.host,
                other.host,
                if refused { "REFUSED (limit shared)" } else { "still accepts (own limit)" }
            );
            shared
                .push(json!({"held_on": first.host, "held": held.len(), "other": other.host, "shared_limit": refused}));
            if let Ok(mut c) = extra {
                c.quit().await;
            }
        }
        close(held).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    println!("{}", serde_json::to_string_pretty(&json!({"servers": report, "shared": shared})).unwrap());
}
