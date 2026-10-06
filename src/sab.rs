//! Driving the bundled SABnzbd (still python) for downloads.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::atomic::write_atomic;
use crate::config::{UsenetServer, load_config as load_atlas_config};
use crate::paths::{self, app_dir, exe_dir, home_dir};
use crate::ui;

pub const SAB_DIR_NAME: &str = "SABnzbd-5.0.4";

static PROCESS: LazyLock<Mutex<Option<Child>>> = LazyLock::new(|| Mutex::new(None));

/// sab keeps its ini in the home dir
pub fn config_dir() -> PathBuf {
    home_dir().join(".sabnzbd")
}

pub fn config_file() -> PathBuf {
    config_dir().join("sabnzbd.ini")
}

fn default_watched_dir() -> PathBuf {
    config_dir().join("watched")
}

/// `ATLAS_SAB_DIR`, else SABnzbd-5.0.4 next to the data dir, the binary or the cwd
pub fn sab_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("ATLAS_SAB_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        return dir.join("SABnzbd.py").is_file().then_some(dir);
    }

    let mut candidates = vec![app_dir().join(SAB_DIR_NAME)];
    if let Some(d) = exe_dir() {
        candidates.push(d.join(SAB_DIR_NAME));
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join(SAB_DIR_NAME));
    }

    candidates.into_iter().find(|d| d.join("SABnzbd.py").is_file())
}

/// every `name` executable on PATH, in PATH order
fn all_in_path(name: &str) -> Vec<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ""] } else { &[""] };
    let path = env::var_os("PATH").unwrap_or_default();

    env::split_paths(&path)
        .filter_map(|dir| exts.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|p| p.is_file()))
        .collect()
}

/// python interpreter to run SABnzbd with. `ATLAS_PYTHON` overrides.
pub fn python() -> Option<PathBuf> {
    if let Some(p) = env::var_os("ATLAS_PYTHON").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }

    // skip the microsoft store "python3.exe" stubs in WindowsApps
    let real = |p: &PathBuf| !p.to_string_lossy().contains("WindowsApps");
    let order: &[&str] = if cfg!(windows) { &["python", "py", "python3"] } else { &["python3", "python"] };

    order.iter().flat_map(|name| all_in_path(name)).find(real)
}

/// can we launch a local SABnzbd at all
pub fn available() -> bool {
    sab_dir().is_some() && python().is_some()
}

fn read_ini() -> String {
    // sab on windows writes CRLF
    fs::read_to_string(config_file()).unwrap_or_default().replace("\r\n", "\n")
}

fn unquote(v: &str) -> String {
    v.trim().trim_matches(['"', '\'']).trim().to_string()
}

static SECTION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\[([^\[\]]+)\]\s*$").unwrap());

/// Value of `key` in the top level `[section]` of a configobj style ini.
pub fn ini_get(text: &str, section: &str, key: &str) -> Option<String> {
    let mut current: Option<String> = None;
    let mut found = None;

    for line in text.lines() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("[[") {
            current = None;
            continue;
        }

        if let Some(c) = SECTION_RE.captures(line) {
            current = Some(c[1].trim().to_lowercase());
            continue;
        }

        if current.as_deref() != Some(section) || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if let Some((k, v)) = trimmed.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            found = Some(unquote(v));
        }
    }

    found
}

/// Set `key = value` inside `[section]`, adding the section if needed.
pub fn ini_set(text: &str, section: &str, key: &str, value: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut start = None;
    let mut end = lines.len();

    for (i, line) in lines.iter().enumerate() {
        let is_header = line.trim_start().starts_with('[');
        match start {
            None => {
                if SECTION_RE.captures(line).is_some_and(|c| c[1].trim().eq_ignore_ascii_case(section)) {
                    start = Some(i);
                }
            }
            Some(_) if is_header && !line.trim_start().starts_with("[[") => {
                end = i;
                break;
            }
            Some(_) => {}
        }
    }

    let entry = format!("{key} = {value}");
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    match start {
        Some(s) => {
            let existing = (s + 1..end).find(|&i| {
                out[i].trim_start().split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            });
            match existing {
                Some(i) => out[i] = entry,
                None => out.insert(s + 1, entry),
            }
        }
        None => {
            out.push(format!("[{section}]"));
            out.push(entry);
        }
    }

    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

fn misc(key: &str) -> Option<String> {
    ini_get(&read_ini(), "misc", key).filter(|v| !v.is_empty())
}

fn single_line(s: &str) -> String {
    s.replace(['\n', '\r'], "").trim().to_string()
}

static SERVER_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\[\[s(\d+)\]\][ \t]*$").unwrap());

/// Make sure SABnzbd has every usenet server from config.json, with the
/// same priority order and connection counts, syncing ones it already has.
pub fn configure_servers() {
    let Some(atlas) = load_atlas_config() else { return };

    let original = read_ini();
    let mut text = original.clone();

    for server in atlas.servers.iter().filter(|s| !single_line(&s.host).is_empty()) {
        text = server_section_update(&text, server);
    }

    if text != original {
        let _ = write_atomic(&config_file(), text);
    }
}

/// sab priorities run 0 (first) to 99 (last), same direction as ours
fn sab_priority(server: &UsenetServer) -> i64 {
    server.priority.clamp(0, 99)
}

/// Replace `key = ...` in a server section body, or add it when missing.
fn set_body_key(body: &str, key: &str, value: &str) -> String {
    let re = Regex::new(&format!(r"(?m)^({}\s*=[ \t]*).*$", regex::escape(key))).unwrap();

    if re.is_match(body) {
        return re.replacen(body, 1, |c: &regex::Captures| format!("{}{}", &c[1], value)).into_owned();
    }

    let mut out = body.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{key} = {value}\n"));
    out
}

/// Ini text with `server` added, or its existing section brought in line.
pub fn server_section_update(text: &str, server: &UsenetServer) -> String {
    let host = single_line(&server.host);
    let username = single_line(&server.username);
    let password = single_line(&server.password);
    let ssl = if server.use_ssl() { "1" } else { "0" };
    let priority = sab_priority(server).to_string();
    let port = server.port.to_string();

    let headers: Vec<_> = SERVER_HEADER_RE.captures_iter(text).collect();
    let host_re = Regex::new(&format!(r#"(?mi)^host\s*=\s*['"]?{}['"]?\s*$"#, regex::escape(&host))).unwrap();

    for cap in &headers {
        let body_start = cap.get(0).unwrap().end();
        // body runs until the next section header of any kind
        let body_end = text[body_start..]
            .match_indices('\n')
            .map(|(i, _)| body_start + i + 1)
            .find(|&i| text[i..].trim_start_matches([' ', '\t']).starts_with('['))
            .unwrap_or(text.len());

        let body = &text[body_start..body_end];

        if !host_re.is_match(body) {
            continue;
        }

        let mut new_body = body.to_string();
        let mut keys = vec![
            ("port", port.clone()),
            ("username", username.clone()),
            ("password", format!("\"{password}\"")),
            ("ssl", ssl.to_string()),
            ("priority", priority.clone()),
        ];
        // only override sab's connection count when config.json sets one
        if let Some(c) = server.connections {
            keys.push(("connections", c.to_string()));
        }

        for (key, value) in keys {
            new_body = set_body_key(&new_body, key, &value);
        }

        return format!("{}{}{}", &text[..body_start], new_body, &text[body_end..]);
    }

    // name it after the highest sN we already got
    let index = headers.iter().filter_map(|c| c[1].parse::<u64>().ok()).max().map(|n| n + 1).unwrap_or(0);
    let connections = server.connections();

    let block = format!(
        "[[s{index}]]\n\
         name = s{index}\n\
         displayname = {host}\n\
         host = {host}\n\
         port = {port}\n\
         timeout = 60\n\
         username = {username}\n\
         password = \"{password}\"\n\
         connections = {connections}\n\
         ssl = {ssl}\n\
         ssl_verify = 1\n\
         ssl_ciphers = \"\"\n\
         enable = 1\n\
         required = 0\n\
         optional = 0\n\
         pipelining_requests = 2\n\
         retention = 0\n\
         expire_date = \"\"\n\
         priority = {priority}\n"
    );

    if text.contains("\n[servers]") {
        text.replacen("\n[servers]", &format!("\n[servers]\n{block}"), 1)
    } else if text.starts_with("[servers]") {
        text.replacen("[servers]", &format!("[servers]\n{block}"), 1)
    } else {
        format!("{text}\n[servers]\n{block}")
    }
}

pub fn rotate_log(path: &Path, max_bytes: u64) {
    if fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = fs::remove_file(&old);
        let _ = fs::rename(path, &old);
    }
}

pub fn open_log(path: &Path) -> std::io::Result<File> {
    rotate_log(path, 5 * 1024 * 1024);
    OpenOptions::new().create(true).append(true).open(path)
}

/// Detach a child from our terminal soo ctrl+c in the menu doesnt kill it.
pub fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

fn our_process_alive() -> Option<bool> {
    let mut guard = PROCESS.lock().unwrap();
    let child = guard.as_mut()?;
    Some(matches!(child.try_wait(), Ok(None)))
}

pub fn start() -> bool {
    if our_process_alive() == Some(true) {
        ui::warn("sab already running");
        return true;
    }

    let (Some(dir), Some(python)) = (sab_dir(), python()) else {
        ui::error("couldnt start sabnzbd: SABnzbd or python not found (set ATLAS_SAB_DIR / ATLAS_PYTHON)");
        return false;
    };

    configure_watched_dir();
    configure_servers();

    let log = match open_log(&paths::sab_log()) {
        Ok(f) => f,
        Err(e) => {
            ui::error(&format!("couldnt start sabnzbd: {e}"));
            return false;
        }
    };

    let mut cmd = Command::new(python);
    // -f pins the ini to the one atlas edits (sab defaults elsewhere on mac/windows)
    cmd.args(["-u", "SABnzbd.py", "-f"])
        .arg(config_file())
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .stderr(Stdio::from(log));
    detach(&mut cmd);

    match cmd.spawn() {
        Ok(child) => {
            *PROCESS.lock().unwrap() = Some(child);
            ui::success("started sabnzbd");
            true
        }
        Err(e) => {
            ui::error(&format!("couldnt start sabnzbd: {e}"));
            false
        }
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into()
}

/// anything answering http counts as up, even a 401 from a password protected sab
fn reachable(url: &str, timeout: Duration) -> Result<(), String> {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().into();
    agent.get(url).call().map(|_| ()).map_err(|e| e.to_string())
}

pub fn is_running() -> bool {
    if our_process_alive() == Some(true) {
        return true;
    }

    reachable(&get_url(), Duration::from_secs(2)).is_ok()
}

fn resolve_dir(folder: Option<String>, base: &Path) -> Option<PathBuf> {
    let folder = folder.map(|f| unquote(&f)).filter(|f| !f.is_empty())?;
    let path = PathBuf::from(folder);

    Some(if path.is_absolute() { path } else { base.join(path) })
}

pub fn get_watched_dir() -> Option<PathBuf> {
    // sab saves relative paths soo resolve em against the config dir
    resolve_dir(misc("dirscan_dir"), &config_dir())
}

/// Watched folder sab picks nzbs up from, set to ours if sab has none.
pub fn configure_watched_dir() -> PathBuf {
    let folder = match get_watched_dir() {
        Some(f) => f,
        None => {
            let folder = default_watched_dir();
            let text = ini_set(&read_ini(), "misc", "dirscan_dir", &folder.to_string_lossy());
            let _ = write_atomic(&config_file(), text);
            folder
        }
    };

    let _ = fs::create_dir_all(&folder);
    folder
}

pub fn get_complete_dir() -> PathBuf {
    // relative means against the home dir
    resolve_dir(misc("complete_dir"), &home_dir()).unwrap_or_else(|| home_dir().join("Downloads").join("complete"))
}

pub fn get_api_key() -> String {
    misc("api_key").unwrap_or_default()
}

pub fn get_url() -> String {
    let host = env::var("ATLAS_SAB_HOST").ok().filter(|h| !h.is_empty()).or_else(|| misc("host"));
    let port = env::var("ATLAS_SAB_PORT").ok().filter(|p| !p.is_empty()).or_else(|| misc("port"));

    let host = match host.as_deref() {
        None | Some("0.0.0.0") | Some("::") | Some("") => "127.0.0.1".to_string(),
        Some(h) if h.contains(':') && !h.starts_with('[') => format!("[{h}]"),
        Some(h) => h.to_string(),
    };

    format!("http://{host}:{}/", port.unwrap_or_else(|| "8080".into()))
}

/// Look for a job in sab's queue, then history. "queued", a history status, or None.
pub fn job_in_sab(name: &str, timeout: Duration) -> Option<String> {
    let key = get_api_key();
    let base = get_url();
    let queue_url = format!("{base}api?mode=queue&output=json&apikey={key}");
    let history_url = format!("{base}api?mode=history&output=json&apikey={key}&start=0&limit=50");
    let nzb_name = format!("{name}.nzb");
    let http = agent(Duration::from_secs(2));

    let fetch = |url: &str| -> Option<Value> { http.get(url).call().ok()?.body_mut().read_json::<Value>().ok() };
    let slots = |v: &Value, section: &str| v[section]["slots"].as_array().cloned().unwrap_or_default();

    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if let Some(q) = fetch(&queue_url) {
            // still in the queue
            if slots(&q, "queue").iter().any(|s| {
                let f = s["filename"].as_str().unwrap_or("");
                f == name || f == nzb_name
            }) {
                return Some("queued".into());
            }
        }

        if let Some(h) = fetch(&history_url)
            && let Some(s) = slots(&h, "history").iter().find(|s| s["name"].as_str() == Some(name))
        {
            return Some(s["status"].as_str().unwrap_or("done").to_lowercase());
        }

        thread::sleep(Duration::from_secs(1));
    }

    None
}

pub fn wait_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    thread::sleep(Duration::from_secs(2));
    let url = get_url();

    while Instant::now() < deadline {
        {
            let mut guard = PROCESS.lock().unwrap();
            if let Some(child) = guard.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                ui::warn(&format!("sabnzbd process exited with {status}"));
                return false;
            }
        }

        match reachable(&url, Duration::from_secs(3)) {
            Ok(()) => return true,
            Err(e) => ui::warn(&format!("sab not ready yet: {e}")),
        }

        thread::sleep(Duration::from_secs(2));
    }

    ui::warn(&format!("sabnzbd didnt respond at {url}"));
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "__version__ = 19\n[misc]\nhost = 127.0.0.1\nport = 8080\ncomplete_dir = Downloads/complete\napi_key = abc\n[servers]\n[[s0]]\nname = s0\nhost = news.example.com\nport = 563\nusername = bob\npassword = \"pw\"\nssl = 1\n[categories]\n[[*]]\nname = *\n";

    #[test]
    fn reads_misc_only() {
        assert_eq!(ini_get(INI, "misc", "port").as_deref(), Some("8080"));
        assert_eq!(ini_get(INI, "misc", "api_key").as_deref(), Some("abc"));
        assert_eq!(ini_get(INI, "misc", "name"), None);
        assert_eq!(ini_get(INI, "misc", "dirscan_dir"), None);
    }

    #[test]
    fn sets_misc_values() {
        let t = ini_set(INI, "misc", "dirscan_dir", "/w");
        assert_eq!(ini_get(&t, "misc", "dirscan_dir").as_deref(), Some("/w"));
        let t = ini_set(&t, "misc", "dirscan_dir", "/x");
        assert_eq!(t.matches("dirscan_dir").count(), 1);
        assert_eq!(ini_get(&t, "misc", "dirscan_dir").as_deref(), Some("/x"));

        let fresh = ini_set("", "misc", "dirscan_dir", "/w");
        assert_eq!(fresh, "[misc]\ndirscan_dir = /w\n");
    }

    fn server(host: &str, port: u16, user: &str, pass: &str, priority: i64) -> UsenetServer {
        let mut s = UsenetServer::new(host, user, pass, port);
        s.priority = priority;
        s
    }

    #[test]
    fn server_sync() {
        // in sync already -> untouched
        let synced = server_section_update(INI, &server("news.example.com", 563, "bob", "pw", 0));
        let again = server_section_update(&synced, &server("news.example.com", 563, "bob", "pw", 0));
        assert_eq!(synced, again);
        assert!(synced.contains("priority = 0\n"));

        let t = server_section_update(INI, &server("news.example.com", 119, "bob", "new", 3));
        assert!(t.contains("port = 119\n"));
        assert!(t.contains("password = \"new\"\n"));
        assert!(t.contains("ssl = 0\n"));
        assert!(t.contains("priority = 3\n"));
        assert!(!t.contains("connections"), "sab's own connection count is left alone");
        assert_eq!(t.matches("[[s").count(), 1);

        let mut with_conns = server("news.example.com", 563, "bob", "pw", 1);
        with_conns.connections = Some(10);
        assert!(server_section_update(INI, &with_conns).contains("connections = 10\n"));

        let t = server_section_update(INI, &server("other.example.com", 563, "u", "p", 2));
        assert!(t.contains("[[s1]]\nname = s1\n"));
        assert!(t.contains("connections = 10\n"));
        assert!(t.contains("priority = 2\n"));
        assert_eq!(t.matches("[servers]").count(), 1);

        let t = server_section_update("", &server("h", 563, "u", "p", 1));
        assert!(t.starts_with("\n[servers]\n[[s0]]"));
    }

    #[test]
    fn several_servers_get_their_own_sections() {
        let mut text = String::new();
        for (i, host) in ["a.example", "b.example", "c.example"].iter().enumerate() {
            text = server_section_update(&text, &server(host, 563, "u", "p", i as i64 + 1));
        }
        assert_eq!(text.matches("[[s").count(), 3);
        assert!(text.contains("[[s2]]\nname = s2\ndisplayname = c.example"));
        // re-running is stable
        let mut again = text.clone();
        for (i, host) in ["a.example", "b.example", "c.example"].iter().enumerate() {
            again = server_section_update(&again, &server(host, 563, "u", "p", i as i64 + 1));
        }
        assert_eq!(text, again);
    }
}
