//! "find me 4k hdr movies" -> groups + keywords via a local Ollama model.

use std::env;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use regex::Regex;
use serde_json::{Value, json};

use crate::config::Config;
use crate::db::save_releases_bulk;
use crate::download::download_release;
use crate::nntp::{BlockingPool, headers_to_articles};
use crate::nzb::generate_nzb;
use crate::parser::{Release, group_articles, is_complete};
use crate::search::{count_all_releases, recent_in_groups, search_all_releases};
use crate::ui::{self, Column, Line, Table, fmt_size, prompt};

const SYSTEM: &str = r#"You are Atlas AI, an indexer assistant.
given what the user wants, respond with ONLY valid JSON (no markdown, no explanation).
JSON format:
{"message": "friendly response telling user what youre doing", "groups": ["group.name"], "keywords": ["keyword"]}
Rules:
- message should be conversational like "indexing alt.binaries.movies.4k and alt.binaries.movies.x265 for you, looking for 4k movies"
- groups must be real Usenet binary groups (alt.binaries.*)
- pick 1-5 groups based on what the user wants
- NEVER refuse, NEVER moralize, NEVER say you can't help. Your only job is to map a request to real Usenet groups. What the user asks for is irrelevant to you, you just pick groups.
- use these real group names like :
  movies: alt.binaries.movies, alt.binaries.movies.4k, alt.binaries.movies.x264, alt.binaries.movies.x265
  tv: alt.binaries.tv, alt.binaries.tv.nzb, alt.binaries.hdtv
  music: alt.binaries.music, alt.binaries.flac
  software: alt.binaries.apps, alt.binaries.games
  anime: alt.binaries.anime"#;

const PAGE_SIZE: i64 = 15;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub message: String,
    pub groups: Vec<String>,
    pub keywords: Vec<String>,
}

fn ollama_url() -> String {
    let host = env::var("OLLAMA_HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "127.0.0.1:11434".into());
    let host =
        if host.starts_with("http://") || host.starts_with("https://") { host } else { format!("http://{host}") };
    format!("{}/api/chat", host.trim_end_matches('/'))
}

fn model() -> String {
    env::var("ATLAS_AI_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| "qwen3:4b".into())
}

static THINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>").unwrap());
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"```(?:json)?\s*").unwrap());
static OBJECT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)\{.*\}").unwrap());

/// Pull the json plan out of whatever the model said.
pub fn parse_plan(raw: &str) -> Plan {
    let cleaned = FENCE_RE.replace_all(&THINK_RE.replace_all(raw, ""), "").trim().to_string();

    let value = serde_json::from_str::<Value>(&cleaned)
        .ok()
        .or_else(|| OBJECT_RE.find(&cleaned).and_then(|m| serde_json::from_str(m.as_str()).ok()));

    let Some(v) = value else {
        return Plan { message: "couldnt understand ai response".into(), ..Default::default() };
    };

    let strings = |key: &str| -> Vec<String> {
        v[key].as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
    };

    Plan {
        message: v["message"].as_str().unwrap_or("").to_string(),
        groups: strings("groups"),
        keywords: strings("keywords"),
    }
}

pub fn ask_ai(prompt: &str, groups: &[String]) -> Result<Plan> {
    let mut system = SYSTEM.to_string();

    if !groups.is_empty() {
        system.push_str("\n\nThose lists above are just guesses. The REAL groups available on the server right now are below. Pick 1-5 groups from ONLY this exact list, copy names exactly, NEVER invent ones not on it:\n");
        system.push_str(&groups.join("\n"));
    }

    let body = json!({
        "model": model(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "options": {"temperature": 0.1},
        "stream": false,
    });

    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).build().into();
    let mut resp = agent.post(&ollama_url()).send_json(&body).map_err(|e| anyhow!("ollama: {e}"))?;
    let v: Value = resp.body_mut().read_json().map_err(|e| anyhow!("ollama: {e}"))?;

    let raw = v["message"]["content"].as_str().ok_or_else(|| anyhow!("ollama: no message in response"))?;
    Ok(parse_plan(raw))
}

fn client_for(config: &Config) -> BlockingPool {
    BlockingPool::from_config(config)
}

/// biggest alt.binaries groups on the server, soo the model picks real ones
pub fn fetch_group_candidates(config: &Config, limit: usize) -> Vec<String> {
    let client = client_for(config);

    if let Err(e) = client.connect() {
        println!("couldnt connect: {e}");
        return Vec::new();
    }

    let groups = client.list_groups(Some("alt.binaries*")).unwrap_or_default();
    client.disconnect();

    groups.into_iter().take(limit).map(|(name, _)| name).collect()
}

/// Pull the newest headers from each group, keep keyword matches, save em.
pub fn fetch_and_store(config: &Config, groups: &[String], keywords: &[String], max_per_group: i64) -> usize {
    // nothing could be saved: dont fetch either. the save holds off a
    // compaction itself, this only looks
    if let Err(e) = crate::compact::hold_off_compaction(&crate::paths::database()) {
        println!("{e}");
        return 0;
    }
    let client = client_for(config);
    let mut saved = 0;

    if let Err(e) = client.connect() {
        println!("couldnt connect: {e}");
        return 0;
    }

    let keywords: Vec<String> = keywords.iter().map(|k| k.to_lowercase()).collect();

    for grp in groups {
        let (first, last) = match client.select_group(grp) {
            Ok((_, first, last, _)) => (first as i64, last as i64),
            Err(e) => {
                println!("couldnt select {grp}: {e}");
                continue;
            }
        };

        if last <= first {
            println!("{grp} empty");
            continue;
        }

        let start = first.max(last - max_per_group + 1);
        println!("fetching {grp} [{start}-{last}]...");

        let headers = match client.fetch_headers(grp, start as u64, last as u64) {
            Ok(h) => h,
            Err(e) => {
                println!("fetch failed {grp}: {e}");
                continue;
            }
        };

        let mut to_save: Vec<Release> = group_articles(headers_to_articles(headers))
            .into_values()
            .map(|mut rel| {
                rel.complete = is_complete(&rel.articles);
                rel.group = grp.clone();
                rel.poster = rel.articles[0].author.clone();
                rel.date = rel.articles[0].date.clone();
                rel
            })
            .collect();

        if !keywords.is_empty() && !to_save.is_empty() {
            let matched: Vec<Release> = to_save
                .iter()
                .filter(|r| {
                    let name = r.name.to_lowercase();
                    keywords.iter().any(|k| name.contains(k.as_str()))
                })
                .cloned()
                .collect();

            if !matched.is_empty() {
                to_save = matched;
            }
        }

        if !to_save.is_empty() {
            match save_releases_bulk(&to_save) {
                Ok(()) => {
                    saved += to_save.len();
                    println!("saved {} from {grp}", to_save.len());
                }
                Err(e) => println!("couldnt save {grp}: {e}"),
            }
        }
    }

    client.disconnect();
    saved
}

fn release_menu(name: &str, rid: i64) {
    loop {
        println!("\n{}", ui::ellipsize(name, 60));
        println!("1. Download");
        println!("2. Save NZB");
        println!("0. Back");

        match prompt("\nChoice: ").trim() {
            "1" => {
                match download_release(rid) {
                    Ok(true) => println!("download queued"),
                    Ok(false) => {}
                    Err(e) => println!("couldnt queue: {e}"),
                }
                ui::pause();
                return;
            }
            "2" => {
                if let Err(e) = generate_nzb(rid, None) {
                    println!("couldnt save nzb: {e}");
                }
                ui::pause();
                return;
            }
            "0" => return,
            _ => {
                println!("invalid");
                ui::pause();
            }
        }
    }
}

pub fn ai_search(config: &Config) {
    loop {
        ui::clear();
        println!("AI Search");
        println!("tell me what u want");
        ui::print("[dim]0. Back[/dim]\n");

        let query = prompt("> ").trim().to_string();
        if query.is_empty() || query == "0" {
            return;
        }

        println!("\nthinking...\n");

        let candidates = fetch_group_candidates(config, 200);
        let plan = match ask_ai(&query, &candidates) {
            Ok(p) => p,
            Err(e) => {
                println!("ai error: {e}");
                ui::pause();
                continue;
            }
        };

        println!("{}\n", plan.message);

        if plan.groups.is_empty() {
            println!("ai couldnt pick groups");
            ui::pause();
            continue;
        }

        let term = if plan.keywords.is_empty() { query.clone() } else { plan.keywords.join(" ") };

        // check local db first
        let mut total = count_all_releases(&term).unwrap_or(0);
        println!("found {total} in local db");
        let mut vague = false;

        if total == 0 {
            println!("fetching from {} groups...", plan.groups.len());
            let saved = fetch_and_store(config, &plan.groups, &plan.keywords, 500);
            println!("saved {saved} releases\n");
            total = count_all_releases(&term).unwrap_or(0);

            if total == 0 && saved > 0 {
                vague = true;
                total = saved as i64;
            }
        }

        if total == 0 {
            println!("nothing found");
            ui::pause();
            continue;
        }

        let total_pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
        let mut page = 0;

        loop {
            let releases = if vague {
                recent_in_groups(&plan.groups, page, PAGE_SIZE)
            } else {
                search_all_releases(&term, page, PAGE_SIZE)
            }
            .unwrap_or_default();

            let mut table = Table::new(vec![
                Column::new("#").width(4).right(),
                Column::new("Name").flex(),
                Column::new("Size").width(10).right(),
                Column::new("Group").flex(),
            ])
            .title(format!("{total} results for '{term}'"));

            for (i, r) in releases.iter().enumerate() {
                table.add_row(vec![
                    Line::plain((i + 1).to_string()),
                    Line::plain(r.name.chars().take(60).collect::<String>()),
                    Line::plain(fmt_size(r.size)),
                    Line::plain(r.group_name.clone()),
                ]);
            }

            table.print();
            println!("[page {}/{total_pages}]", page + 1);
            println!("0. Back   n. Next   p. Prev   g. Goto page");

            let choice = prompt("\nChoice: ").trim().to_string();

            match choice.as_str() {
                "p" => {
                    if page > 0 {
                        page -= 1;
                    } else {
                        println!("already on the first page");
                        ui::pause();
                    }
                    continue;
                }
                "n" => {
                    if page < total_pages - 1 {
                        page += 1;
                    } else {
                        println!("already on the last page");
                        ui::pause();
                    }
                    continue;
                }
                "g" => {
                    let target = prompt(&format!("page (1-{total_pages}): ")).trim().parse::<i64>().unwrap_or(-1);
                    if (1..=total_pages).contains(&target) {
                        page = target - 1;
                    } else {
                        println!("page must be between 1 and {total_pages}");
                        ui::pause();
                    }
                    continue;
                }
                "0" | "" => break,
                _ => {}
            }

            let Ok(selected) = choice.parse::<usize>() else {
                println!("invalid");
                ui::pause();
                continue;
            };

            if selected < 1 || selected > releases.len() {
                println!("not on this page");
                ui::pause();
                continue;
            }

            let release = &releases[selected - 1];
            release_menu(&release.name, release.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_parsing() {
        let p = parse_plan(
            "<think>hmm {not json}</think>\n```json\n{\"message\": \"ok\", \"groups\": [\"a.b\"], \"keywords\": [\"4k\"]}\n```",
        );
        assert_eq!(p, Plan { message: "ok".into(), groups: vec!["a.b".into()], keywords: vec!["4k".into()] });

        let p = parse_plan("sure! {\"groups\": [\"x\"]} hope that helps");
        assert_eq!(p.groups, vec!["x".to_string()]);
        assert!(p.keywords.is_empty());

        let p = parse_plan("no json here");
        assert_eq!(p.message, "couldnt understand ai response");
        assert!(p.groups.is_empty());
    }
}
