use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{Config, save_config};
use crate::nntp::{BlockingPool, NntpError};
use crate::ui::{self, Column, Line, Table, prompt};

const PAGE_SIZE: usize = 30;
const CACHE_TTL: Duration = Duration::from_secs(600);

type CacheKey = (String, Option<String>);
type GroupsCache = HashMap<CacheKey, (Instant, Vec<String>)>;

/// group lists per (host, pattern), refetched every 10 mins soo new groups show up
static GROUPS_CACHE: Mutex<Option<GroupsCache>> = Mutex::new(None);
/// group -> is it empty
static EMPTY: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

fn page_nonempty(client: &BlockingPool, page_groups: &[String]) -> Vec<String> {
    let mut guard = EMPTY.lock().unwrap();
    let empty = guard.get_or_insert_with(HashMap::new);
    let mut kept = Vec::new();

    for g in page_groups {
        let is_empty = *empty.entry(g.clone()).or_insert_with(|| match client.select_group(g) {
            Ok((_, first, last, _)) => last <= first,
            Err(_) => true,
        });

        if !is_empty {
            kept.push(g.clone());
        }
    }

    kept
}

fn load_groups(client: &BlockingPool, host: &str, pattern: Option<&str>) -> Result<Vec<String>, NntpError> {
    let key = (host.to_string(), pattern.map(String::from));

    if let Some((at, groups)) = GROUPS_CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key)
        && at.elapsed() < CACHE_TTL
    {
        return Ok(groups.clone());
    }

    let groups: Vec<String> = match client.list_groups(pattern) {
        Ok(g) => g.into_iter().map(|(name, _)| name).collect(),
        // server doesnt support wildcards soo load everything and filter client side
        Err(_) if pattern.is_some() => return load_groups(client, host, None),
        Err(e) => return Err(e),
    };

    GROUPS_CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, (Instant::now(), groups.clone()));
    Ok(groups)
}

fn results_page(client: &BlockingPool, groups: &[String]) -> Option<String> {
    let total_pages = groups.len().div_ceil(PAGE_SIZE).max(1);
    let mut page = 0;

    loop {
        ui::clear();

        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(groups.len());

        // check the groups on this page arent empty
        let page_groups = page_nonempty(client, &groups[start..end]);

        ui::print_panel(
            &format!(
                "[bold]Results[/bold]\nPage {} of {total_pages}\n[dim]Showing {}-{end} of {} matches[/dim]",
                page + 1,
                start + 1,
                groups.len()
            ),
            "green",
        );

        let mut table = Table::new(vec![Column::new("#").width(4).right(), Column::new("Group").flex()]);
        for (i, g) in page_groups.iter().enumerate() {
            table.add_row(vec![Line::plain((i + 1).to_string()), Line::plain(g.clone())]);
        }
        table.print();

        ui::print("\n[dim]0. Back[/dim]");
        if page > 0 {
            ui::print("[cyan]p.[/cyan] Previous Page");
        }
        if end < groups.len() {
            ui::print("[cyan]n.[/cyan] Next Page");
        }
        if total_pages > 1 {
            ui::print("[cyan]g.[/cyan] Go to Page");
        }

        let choice = prompt("\nChoice: ").trim().to_string();

        match choice.as_str() {
            "0" => return None,
            "p" => {
                if page > 0 {
                    page -= 1;
                } else {
                    ui::dim("already on the first page");
                    ui::pause();
                }
                continue;
            }
            "n" => {
                if end < groups.len() {
                    page += 1;
                } else {
                    ui::dim("already on the last page");
                    ui::pause();
                }
                continue;
            }
            "g" => {
                let target = prompt(&format!("Go to page (1-{total_pages}): ")).trim().parse::<usize>().unwrap_or(0);
                if (1..=total_pages).contains(&target) {
                    page = target - 1;
                } else {
                    ui::error(&format!("page must be between 1 and {total_pages}"));
                    ui::pause();
                }
                continue;
            }
            _ => {}
        }

        match choice.parse::<usize>() {
            Ok(n) if (1..=page_groups.len()).contains(&n) => return Some(page_groups[n - 1].clone()),
            _ => {
                ui::error("invalid");
                ui::pause();
            }
        }
    }
}

pub fn groups_menu(config: &mut Config) {
    if config.servers.is_empty() {
        ui::error("no server configured, setup in Settings first");
        ui::pause();
        return;
    }

    let client = BlockingPool::from_config(config);

    // always connect before talking to the server, cache or not
    if let Err(e) = client.connect() {
        ui::error(&format!("couldnt connect to server: {e}"));
        ui::pause();
        return;
    }

    loop {
        ui::clear();
        ui::print_panel("[bold cyan]Groups[/bold cyan]", "cyan");
        ui::print("[dim]Search for newsgroups to add.[/dim]\n");
        ui::print("[dim]0. Back[/dim]\n");

        let query = prompt("Search: ").trim().to_string();
        if query.is_empty() || query == "0" {
            break;
        }

        if query.chars().count() < 3 {
            ui::warn("Search at least 3 characters bruh\n");
            ui::pause();
            continue;
        }

        // server side wildcard pattern for fast filtering
        let pattern = format!("*{query}*");

        let groups = match load_groups(&client, &config.servers_label(), Some(&pattern)) {
            Ok(g) => g,
            Err(_) => {
                ui::error("couldnt fetch groups from the server");
                ui::pause();
                continue;
            }
        };

        // client side filtering in case the server ignored the wildcard
        let q = query.to_lowercase();
        let groups: Vec<String> = groups
            .into_iter()
            .filter(|g| {
                let g = g.to_lowercase();
                g.contains(&q) && g.contains(".binaries.")
            })
            .collect();

        if groups.is_empty() {
            ui::error("No matching groups found\n");
            ui::pause();
            continue;
        }

        let Some(chosen) = results_page(&client, &groups) else { continue };

        // add the group, emptiness was already checked before display
        config.group = chosen.clone();
        if !config.groups.contains(&chosen) {
            config.groups.push(chosen);
        }

        // save the pick soo it sticks after restart
        if let Err(e) = save_config(config) {
            ui::error(&format!("couldnt save config: {e}"));
            ui::pause();
        }

        break;
    }

    client.disconnect();
}
