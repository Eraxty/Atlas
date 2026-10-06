use std::env;
use std::path::PathBuf;

/// Where atlas keeps config.json, atlas.db, logs and status files.
/// `ATLAS_HOME` wins, otherwise the folder the binary lives in.
pub fn app_dir() -> PathBuf {
    if let Some(home) = env::var_os("ATLAS_HOME").filter(|v| !v.is_empty()) {
        // absolute soo the background indexer (started with cwd = app dir) resolves the same place
        let home = PathBuf::from(home);
        return std::path::absolute(&home).unwrap_or(home);
    }

    exe_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn exe_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok().or(Some(p)))
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

pub fn config_file() -> PathBuf {
    app_dir().join("config.json")
}

pub fn database() -> PathBuf {
    app_dir().join("atlas.db")
}

pub fn status_file() -> PathBuf {
    app_dir().join("status.json")
}

pub fn stats_file() -> PathBuf {
    app_dir().join("stats.json")
}

pub fn pid_file() -> PathBuf {
    app_dir().join("bg_indexer.pid")
}

pub fn indexer_log() -> PathBuf {
    app_dir().join("bg_index.log")
}

pub fn sab_log() -> PathBuf {
    app_dir().join("sabnzbd.log")
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}
