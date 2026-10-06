//! The interactive menus.

use std::fs;

use crate::ai::ai_search;
use crate::api;
use crate::config::{Config, UsenetServer, load_config, save_config};
use crate::dashboard;
use crate::db::{create_db, purge_broken};
use crate::download::download_release;
use crate::groups_menu::groups_menu;
use crate::nzb::generate_nzb;
use crate::paths;
use crate::procs::{get_status, indexer_alive, start_background_indexer, stop_background_indexer};
use crate::search::{
    ArticleRow, ReleaseRow, count_all_releases, count_obfuscated, count_releases, get_articles, search_all_releases,
    search_obfuscated, search_releases,
};
use crate::stats_dashboard;
use crate::ui::{self, Column, Line, Table, ask, fmt_date, fmt_size, prompt};

const LOGO: &str = r"
         █████╗ ████████╗ ██╗       █████╗  ███████╗
        ██╔══██╗╚══██╔══╝ ██║      ██╔══██╗ ██╔════╝
        ███████║   ██║    ██║      ███████║ ███████╗
        ██╔══██║   ██║    ██║      ██╔══██║ ╚════██║
        ██║  ██║   ██║    ███████╗ ██║  ██║ ███████║
        ╚═╝  ╚═╝   ╚═╝    ╚══════╝ ╚═╝  ╚═╝ ╚══════╝
";

fn show_results(releases: &[ReleaseRow], query: &str, page: i64, total_pages: i64, total: i64, page_size: i64) {
    ui::clear();

    let start = page * page_size + 1;
    let end = ((page + 1) * page_size).min(total);
    let mut header = vec![
        Line::styled(format!("Search: {query}"), "bold"),
        Line::plain(format!("Page {} of {total_pages}", page + 1)),
    ];
    header.push(Line::styled(format!("Showing {start}-{end} of {total} results"), "dim"));
    ui::print_lines(&ui::panel(&header, "cyan"));

    let mut table = Table::new(vec![
        Column::new("#").width(4).right(),
        Column::new("Name").flex(),
        Column::new("Size").width(10).right(),
        Column::new("Parts").width(8).right(),
        Column::new("Date").width(12),
        Column::new("Status").width(10),
    ]);

    let width = ui::term_size().0.saturating_sub(40).max(20);

    for (i, r) in releases.iter().enumerate() {
        table.add_row(vec![
            Line::plain((i + 1).to_string()),
            Line::plain(ui::ellipsize(&r.name, width)),
            Line::plain(fmt_size(r.size)),
            Line::plain(r.parts.map(|p| p.to_string()).unwrap_or_else(|| "None".into())),
            Line::plain(fmt_date(r.posted_date.as_deref())),
            if r.complete { Line::default() } else { Line::styled("[broken]", "red") },
        ]);
    }

    table.print();
    ui::print("\n[dim]0. Back[/dim]");

    if page > 0 {
        ui::print("[cyan]p.[/cyan] Previous Page");
    }
    if page < total_pages - 1 {
        ui::print("[cyan]n.[/cyan] Next Page");
    }
    ui::print("[cyan]g.[/cyan] Go to Page");
}

fn show_release(release: &ReleaseRow, articles: &[ArticleRow]) {
    let mut info = vec![
        Line::styled(format!("Release: {}", release.name), "bold"),
        Line::styled(
            format!(
                "poster: {}   posted: {}",
                release.poster.as_deref().filter(|p| !p.is_empty()).unwrap_or("unknown"),
                fmt_date(release.posted_date.as_deref())
            ),
            "dim",
        ),
    ];

    let mut summary = Line::plain(format!(
        "{} - {} parts - ",
        fmt_size(release.size),
        release.parts.map(|p| p.to_string()).unwrap_or_else(|| "None".into())
    ));
    if release.complete {
        summary.push("complete", "green");
    } else {
        summary.push("incomplete", "red");
    }
    info.push(summary);
    ui::print_lines(&ui::panel(&info, "blue"));

    if articles.is_empty() {
        return;
    }

    let mut files: indexmap::IndexMap<&str, Vec<&ArticleRow>> = indexmap::IndexMap::new();
    for a in articles {
        files.entry(a.filename.as_deref().unwrap_or("?")).or_default().push(a);
    }

    let width = ui::term_size().0.saturating_sub(20).max(20);
    let mut table = Table::new(vec![Column::new("name").style("white").flex(), Column::new("parts").style("dim")])
        .title(format!("files ({})", files.len()))
        .no_header();
    table.gap = 1;

    for (filename, parts) in files.iter().take(30) {
        let present = parts.iter().map(|p| p.part).collect::<std::collections::HashSet<_>>().len() as i64;
        let expected = parts.iter().filter_map(|p| p.total_parts).filter(|t| *t != 0).max().unwrap_or(present);
        table.add_row(vec![Line::plain(ui::ellipsize(filename, width)), Line::plain(format!("{present}/{expected}"))]);
    }

    table.print();

    if files.len() > 30 {
        ui::print(&format!("  [dim]... and {} more[/dim]", files.len() - 30));
    }
}

/// First run: ask for a server. A blank host quits without writing anything.
fn setup(existing: Option<Config>) -> bool {
    let path = paths::config_file();
    let why = if existing.is_some() { "has no usenet servers" } else { "not found" };

    ui::print_panel(
        &format!(
            "[red]{} {why}[/red]\n[dim]already have a config.json somewhere else? quit and point ATLAS_HOME at its folder\nor enter a server below (blank host to quit)[/dim]",
            path.display()
        ),
        "red",
    );

    let host = prompt("Host: ").trim().to_string();
    if host.is_empty() || host == "0" {
        ui::dim("nothing saved");
        return false;
    }

    let username = prompt("Username: ").trim().to_string();
    let password = prompt("Password: ");
    let port = ask("Port (563): ", Some(563)).clamp(1, 65535) as u16;

    // keep groups / api key from a config.json that just lacked servers
    let mut cfg = existing.unwrap_or_default();
    cfg.servers = vec![UsenetServer::new(&host, &username, &password, port)];

    if let Err(e) = save_config(&cfg) {
        ui::error(&format!("couldnt save config: {e}"));
        return false;
    }

    if cfg.groups.is_empty() {
        ui::print_panel("[yellow]group empty rn, select one from the Groups menu[/yellow]", "yellow");
    }
    true
}

fn release_actions(release: &ReleaseRow) {
    let articles = get_articles(release.id).unwrap_or_default();

    loop {
        ui::clear();
        // actions for the picked release
        show_release(release, &articles);
        Table::menu(&[("1.", "Download"), ("2.", "Save NZB"), ("0.", "Back")]).print();

        match prompt("\nChoice: ").trim() {
            "1" => {
                match download_release(release.id) {
                    Ok(true) => ui::print_panel(
                        "[green]Download queued it is downloading in background.[/green]\n[dim]Finished files land ~/Downloads[/dim]",
                        "green",
                    ),
                    Ok(false) => {}
                    Err(e) => ui::error(&format!("couldnt queue download: {e}")),
                }
                ui::pause();
                return;
            }
            "2" => {
                let _ = generate_nzb(release.id, None);
                ui::pause();
                return;
            }
            "0" => return,
            _ => {
                ui::error("invalid");
                ui::pause();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Group,
    All,
    Obfuscated,
}

fn do_search(config: &Config) {
    loop {
        ui::clear();
        let menu =
            Table::menu(&[("1.", "Current Group"), ("2.", "All Groups"), ("3.", "Obfuscated Posts"), ("0.", "Back")]);
        ui::print_lines(&ui::panel(&menu.render(ui::term_size().0.saturating_sub(4)), "green"));

        let scope = match ask("\nChoice: ", None) {
            1 => Scope::Group,
            2 => Scope::All,
            3 => Scope::Obfuscated,
            _ => return,
        };

        let mut query = if scope == Scope::Obfuscated {
            "obfuscated".to_string()
        } else {
            ui::print("[dim]0. Back[/dim]\n");
            let q = prompt("Search: ").trim().to_string();
            if q.is_empty() || q == "0" {
                continue;
            }
            q
        };

        let mut page = 0;
        let page_size = (ui::term_size().1 as i64 - 15).max(10);

        loop {
            let result = match scope {
                Scope::Group => count_releases(&query, &config.group)
                    .and_then(|t| Ok((t, search_releases(&query, &config.group, page, page_size)?))),
                Scope::All => {
                    count_all_releases(&query).and_then(|t| Ok((t, search_all_releases(&query, page, page_size)?)))
                }
                Scope::Obfuscated => count_obfuscated().and_then(|t| Ok((t, search_obfuscated(page, page_size)?))),
            };

            let (total, releases) = match result {
                Ok(r) => r,
                Err(_) => {
                    ui::print_panel("[red]couldnt search, db error[/red]", "red");
                    return;
                }
            };

            if total == 0 {
                ui::print_panel("[red]no releases found[/red]", "red");

                if scope == Scope::Obfuscated {
                    return;
                }

                query = prompt("\nSearch: ").trim().to_string();
                if query.is_empty() || query == "0" {
                    break;
                }
                page = 0;
                continue;
            }

            let total_pages = ((total + page_size - 1) / page_size).max(1);

            if page > total_pages - 1 {
                page = total_pages - 1;
                continue;
            }

            show_results(&releases, &query, page, total_pages, total, page_size);

            let choice = prompt("\nChoice: ").trim().to_string();

            match choice.as_str() {
                "0" => return,
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
                    if page < total_pages - 1 {
                        page += 1;
                    } else {
                        ui::dim("already on the last page");
                        ui::pause();
                    }
                    continue;
                }
                "g" => {
                    let target = prompt(&format!("Go to page (1-{total_pages}): ")).trim().parse::<i64>().unwrap_or(-1);
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
                Ok(n) if (1..=releases.len()).contains(&n) => release_actions(&releases[n - 1]),
                Ok(_) => {
                    ui::error("not on this page");
                    ui::pause();
                }
                Err(_) => {
                    ui::error("invalid");
                    ui::pause();
                }
            }
        }
    }
}

fn save_or_report(cfg: &Config) -> bool {
    match save_config(cfg) {
        Ok(()) => true,
        Err(e) => {
            ui::error(&format!("couldnt save config: {e}"));
            ui::pause();
            false
        }
    }
}

fn server_rows(config: &Config) -> Table {
    let mut table = Table::new(vec![
        Column::new("#").width(3).right(),
        Column::new("Host").flex(),
        Column::new("Port").width(5).right(),
        Column::new("SSL").width(3),
        Column::new("Conns").width(5).right(),
        Column::new("Priority").width(8).right(),
    ]);

    for (i, s) in config.servers.iter().enumerate() {
        table.add_row(vec![
            Line::plain((i + 1).to_string()),
            Line::plain(s.host.clone()),
            Line::plain(s.port.to_string()),
            Line::plain(if s.use_ssl() { "yes" } else { "no" }),
            Line::plain(s.connections().to_string()),
            Line::plain(s.priority.to_string()),
        ]);
    }

    table
}

/// prompt showing the current value, blank keeps it
fn prompt_keep(label: &str, current: &str) -> String {
    let value = prompt(&format!("{label} [{current}]: ")).trim().to_string();
    if value.is_empty() { current.to_string() } else { value }
}

fn prompt_server(existing: Option<&UsenetServer>, next_priority: i64) -> Option<UsenetServer> {
    let blank = UsenetServer { priority: next_priority, ..UsenetServer::new("", "", "", 563) };
    let current = existing.unwrap_or(&blank);
    let mut s = current.clone();

    s.host = prompt_keep("Host", &current.host);
    if s.host.is_empty() {
        ui::error("host cant be empty");
        return None;
    }

    s.username = prompt_keep("Username", &current.username);

    let hint = if current.password.is_empty() { "" } else { "keep current" };
    let password = prompt(&format!("Password [{hint}]: "));
    if !password.trim().is_empty() {
        s.password = password.trim().to_string();
    }
    if s.password.is_empty() {
        ui::error("password cant be empty");
        return None;
    }

    s.port = ask(&format!("Port [{}]: ", current.port), Some(current.port as i64)).clamp(1, 65535) as u16;
    s.ssl = Some(
        s.port != 119 && prompt_keep("SSL (y/n)", if current.use_ssl() { "y" } else { "n" }).starts_with(['y', 'Y']),
    );
    s.connections = Some(
        ask(&format!("Connections [{}]: ", current.connections()), Some(current.connections() as i64)).clamp(1, 100)
            as u32,
    );
    s.priority = ask(&format!("Priority, lower is tried first [{}]: ", current.priority), Some(current.priority));

    Some(s)
}

/// list, add, edit and remove usenet servers
fn edit_servers(config: &mut Config) {
    loop {
        ui::clear();
        ui::print_panel(
            "[bold]Usenet servers[/bold]\n[dim]tried in priority order, lowest first. downloads in sabnzbd use the same order[/dim]",
            "blue",
        );
        server_rows(config).print();
        ui::print("\n[dim]0. Back[/dim]");
        ui::print("[cyan]a.[/cyan] Add server");
        if !config.servers.is_empty() {
            ui::print("[cyan]#[/cyan]  Edit server");
            ui::print("[cyan]r #[/cyan] Remove server (e.g. r 2)");
        }

        let choice = prompt("\nChoice: ").trim().to_lowercase();
        let next_priority = config.servers.iter().map(|s| s.priority).max().unwrap_or(0) + 1;

        let changed = match choice.as_str() {
            "0" | "" => return,
            "a" => match prompt_server(None, next_priority) {
                Some(s) => {
                    config.servers.push(s);
                    true
                }
                None => false,
            },
            c if c.starts_with('r') => {
                match c[1..].trim().parse::<usize>().ok().filter(|n| (1..=config.servers.len()).contains(n)) {
                    Some(n) if config.servers.len() > 1 => {
                        let removed = config.servers.remove(n - 1);
                        ui::success(&format!("removed {}", removed.host));
                        true
                    }
                    Some(_) => {
                        ui::error("cant remove the last server, edit it instead");
                        false
                    }
                    None => {
                        ui::error("invalid");
                        false
                    }
                }
            }
            c => match c.parse::<usize>().ok().filter(|n| (1..=config.servers.len()).contains(n)) {
                Some(n) => match prompt_server(Some(&config.servers[n - 1]), next_priority) {
                    Some(s) => {
                        config.servers[n - 1] = s;
                        true
                    }
                    None => false,
                },
                None => {
                    ui::error("invalid");
                    false
                }
            },
        };

        if changed {
            config.sort_servers();
            if save_or_report(config) {
                ui::success("saved");
            }
        }
        ui::pause();
    }
}

fn do_settings() {
    ui::clear();

    let Some(mut config) = load_config() else { return };

    let mode_label = format!("Change indexer mode ({})", config.index_mode);
    let port_label = format!("Change api port ({})", config.api_port());
    let menu = Table::menu(&[
        ("1.", "Usenet servers"),
        ("2.", &mode_label),
        ("3.", "Purge broken releases"),
        ("4.", "Wipe db and cache"),
        ("5.", &port_label),
        ("0.", "Back"),
    ]);
    ui::print_lines(&ui::panel(&menu.render(ui::term_size().0.saturating_sub(4)), "blue"));

    match ask("\nChoice: ", None) {
        1 => edit_servers(&mut config),
        2 => loop {
            ui::clear();

            let modes = ["dynamic", "live", "backfill"];
            let menu = Table::menu(&[("1", "dynamic"), ("2", "live"), ("3", "backfill"), ("0.", "Back")]);
            ui::print_lines(&ui::panel(&menu.render(ui::term_size().0.saturating_sub(4)), "blue"));

            let mode = ask("\nChoice: ", None);
            if mode == 0 {
                return;
            }

            let Some(mode) = usize::try_from(mode - 1).ok().and_then(|i| modes.get(i)) else {
                ui::error("that is not a number");
                continue;
            };

            config.index_mode = mode.to_string();
            if save_or_report(&config) {
                ui::success(&format!("indexer mode set to {mode}"));
            }
            return;
        },
        3 => {
            match purge_broken() {
                Ok(freed) if freed > 0 => {
                    ui::success(&format!("deleted broken releases, freed {}", fmt_size(Some(freed))))
                }
                Ok(_) => ui::dim("nothing broken to purge"),
                Err(e) => ui::error(&format!("purge failed: {e}")),
            }
            ui::pause();
        }
        4 => {
            if indexer_alive() {
                ui::warn("stop the indexer first");
                ui::pause();
                return;
            }

            // every shard goes with it, under the exclusive lock: leaving them
            // would give the new main database cursors and ids over old shards
            if let Err(e) = crate::db::wipe(&paths::database()) {
                if e.downcast_ref::<crate::compact::Busy>().is_some() {
                    ui::warn("the database is in use (indexing, a compaction or a save); nothing was wiped");
                } else {
                    ui::error(&format!("couldnt wipe the database: {e:#}"));
                }
                ui::pause();
                return;
            }

            for f in [paths::pid_file(), paths::indexer_log(), paths::status_file(), paths::stats_file()] {
                let _ = fs::remove_file(f);
            }

            // keep the menus usable after the wipe
            let _ = create_db();

            ui::success("wiped db and cache");
            ui::pause();
        }
        5 => {
            let current = config.api_port();
            let port = ask(&format!("API port ({current}): "), Some(current as i64));

            if !(1..=65535).contains(&port) {
                ui::error("port must be between 1 and 65535");
                ui::pause();
                return;
            }

            config.api_port = Some(port as u16);
            if !save_or_report(&config) {
                return;
            }

            api::stop();
            if let Some(mut fresh) = load_config() {
                api::start(&mut fresh);
            }

            ui::success(&format!("api moved to port {port}"));
            ui::pause();
        }
        _ => {}
    }
}

fn remove_group(config: &mut Config) {
    let groups = config.groups.clone();
    if groups.len() <= 1 {
        return;
    }

    let per_page = 5;
    let total_pages = groups.len().div_ceil(per_page).max(1);
    let mut page = 0;

    loop {
        ui::clear();

        let start = page * per_page;
        let end = (start + per_page).min(groups.len());

        ui::print_panel(
            &format!(
                "[bold]Remove group[/bold]\nPage {} of {total_pages}\n[dim]Showing {}-{end} of {} groups[/dim]",
                page + 1,
                start + 1,
                groups.len()
            ),
            "red",
        );

        let mut table = Table::new(vec![Column::new("#").width(4).right(), Column::new("Group").flex()]);
        for (i, g) in groups[start..end].iter().enumerate() {
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

        let choice = prompt("\nChoice: ").trim().to_string();

        match choice.as_str() {
            "0" => return,
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
            _ => {}
        }

        let Some(selected) = choice.parse::<usize>().ok().filter(|n| (1..=end - start).contains(n)) else {
            ui::error("invalid");
            ui::pause();
            continue;
        };

        let chosen = groups[start + selected - 1].clone();
        config.groups.retain(|g| *g != chosen);

        if config.group == chosen {
            config.group = config.groups.first().cloned().unwrap_or_default();
        }

        if save_or_report(config) {
            ui::success(&format!("removed {chosen}"));
            ui::pause();
        }
        return;
    }
}

fn status_line(config: &Config, indexing: bool) -> Line {
    let status = get_status();
    let st = status["status"].as_str().unwrap_or("stopped");
    let label = status["group"].as_str().filter(|g| !g.is_empty()).unwrap_or(&config.group).to_string();
    let err_count = status["error_count"].as_i64().unwrap_or(0);
    let stale = status["stale"].as_bool().unwrap_or(false);

    let mut line = Line::default();

    if indexing {
        if st == "warning" {
            line.push(format!("{label} "), "yellow").push("[WARNING]", "yellow bold");
            if err_count > 0 {
                line.push(format!(" ({err_count} errors)"), "yellow");
            }
        } else if status["idle"].as_bool().unwrap_or(false) {
            line.push(format!("{label} (idle)"), "cyan");
        } else {
            line.push(format!("{label} "), "green").push("[active]", "green bold");
        }
    } else if st == "error" {
        if stale {
            line.push("stopped (last run failed)", "dim");
        } else {
            line.push("FAILED (error)", "red bold");
        }
    } else if st == "warning" {
        if stale {
            line.push("stopped (last run: warning)", "dim");
        } else {
            line.push("stopped (warning)", "yellow");
        }
    } else {
        line.push("stopped", "dim");
    }

    line
}

pub fn main_menu() -> i32 {
    // compaction is held off only while setting up: the menu itself doesnt
    // write (its saves hold it off for themselves)
    match create_db() {
        Ok(Some(_setup)) => {}
        Ok(None) => ui::warn(
            "the database is being compacted; indexing, AI search saves and purging are refused till it's done",
        ),
        Err(e) => {
            ui::error(&format!("couldnt open database {}: {e:#}", paths::database().display()));
            return 1;
        }
    }

    // a config.json that's there but broken isnt missing: setup would write over it
    if let Some(problem) = crate::config::file_problem() {
        ui::error(&format!("{problem}. fix it and start atlas again"));
        return 1;
    }
    let mut config = match load_config() {
        Some(c) if !c.servers.is_empty() => c,
        other => {
            if !setup(other) {
                return 1;
            }
            match load_config().filter(|c| !c.servers.is_empty()) {
                Some(c) => c,
                None => {
                    ui::error("setup failed, no config found");
                    return 1;
                }
            }
        }
    };

    ui::print_panel(
        &format!(
            "[green]config loaded[/green] [dim]{}[/dim]\nServer: {}\nCurrent Group: {}",
            paths::config_file().display(),
            config.servers_label(),
            config.group
        ),
        "cyan",
    );

    api::start(&mut config);

    loop {
        ui::clear();

        let indexing = indexer_alive();

        let mut content: Vec<Line> = LOGO.lines().map(|l| Line::styled(l, "bold cyan")).collect();
        content.push(Line::default());

        let mut group_line = Line::styled("Current Group : ", "bold");
        group_line.push(config.group.clone(), "");
        content.push(group_line);

        let mut idx_line = Line::styled("Indexing      : ", "bold");
        idx_line.append(status_line(&config, indexing));
        content.push(idx_line);
        content.push(Line::default());

        let mut items =
            vec![("1.", if indexing { "Stop Indexing" } else { "Start Indexing" }), ("2.", "Search"), ("3.", "Groups")];
        if config.groups.len() > 1 {
            items.push(("4.", "Remove group"));
        }
        items.extend([
            ("5.", "Live Dashboard"),
            ("6.", "AI Search"),
            ("7.", "Settings"),
            ("8.", "Stats Dashboard"),
            ("0.", "Exit"),
        ]);

        let mut menu = Table::menu(&items);
        menu.indent = 0;
        content.extend(menu.render(ui::term_size().0.saturating_sub(4)));

        ui::print_lines(&ui::panel(&content, "cyan"));

        match prompt("\nChoice: ").trim() {
            "1" => {
                if indexing {
                    if stop_background_indexer() {
                        ui::success("indexing stopped");
                    } else {
                        ui::warn("indexer wasnt running");
                    }
                } else if start_background_indexer() {
                    ui::success("indexing started");
                }
            }
            "2" => do_search(&config),
            "3" => {
                groups_menu(&mut config);
                config = load_config().unwrap_or(config);
            }
            "4" => remove_group(&mut config),
            "5" => {
                if let Err(e) = dashboard::run() {
                    ui::error(&format!("dashboard failed: {e}"));
                    ui::pause();
                }
            }
            "6" => ai_search(&config),
            "7" => {
                do_settings();
                config = load_config().unwrap_or(config);
            }
            "8" => {
                if let Err(e) = stats_dashboard::run() {
                    ui::error(&format!("stats dashboard failed: {e}"));
                    ui::pause();
                }
            }
            "0" => {
                ui::print("\n[bold cyan]byee.[/bold cyan]");
                break;
            }
            _ => {}
        }
    }

    api::stop();
    0
}

pub fn selftest() -> i32 {
    if let Some(problem) = crate::config::file_problem() {
        println!("selftest: {problem}");
        return 1;
    }
    let Some(cfg) = load_config() else {
        println!("selftest: no config found (run Settings or set ATLAS_NNTP_* env)");
        return 1;
    };

    if cfg.servers.is_empty() {
        println!("selftest: no usenet servers configured");
        return 1;
    }

    // check every server, in the order they get tried
    let mut ok = 0;
    for (i, s) in cfg.servers.iter().enumerate() {
        print!(
            "selftest {}: {}:{} ssl={} user={} priority={} connections={} ... ",
            i + 1,
            s.host,
            s.port,
            s.use_ssl(),
            s.username,
            s.priority,
            s.connections()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());

        if s.username.is_empty() || s.password.is_empty() {
            println!("skipped: no username/password set");
            continue;
        }

        match crate::nntp::BlockingPool::check_server(s) {
            Ok(()) => {
                println!("connected OK");
                ok += 1;
            }
            Err(e) => println!("FAILED: {e}"),
        }
    }

    println!("selftest: {ok}/{} servers OK", cfg.servers.len());
    if ok > 0 { 0 } else { 1 }
}
