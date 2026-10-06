//! Newznab compatible api for prowlarr / sonarr / radarr.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use tiny_http::{Header, Request, Response, Server};

use crate::config::{Config, get_api_key};
use crate::dates::pub_date;
use crate::nzb::{build_nzb, nzb_filename};
use crate::search::{all_releases, get_release, search_all_releases};
use crate::ui;

const WORKERS: usize = 4;

const CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server version="1.0" title="Atlas" strapline="Atlas Usenet Indexer" url="http://127.0.0.1:9090" email="" image=""/>
  <limits max="100" default="100"/>
  <retention days="0"/>
  <registration available="no" open="no"/>
  <searching>
    <search available="yes" supportedParams="q"/>
    <tv-search available="yes" supportedParams="q,season,ep"/>
    <movie-search available="yes" supportedParams="q"/>
    <audio-search available="yes" supportedParams="q"/>
    <book-search available="yes" supportedParams="q"/>
  </searching>
  <categories>
    <category id="7000" name="Other"/>
  </categories>
</caps>"#;

const XML: &str = "application/xml; charset=utf-8";

#[derive(Debug, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl ApiResponse {
    fn xml(status: u16, body: String) -> Self {
        ApiResponse { status, content_type: Some(XML), body, headers: Vec::new() }
    }

    fn not_found() -> Self {
        ApiResponse { status: 404, content_type: None, body: String::new(), headers: Vec::new() }
    }

    /// newznab error, sent with http 200 like real indexers do
    fn error(code: u16, description: &str) -> Self {
        ApiResponse::xml(
            200,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<error code=\"{code}\" description=\"{description}\"/>"
            ),
        )
    }
}

struct Running {
    server: Arc<Server>,
    workers: Vec<JoinHandle<()>>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// first value wins, like flask's request.args.get
pub fn parse_query(query: &str) -> HashMap<String, String> {
    let mut args = HashMap::new();

    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        args.entry(percent_decode(k)).or_insert_with(|| percent_decode(v));
    }

    args
}

/// Handle `/api?{query}`. `base` is the url root without trailing slash.
pub fn handle(path: &str, query: &str, base: &str, api_key: &str) -> ApiResponse {
    // a browser pointed at the bare address gets told where the api is,
    // instead of a blank 404 that looks like nothing is listening
    if path == "/" || path.is_empty() {
        return ApiResponse {
            status: 200,
            content_type: Some("text/plain; charset=utf-8"),
            body: format!(
                "Atlas newznab api is running.\n\nAdd it to Prowlarr / Sonarr / Radarr as a Newznab indexer:\n  URL:      {base}\n  API path: /api\n  API key:  the api_key in config.json\n\nCheck it: {base}/api?t=caps\n"
            ),
            headers: Vec::new(),
        };
    }

    if path != "/api" {
        return ApiResponse::not_found();
    }

    let args = parse_query(query);
    let t = args.get("t").map(String::as_str);

    if t == Some("caps") {
        // advertise the address the client actually used
        return ApiResponse::xml(200, CAPS.replace("http://127.0.0.1:9090", &escape(base)));
    }

    if args.get("apikey").map(String::as_str) != Some(api_key) {
        return ApiResponse::xml(
            401,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<error code=\"100\" description=\"Invalid API Key\"/>".into(),
        );
    }

    match t {
        Some("get") => get(&args),
        Some(kind @ ("search" | "tvsearch" | "movie" | "music" | "audio" | "book")) => match search_term(kind, &args) {
            Some(term) => search(&term, &args, base, api_key),
            // only an id (tvdbid, imdbid, ...): atlas has no metadata to match ids against
            None => rss(base, api_key, 0, &[]),
        },
        None => ApiResponse::error(200, "Missing parameter (t)"),
        Some(_) => ApiResponse::error(203, "Function not available"),
    }
}

/// id parameters atlas cant match (it only knows release names)
const ID_PARAMS: [&str; 8] = ["tvdbid", "rid", "tvrageid", "tvmazeid", "imdbid", "tmdbid", "traktid", "doubanid"];

/// What to search for. `tvsearch` adds `S01E02` style season/episode terms.
/// None when the request only has ids and nothing to search by name.
pub fn search_term(kind: &str, args: &HashMap<String, String>) -> Option<String> {
    let mut term = args.get("q").map(|q| q.trim().to_string()).unwrap_or_default();

    if kind == "tvsearch" {
        let num = |k: &str| args.get(k).and_then(|v| v.trim().parse::<u32>().ok());
        match (num("season"), num("ep")) {
            (Some(s), Some(e)) => term = format!("{term} S{s:02}E{e:02}"),
            (Some(s), None) => term = format!("{term} S{s:02}"),
            _ => {}
        }
        term = term.trim().to_string();
    }

    let has_id = ID_PARAMS.iter().any(|k| args.get(*k).is_some_and(|v| !v.trim().is_empty()));
    if has_id && args.get("q").is_none_or(|q| q.trim().is_empty()) {
        return None;
    }

    Some(term)
}

fn search(q: &str, args: &HashMap<String, String>, base: &str, api_key: &str) -> ApiResponse {
    let limit = args.get("limit").and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(100).clamp(1, 100);
    let offset = args.get("offset").and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0).max(0);

    let releases = if q.trim().is_empty() {
        all_releases(offset / limit, limit)
    } else {
        search_all_releases(q, offset / limit, limit)
    };

    match releases {
        Ok(r) => rss(base, api_key, offset, &r),
        Err(e) => {
            eprintln!("api search failed: {e}");
            ApiResponse::error(900, "Search failed")
        }
    }
}

fn rss(base: &str, api_key: &str, offset: i64, releases: &[crate::search::ReleaseRow]) -> ApiResponse {
    let items: String = releases
        .iter()
        .map(|r| {
            let nzb_url = format!("{base}/api?t=get&id={}&apikey={api_key}", r.id);
            let size = r.size.unwrap_or(0);
            format!(
                "    <item>      <title>{}</title>      <guid isPermaLink=\"false\">{}</guid>      <link>{}</link>      <size>{size}</size>      <pubDate>{}</pubDate>      <enclosure url=\"{}\" type=\"application/x-nzb\" length=\"{size}\"/>      <newznab:attr name=\"category\" value=\"7000\"/>    </item>",
                escape(&r.name),
                r.id,
                escape(&nzb_url),
                pub_date(r.posted_date.as_deref().unwrap_or("")),
                escape(&nzb_url),
            )
        })
        .collect();

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"
     xmlns:atom="http://www.w3.org/2005/Atom"
     xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
    <channel>
        <title>Atlas</title>
        <description>Atlas search results</description>
        <link>{}/api</link>
        <language>en-gb</language>
        <newznab:response offset="{offset}" total="{}"/>
        {items}
    </channel>
</rss>"#,
        escape(base),
        offset + releases.len() as i64,
    );

    ApiResponse::xml(200, xml)
}

fn get(args: &HashMap<String, String>) -> ApiResponse {
    let release_id = args.get("id").and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0);

    let Ok(Some(release)) = get_release(release_id) else {
        return ApiResponse::not_found();
    };

    let Ok(Some(content)) = build_nzb(release_id) else {
        return ApiResponse::not_found();
    };

    let filename = nzb_filename(&release.name, Some(release_id)).replace(['"', '\\'], "_");

    ApiResponse {
        status: 200,
        content_type: Some("application/x-nzb"),
        body: content,
        headers: vec![("Content-Disposition".into(), format!("attachment; filename=\"{filename}\""))],
    }
}

fn respond(request: Request, api_key: &str, fallback_host: &str) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));

    let host = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_else(|| fallback_host.to_string());
    let base = format!("http://{host}");

    let is_get = matches!(request.method(), tiny_http::Method::Get | tiny_http::Method::Head);
    let resp = if is_get {
        handle(path, query, &base, api_key)
    } else {
        ApiResponse { status: 405, content_type: None, body: String::new(), headers: Vec::new() }
    };

    let mut response = Response::from_string(resp.body).with_status_code(resp.status);

    let mut headers = resp.headers;
    headers.push(("Content-Type".into(), resp.content_type.unwrap_or("text/html; charset=utf-8").into()));

    for (k, v) in headers {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            response.add_header(h);
        }
    }

    let _ = request.respond(response);
}

/// Start the api on a few worker threads. Returns false when it couldnt bind.
pub fn start(config: &mut Config) -> bool {
    let (api_key, created) = get_api_key(config);

    if created {
        ui::print(&format!("[yellow]api key: {api_key}[/yellow]"));
    }

    let port = config.api_port();
    let host = config.api_host.clone();
    let bind_host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.clone() };
    let addr = format!("{bind_host}:{port}");

    let server = match Server::http(&addr) {
        Ok(s) => Arc::new(s),
        Err(_) => {
            ui::print(&format!("[red]port {port} is already in use, api not started[/red]"));
            ui::print("[dim]change it under Settings -> Change api port[/dim]");
            return false;
        }
    };

    ui::print(&format!("[dim]newznab api on http://{addr} — apikey required for search/get[/dim]"));

    let workers = (0..WORKERS)
        .map(|_| {
            let server = server.clone();
            let key = api_key.clone();
            let fallback = addr.clone();
            thread::spawn(move || {
                for request in server.incoming_requests() {
                    respond(request, &key, &fallback);
                }
            })
        })
        .collect();

    *RUNNING.lock().unwrap() = Some(Running { server, workers });
    true
}

pub fn stop() {
    let Some(running) = RUNNING.lock().unwrap().take() else { return };

    for _ in &running.workers {
        running.server.unblock();
    }

    for w in running.workers {
        let _ = w.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        let a = parse_query("t=search&q=the+matrix%20reloaded&q=ignored&apikey=a%2Bb&bad=%zz");
        assert_eq!(a["q"], "the matrix reloaded");
        assert_eq!(a["apikey"], "a+b");
        assert_eq!(a["bad"], "%zz");
    }

    #[test]
    fn caps_needs_no_key_and_others_do() {
        let r = handle("/api", "t=caps", "http://x", "key");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("<caps>"));

        let r = handle("/api", "t=search&apikey=wrong", "http://x", "key");
        assert_eq!(r.status, 401);
        assert!(r.body.contains("Invalid API Key"));

        assert_eq!(handle("/nope", "t=caps", "http://x", "key").status, 404);

        let root = handle("/", "", "http://192.168.1.172:9091", "key");
        assert_eq!(root.status, 200);
        assert!(root.body.contains("http://192.168.1.172:9091/api?t=caps"));
        assert!(!root.body.contains("key\n"), "never print the api key");
        assert!(
            handle("/api", "t=caps", "http://192.168.1.172:9091", "key")
                .body
                .contains(r#"url="http://192.168.1.172:9091""#)
        );
        let unknown = handle("/api", "t=wat&apikey=key", "http://x", "key");
        assert_eq!(unknown.status, 200);
        assert!(unknown.body.contains(r#"<error code="203""#));
        assert!(handle("/api", "apikey=key", "http://x", "key").body.contains(r#"<error code="200""#));
    }

    #[test]
    fn search_terms_for_each_type() {
        let term = |kind: &str, q: &str| search_term(kind, &parse_query(q));

        assert_eq!(term("search", "q=the+matrix").as_deref(), Some("the matrix"));
        assert_eq!(term("tvsearch", "q=Some+Show&season=1&ep=2").as_deref(), Some("Some Show S01E02"));
        assert_eq!(term("tvsearch", "q=Some+Show&season=3").as_deref(), Some("Some Show S03"));
        assert_eq!(term("tvsearch", "season=1&ep=2").as_deref(), Some("S01E02"));
        assert_eq!(term("movie", "q=Heat").as_deref(), Some("Heat"));
        // ids alone cant be matched: an empty answer, not recent releases
        assert_eq!(term("tvsearch", "tvdbid=121361&season=1&ep=1"), None);
        assert_eq!(term("movie", "imdbid=0133093"), None);
        // an id next to a name searches the name
        assert_eq!(term("movie", "imdbid=0133093&q=The+Matrix").as_deref(), Some("The Matrix"));
        // no q at all = recent releases, like t=search
        assert_eq!(term("music", "").as_deref(), Some(""));
    }

    #[test]
    fn caps_lists_every_search_type() {
        let caps = handle("/api", "t=caps", "http://x", "key").body;
        for kind in ["<search ", "<tv-search ", "<movie-search ", "<audio-search ", "<book-search "] {
            assert!(caps.contains(&format!(r#"{kind}available="yes""#)), "{kind} in {caps}");
        }
    }
}
