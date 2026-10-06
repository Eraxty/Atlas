use std::env;
use std::fs;
use std::io;

use serde_json::{Map, Value, json};

use crate::atomic::write_atomic;
use crate::paths::config_file;

pub const SERVICE: &str = "atlas";
pub const DEFAULT_API_PORT: u16 = 9090;
pub const DEFAULT_API_HOST: &str = "127.0.0.1";
/// requests in flight per server (indexer and sabnzbd) when config.json doesnt say
pub const DEFAULT_CONNECTIONS: u32 = 10;
/// servers without a priority go after the ones that have one
pub const DEFAULT_PRIORITY: i64 = 99;
/// article numbers the indexer takes on per pass over a group (50 requests)
pub const DEFAULT_BATCH_SIZE: u64 = 500_000;
/// with `parallel_groups` unset, one group runs per this many connections
pub const CONNECTIONS_PER_GROUP: usize = 5;
/// sanity cap on groups indexed at once
pub const MAX_PARALLEL_GROUPS: usize = 256;
/// article numbers per XOVER request (one connection's slice of a batch).
/// providers spend most of a request's time on their side, not the network:
/// measured on old articles, 10k per request got 3-6x the headers/s of 1k on
/// one connection, and 50k was no better on most servers and erratic
pub const DEFAULT_REQUEST_SIZE: u64 = 10_000;
/// headers fetched and not saved yet, over every group at once, when
/// config.json doesnt say (about 0.3GB of headers)
pub const DEFAULT_MAX_UNSAVED_HEADERS: u64 = 500_000;
/// article numbers of backfill left on a group's home server before its
/// backfill is split into day chunks over every server that carries it
pub const SPLIT_MIN_BACKLOG: i64 = 10_000_000;

/// One usenet provider. Lower `priority` is tried first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsenetServer {
    pub host: String,
    pub username: String,
    pub password: String,
    pub port: u16,
    /// None = guess from the port (563 ssl, 119 plain)
    pub ssl: Option<bool>,
    pub connections: Option<u32>,
    pub priority: i64,
    /// ask for gzip compressed header listings, default on
    pub compress: Option<bool>,
    /// take part in indexing (header downloads), default on. off keeps e.g. a
    /// block account for downloads and article lookups only
    pub index: Option<bool>,
    /// set apart in stored state (cursors, chunk claims, sweeps) from other
    /// accounts on the same host and port, see `nntp::server_keys`. Only
    /// for a second account on a provider that numbers articles differently
    pub key: Option<String>,
}

impl UsenetServer {
    pub fn new(host: &str, username: &str, password: &str, port: u16) -> Self {
        UsenetServer {
            host: host.trim().to_string(),
            username: username.to_string(),
            password: password.to_string(),
            port,
            ssl: None,
            connections: None,
            priority: 1,
            compress: None,
            index: None,
            key: None,
        }
    }

    pub fn use_ssl(&self) -> bool {
        self.ssl.unwrap_or_else(|| crate::nntp::detect_use_ssl(&self.host, self.port))
    }

    pub fn connections(&self) -> u32 {
        self.connections.unwrap_or(DEFAULT_CONNECTIONS)
    }

    pub fn indexes(&self) -> bool {
        self.index.unwrap_or(true)
    }

    fn from_value(v: &Value) -> Option<UsenetServer> {
        let s = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let host = s("host").trim().to_string();

        if host.is_empty() {
            return None;
        }

        Some(UsenetServer {
            host,
            username: s("username"),
            password: s("password"),
            port: v.get("port").and_then(as_port).unwrap_or(563),
            ssl: v.get("ssl").and_then(as_bool),
            connections: v.get("connections").and_then(as_int).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0),
            priority: v.get("priority").and_then(as_int).unwrap_or(DEFAULT_PRIORITY),
            compress: v.get("compress").and_then(as_bool),
            index: v.get("index").and_then(as_bool),
            key: v.get("key").and_then(Value::as_str).map(str::trim).filter(|k| !k.is_empty()).map(String::from),
        })
    }

    fn to_value(&self, password: bool) -> Value {
        let mut m = Map::new();
        m.insert("host".into(), json!(self.host));
        m.insert("username".into(), json!(self.username));
        m.insert("password".into(), json!(if password { self.password.as_str() } else { "" }));
        m.insert("port".into(), json!(self.port));
        if let Some(ssl) = self.ssl {
            m.insert("ssl".into(), json!(ssl));
        }
        if let Some(c) = self.connections {
            m.insert("connections".into(), json!(c));
        }
        m.insert("priority".into(), json!(self.priority));
        if let Some(c) = self.compress {
            m.insert("compress".into(), json!(c));
        }
        if let Some(i) = self.index {
            m.insert("index".into(), json!(i));
        }
        if let Some(k) = &self.key {
            m.insert("key".into(), json!(k));
        }
        Value::Object(m)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// sorted by priority, first one is tried first
    pub servers: Vec<UsenetServer>,
    pub group: String,
    pub groups: Vec<String>,
    pub index_mode: String,
    pub api_port: Option<u16>,
    pub api_host: String,
    pub api_key: Option<String>,
    pub batch_size: Option<u64>,
    pub request_size: Option<u64>,
    /// None = SPLIT_MIN_BACKLOG
    pub split_min_backlog: Option<i64>,
    /// groups indexed at the same time, None = worked out from the connections
    pub parallel_groups: Option<usize>,
    /// compact the database every 24 hours from the indexer
    pub auto_run_compact: bool,
    /// None = DEFAULT_MAX_UNSAVED_HEADERS
    pub max_unsaved_headers: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            servers: Vec::new(),
            group: String::new(),
            groups: Vec::new(),
            index_mode: "dynamic".into(),
            api_port: None,
            api_host: DEFAULT_API_HOST.into(),
            api_key: None,
            batch_size: None,
            request_size: None,
            split_min_backlog: None,
            parallel_groups: None,
            auto_run_compact: false,
            max_unsaved_headers: None,
        }
    }
}

impl Config {
    pub fn api_port(&self) -> u16 {
        self.api_port.unwrap_or(DEFAULT_API_PORT)
    }

    pub fn batch_size(&self) -> u64 {
        self.batch_size.unwrap_or(DEFAULT_BATCH_SIZE).max(1)
    }

    /// Groups indexed at once on each server, given each indexing server's
    /// connections: one group per `CONNECTIONS_PER_GROUP` connections, or
    /// `parallel_groups` shared out by connections. Every server gets at least one.
    pub fn workers_per_server(&self, connections: &[usize]) -> Vec<usize> {
        if connections.is_empty() {
            return Vec::new();
        }

        let Some(total) = self.parallel_groups else {
            return connections.iter().map(|c| (c / CONNECTIONS_PER_GROUP).clamp(1, MAX_PARALLEL_GROUPS)).collect();
        };

        let total = total.clamp(connections.len(), MAX_PARALLEL_GROUPS);
        let sum: usize = connections.iter().sum::<usize>().max(1);

        // largest remainder, at least one each
        let mut shares: Vec<(usize, usize, usize)> =
            connections.iter().enumerate().map(|(i, c)| (i, (total * c / sum).max(1), (total * c) % sum)).collect();
        let mut given: usize = shares.iter().map(|s| s.1).sum();

        shares.sort_by_key(|s| std::cmp::Reverse(s.2));
        let n = shares.len();
        let mut k = 0;
        while given < total {
            shares[k % n].1 += 1;
            given += 1;
            k += 1;
        }

        shares.sort_by_key(|s| s.0);
        shares.into_iter().map(|s| s.1).collect()
    }

    pub fn request_size(&self) -> u64 {
        self.request_size.unwrap_or(DEFAULT_REQUEST_SIZE).max(1)
    }

    /// headers the indexer fetches ahead of saving them, over every group at
    /// once: what the pool takes of the setting (`nntp::unsaved_cap`)
    pub fn max_unsaved_headers(&self) -> usize {
        let n = self.max_unsaved_headers.unwrap_or(DEFAULT_MAX_UNSAVED_HEADERS);
        crate::nntp::unsaved_cap(usize::try_from(n).unwrap_or(usize::MAX))
    }

    /// article numbers of backfill left before a group's backfill is split
    /// over every server that carries it
    pub fn split_min_backlog(&self) -> i64 {
        self.split_min_backlog.unwrap_or(SPLIT_MIN_BACKLOG).max(1)
    }

    /// the highest priority server
    pub fn primary(&self) -> Option<&UsenetServer> {
        self.servers.first()
    }

    /// `news.a.com` or `news.a.com (+2 fallback)`
    pub fn servers_label(&self) -> String {
        match self.servers.len() {
            0 => String::new(),
            1 => self.servers[0].host.clone(),
            n => format!("{} (+{} fallback)", self.servers[0].host, n - 1),
        }
    }

    /// stable sort: equal priorities keep their order from the file
    pub fn sort_servers(&mut self) {
        self.servers.sort_by_key(|s| s.priority);
    }

    /// groups the indexer should walk, falls back to the single `group` field
    pub fn tracked_groups(&self) -> Vec<String> {
        let groups: Vec<String> = self.groups.iter().filter(|g| !g.is_empty()).cloned().collect();

        if groups.is_empty() && !self.group.is_empty() {
            return vec![self.group.clone()];
        }

        groups
    }

    fn from_value(v: &Value) -> Config {
        let s = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();

        let mut servers: Vec<UsenetServer> = v
            .get("usenet_servers")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(UsenetServer::from_value).collect())
            .unwrap_or_default();

        // the old single server layout
        if servers.is_empty()
            && let Some(mut legacy) = UsenetServer::from_value(v)
        {
            legacy.priority = 1;
            legacy.connections = None;
            servers.push(legacy);
        }

        let mut cfg = Config {
            servers,
            group: s("group"),
            groups: v
                .get("groups")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            index_mode: Some(s("index_mode")).filter(|m| !m.is_empty()).unwrap_or_else(|| "dynamic".into()),
            api_port: v.get("api_port").and_then(as_port),
            api_host: Some(s("api_host")).filter(|h| !h.is_empty()).unwrap_or_else(|| DEFAULT_API_HOST.into()),
            api_key: v.get("api_key").and_then(Value::as_str).filter(|k| !k.is_empty()).map(String::from),
            batch_size: v.get("batch_size").and_then(as_int).and_then(|n| u64::try_from(n).ok()).filter(|n| *n > 0),
            request_size: v.get("request_size").and_then(as_int).and_then(|n| u64::try_from(n).ok()).filter(|n| *n > 0),
            split_min_backlog: v.get("split_min_backlog").and_then(as_int).filter(|n| *n > 0),
            parallel_groups: v
                .get("parallel_groups")
                .and_then(as_int)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0),
            auto_run_compact: v.get("auto_run_compact").and_then(Value::as_bool).unwrap_or(false),
            max_unsaved_headers: v
                .get("max_unsaved_headers")
                .and_then(as_int)
                .and_then(|n| u64::try_from(n).ok())
                .filter(|n| *n > 0),
        };

        cfg.sort_servers();

        if cfg.groups.is_empty() && !cfg.group.is_empty() {
            cfg.groups = vec![cfg.group.clone()];
        }

        cfg
    }
}

fn as_port(v: &Value) -> Option<u16> {
    as_int(v).and_then(|n| u16::try_from(n).ok())
}

fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

/// server creds come from ATLAS_NNTP_* (docker) instead of config.json
fn env_creds_active() -> bool {
    ["ATLAS_NNTP_HOST", "ATLAS_NNTP_USER", "ATLAS_NNTP_PASS"].iter().all(|k| env_nonempty(k).is_some())
}

/// Why config.json cant be used though it's there, see `file_problem_at`.
pub fn file_problem() -> Option<String> {
    file_problem_at(&config_file())
}

/// Why the config file at `path` cant be used though it's there: it cant be
/// read, or isnt valid JSON (where, as serde_json says, never the text: it
/// holds passwords), or is JSON that isnt an object (`null`, a list...: it
/// would load as an empty config, and setup would save over it). None when
/// it's fine, or isnt there at all.
pub fn file_problem_at(path: &std::path::Path) -> Option<String> {
    let text = match fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("couldnt read {}: {e}", path.display())),
        Ok(text) => text,
    };
    match serde_json::from_str::<Value>(&text) {
        Err(e) => Some(format!("{} isnt valid JSON: {e}", path.display())),
        Ok(Value::Object(_)) => None,
        Ok(other) => {
            let kind = match other {
                Value::Null => "null",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                _ => "an array",
            };
            Some(format!("{} holds {kind}, not an object", path.display()))
        }
    }
}

fn read_file() -> Option<Value> {
    let text = fs::read_to_string(config_file()).ok()?;
    serde_json::from_str(&text).ok().filter(Value::is_object)
}

/// Load config. Servers come from `usenet_servers` (or the old top level
/// host/username/password/port). `ATLAS_NNTP_*` env vars (docker) add a
/// server in front of those, while groups and the api key still come from
/// the file soo they survive restarts.
pub fn load_config() -> Option<Config> {
    let file = read_file();

    let host = env_nonempty("ATLAS_NNTP_HOST");
    let user = env_nonempty("ATLAS_NNTP_USER");
    let pass = env_nonempty("ATLAS_NNTP_PASS");

    if let (Some(host), Some(user), Some(pass)) = (host, user, pass) {
        let mut cfg = file.as_ref().map(Config::from_value).unwrap_or_default();

        let port = env::var("ATLAS_NNTP_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(563);
        let mut server = UsenetServer::new(&host, &user, &pass, port);
        server.priority = i64::MIN;
        server.connections =
            env::var("ATLAS_NNTP_CONNECTIONS").ok().and_then(|c| c.trim().parse().ok()).filter(|c| *c > 0);

        cfg.servers.retain(|s| !s.host.eq_ignore_ascii_case(&server.host));
        cfg.servers.insert(0, server);

        if let Some(mode) = env_nonempty("ATLAS_INDEX_MODE") {
            cfg.index_mode = mode;
        }

        if let Some(port) = env_nonempty("ATLAS_API_PORT") {
            cfg.api_port = Some(port.trim().parse().unwrap_or(DEFAULT_API_PORT));
        }

        cfg.api_host = env_nonempty("ATLAS_API_HOST").unwrap_or_else(|| DEFAULT_API_HOST.into());

        return Some(cfg);
    }

    let mut cfg = Config::from_value(&file?);

    // passwords that arent in the file live in the os keyring
    for server in cfg.servers.iter_mut().filter(|s| s.password.is_empty()) {
        if let Some(password) = keyring::get(&server.username) {
            server.password = password;
        }
    }

    Some(cfg)
}

/// Persist the config, keeping any keys in config.json atlas doesnt know about.
///
/// One server is saved in the old top level layout with its password in the
/// os keyring when possible. Several servers are saved as `usenet_servers`;
/// a password that was already written in the file stays there, new ones go
/// to the keyring when it works. Server creds from env vars never get saved.
pub fn save_config(cfg: &Config) -> io::Result<()> {
    let existing = read_file();
    let mut out = match &existing {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };

    let mut private = out.get("password").and_then(Value::as_str).is_some_and(|p| !p.is_empty());

    if !env_creds_active() {
        let list_form = out.contains_key("usenet_servers") || cfg.servers.len() > 1;

        if list_form {
            let in_file = |host: &str| {
                existing.as_ref().map(Config::from_value).is_some_and(|old| {
                    old.servers.iter().any(|s| s.host.eq_ignore_ascii_case(host) && !s.password.is_empty())
                })
            };

            let servers: Vec<Value> = cfg
                .servers
                .iter()
                .map(|s| {
                    let plain = in_file(&s.host) || !keyring::set(&s.username, &s.password);
                    private |= plain && !s.password.is_empty();
                    s.to_value(plain)
                })
                .collect();

            for key in ["host", "username", "password", "port", "ssl"] {
                out.remove(key);
            }
            out.insert("usenet_servers".into(), Value::Array(servers));
            private |= out.get("password").is_some();
        } else {
            let server = cfg.servers.first().cloned().unwrap_or_else(|| UsenetServer::new("", "", "", 563));
            out.insert("host".into(), json!(server.host));
            out.insert("username".into(), json!(server.username));
            out.insert("port".into(), json!(server.port));

            if keyring::set(&server.username, &server.password) {
                out.remove("password");
                private = false;
            } else {
                out.insert("password".into(), json!(server.password));
                private = true;
            }
        }
    } else if out.get("usenet_servers").is_some() {
        private = true;
    }

    let api_port = cfg.api_port.or_else(|| out.get("api_port").and_then(as_port));
    let api_key = out
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .map(String::from)
        .or_else(|| cfg.api_key.clone());

    out.insert("group".into(), json!(cfg.group));
    out.insert("groups".into(), json!(cfg.groups.iter().filter(|g| !g.is_empty()).collect::<Vec<_>>()));
    out.insert("index_mode".into(), json!(cfg.index_mode));

    if let Some(port) = api_port {
        out.insert("api_port".into(), json!(port));
    }

    if let Some(key) = api_key {
        out.insert("api_key".into(), json!(key));
    }

    if let Some(n) = cfg.split_min_backlog {
        out.insert("split_min_backlog".into(), json!(n));
    }

    if cfg.auto_run_compact {
        out.insert("auto_run_compact".into(), json!(true));
    }

    write_json(&Value::Object(out), private)
}

/// Returns the api key, generating and persisting one the first time.
/// The bool is true when a new key was made.
pub fn get_api_key(cfg: &mut Config) -> (String, bool) {
    if let Some(key) = &cfg.api_key {
        return (key.clone(), false);
    }

    let key = token_urlsafe(32);
    cfg.api_key = Some(key.clone());

    let mut saved = match read_file() {
        Some(Value::Object(map)) => map,
        Some(_) => return (key, true),
        None if config_file().exists() => return (key, true),
        None => Map::new(),
    };

    let has_password = saved.contains_key("password") || saved.contains_key("usenet_servers");
    saved.insert("api_key".into(), json!(key));
    let _ = write_json(&Value::Object(saved), has_password);

    (key, true)
}

fn write_json(value: &Value, private: bool) -> io::Result<()> {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(value, &mut ser).map_err(io::Error::other)?;

    let path = config_file();
    write_atomic(&path, &buf)?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }

    #[cfg(not(unix))]
    let _ = private;

    Ok(())
}

/// Same shape as python's secrets.token_urlsafe
pub fn token_urlsafe(nbytes: usize) -> String {
    use base64::Engine;

    let mut bytes = vec![0u8; nbytes];
    getrandom::fill(&mut bytes).expect("os rng unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub mod keyring {
    use super::SERVICE;

    fn disabled() -> bool {
        std::env::var_os("ATLAS_NO_KEYRING").is_some()
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn get(username: &str) -> Option<String> {
        if disabled() || username.is_empty() {
            return None;
        }

        ::keyring::Entry::new(SERVICE, username).and_then(|e| e.get_password()).ok().filter(|p| !p.is_empty())
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn set(username: &str, password: &str) -> bool {
        if disabled() || username.is_empty() {
            return false;
        }

        ::keyring::Entry::new(SERVICE, username).and_then(|e| e.set_password(password)).is_ok()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    pub fn get(_username: &str) -> Option<String> {
        let _ = (SERVICE, disabled());
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    pub fn set(_username: &str, _password: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_43_chars_urlsafe() {
        let t = token_urlsafe(32);
        assert_eq!(t.len(), 43);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn from_value_is_lenient() {
        let v = json!({"host": "h", "username": "u", "port": "119", "group": "a.b", "api_port": 8000});
        let cfg = Config::from_value(&v);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].port, 119);
        assert!(!cfg.servers[0].use_ssl());
        assert_eq!(cfg.groups, vec!["a.b".to_string()]);
        assert_eq!(cfg.api_port(), 8000);
        assert_eq!(cfg.api_host, "127.0.0.1");
        assert_eq!(cfg.index_mode, "dynamic");
        assert_eq!(cfg.max_unsaved_headers(), DEFAULT_MAX_UNSAVED_HEADERS as usize);
    }

    #[test]
    fn an_invalid_config_says_where_without_its_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert_eq!(file_problem_at(&path), None, "missing isnt a problem: setup offers to make one");

        // a trailing comma after the last server
        std::fs::write(&path, "{\n  \"usenet_servers\": [{\"host\": \"news.x\", \"password\": \"hunter2\"},]\n}\n")
            .unwrap();
        let problem = file_problem_at(&path).expect("a trailing comma is a problem");
        assert!(problem.contains(&path.display().to_string()), "{problem}");
        assert!(problem.contains("trailing comma at line 2 column"), "{problem}");
        assert!(!problem.contains("hunter2") && !problem.contains("news.x"), "no file contents: {problem}");

        std::fs::write(&path, "{\"groups\": []}").unwrap();
        assert_eq!(file_problem_at(&path), None);
    }

    #[test]
    fn valid_json_that_isnt_an_object_is_a_problem_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // setup would save over any of these, as if there were no config
        for (text, kind) in [("null", "null"), ("[]", "an array"), ("\"hunter2\"", "a string"), ("42", "a number")] {
            std::fs::write(&path, text).unwrap();
            let problem = file_problem_at(&path).unwrap_or_else(|| panic!("{text} isnt a config"));
            assert!(problem.contains(&path.display().to_string()), "{problem}");
            assert!(problem.contains(&format!("{kind}, not an object")), "{problem}");
            assert!(!problem.contains("hunter2"), "no file contents: {problem}");
        }
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(file_problem_at(&path), None, "an empty object is still a config");
    }

    #[test]
    fn max_unsaved_headers_from_the_file() {
        let cfg = |v: Value| Config::from_value(&v).max_unsaved_headers();
        assert_eq!(cfg(json!({"max_unsaved_headers": 2_000_000})), 2_000_000);
        assert_eq!(cfg(json!({"max_unsaved_headers": "40000"})), 40_000);
        // nonsense falls back to the default
        assert_eq!(cfg(json!({"max_unsaved_headers": 0})), DEFAULT_MAX_UNSAVED_HEADERS as usize);
        assert_eq!(cfg(json!({"max_unsaved_headers": -5})), DEFAULT_MAX_UNSAVED_HEADERS as usize);
    }

    /// A cap past what the pool can hold reads as what the pool holds, soo
    /// the config reload (which compares the two) doesnt see a change every
    /// time it looks.
    #[test]
    fn a_max_unsaved_headers_past_the_pools_limit_reads_as_the_pools_cap() {
        let cfg = Config::from_value(&json!({"max_unsaved_headers": 1u64 << 62}));
        let pool = crate::nntp::Pool::new(&[]).with_max_unsaved(cfg.max_unsaved_headers());
        assert_eq!(cfg.max_unsaved_headers(), pool.max_unsaved());
    }

    #[test]
    fn multiple_servers_sorted_by_priority() {
        let v = json!({
            "usenet_servers": [
                {"host": "d.example", "username": "u", "password": "p", "port": 563, "ssl": true, "connections": 10, "priority": 4},
                {"host": "b.example", "username": "u", "password": "p", "port": 563, "ssl": true, "connections": 10, "priority": 2},
                {"host": "a.example", "username": "u", "password": "p", "port": 563, "ssl": true, "priority": 1},
                {"host": "e.example", "username": "u", "password": "p", "port": "119", "ssl": "false", "priority": 4},
                {"host": "", "username": "skipped"},
                {"host": "z.example", "username": "u", "password": "p"}
            ],
            "host": "ignored.legacy",
            "group": "alt.binaries.x"
        });
        let cfg = Config::from_value(&v);
        let hosts: Vec<_> = cfg.servers.iter().map(|s| s.host.as_str()).collect();
        // ties keep file order, missing priority goes last
        assert_eq!(hosts, vec!["a.example", "b.example", "d.example", "e.example", "z.example"]);
        assert_eq!(cfg.servers[1].connections(), 10);
        assert_eq!(cfg.servers[0].connections(), DEFAULT_CONNECTIONS);
        assert!(!cfg.servers[3].use_ssl());
        assert_eq!(cfg.servers[3].port, 119);
        assert_eq!(cfg.servers_label(), "a.example (+4 fallback)");
    }

    #[test]
    fn workers_follow_connections() {
        let auto = Config::default();
        assert_eq!(auto.workers_per_server(&[25, 25, 25, 25]), vec![5, 5, 5, 5]);
        assert_eq!(auto.workers_per_server(&[10, 3]), vec![2, 1], "at least one each");

        let fixed = Config { parallel_groups: Some(20), ..Config::default() };
        assert_eq!(fixed.workers_per_server(&[25, 25, 25, 25]), vec![5, 5, 5, 5]);
        assert_eq!(fixed.workers_per_server(&[10, 30]), vec![5, 15]);
        assert_eq!(fixed.workers_per_server(&[1, 1, 1]).iter().sum::<usize>(), 20);

        let tiny = Config { parallel_groups: Some(1), ..Config::default() };
        assert_eq!(tiny.workers_per_server(&[10, 10]), vec![1, 1], "every server gets one");
    }

    #[test]
    fn index_flag_round_trips() {
        let v = json!({"usenet_servers": [
            {"host": "a", "password": "p", "connections": 30},
            {"host": "b", "password": "p", "index": false}
        ]});
        let cfg = Config::from_value(&v);
        assert_eq!(cfg.servers[0].connections(), 30);
        assert!(cfg.servers[0].indexes());
        assert!(!cfg.servers[1].indexes());
        assert!(cfg.servers[0].to_value(true).get("index").is_none());
        assert!(cfg.servers[1].to_value(true)["index"] == false);
    }
}
