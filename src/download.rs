use std::time::Duration;

use anyhow::Result;

use crate::nzb::{generate_nzb, nzb_filename};
use crate::sab;
use crate::search::{get_articles, get_release};
use crate::ui;

/// Hand a release to SABnzbd via its watched folder. true when it got queued.
pub fn download_release(release_id: i64) -> Result<bool> {
    let Some(release) = get_release(release_id)? else {
        ui::error("Release not found");
        return Ok(false);
    };

    let articles = get_articles(release_id)?;

    if articles.is_empty() {
        ui::error("No articles found");
        return Ok(false);
    }

    let complete_dir = sab::get_complete_dir();
    if let Some(first_file) = articles.iter().find_map(|a| a.filename.as_deref().filter(|f| !f.is_empty()))
        && complete_dir.join(first_file).exists()
    {
        ui::warn("already downloaded");
        return Ok(false);
    }

    sab::configure_servers();

    if !sab::is_running() {
        ui::dim("starting sabnzbd...");
        if !sab::start() {
            ui::error("couldnt start sabnzbd");
            return Ok(false);
        }
    }

    if !sab::wait_ready(Duration::from_secs(90)) {
        ui::warn("sab isnt ready");
        return Ok(false);
    }

    let watched_dir = sab::configure_watched_dir();
    let release_name = if release.name.is_empty() { format!("release_{}", release.id) } else { release.name.clone() };
    let filename = nzb_filename(&release_name, Some(release.id));

    if watched_dir.join(&filename).exists() {
        ui::warn("already queued");
        return Ok(false);
    }

    generate_nzb(release_id, Some(&watched_dir))?;

    match sab::job_in_sab(filename.strip_suffix(".nzb").unwrap_or(&filename), Duration::from_secs(10)).as_deref() {
        Some("queued") => {
            ui::success("download queued");
            let _ = webbrowser::open(&sab::get_url());
            Ok(true)
        }
        Some(status) => {
            ui::warn(&format!("already did this one bruh: {status}"));
            Ok(false)
        }
        None => {
            ui::error("couldnt confirm it");
            ui::error("download failed");
            Ok(false)
        }
    }
}
