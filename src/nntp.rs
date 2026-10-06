//! Async NNTP client (tokio): just what atlas needs (GROUP, XOVER, BODY, LIST),
//! with a connection pool per usenet server soo many requests are in flight at once.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use regex::Regex;

use crate::config::UsenetServer;
use crate::dates::to_iso_date;
use crate::parser::Article;

#[derive(Debug)]
pub enum NntpError {
    Io(io::Error),
    /// server answered with an unexpected status code
    Reply {
        code: u16,
        message: String,
    },
    Protocol(String),
    /// a `[COMPRESS=GZIP]` response that wouldnt inflate
    Decompress(String),
}

impl NntpError {
    pub fn code(&self) -> Option<u16> {
        match self {
            NntpError::Reply { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// an XOVER range with no articles in it: 423, or 420 ("No Articles
    /// Selected") from some providers
    pub fn is_empty_range(&self) -> bool {
        matches!(self.code(), Some(420 | 423))
    }

    /// 4xx
    pub fn is_temporary(&self) -> bool {
        self.code().is_some_and(|c| (400..500).contains(&c))
    }

    /// 5xx
    pub fn is_permanent(&self) -> bool {
        self.code().is_some_and(|c| (500..600).contains(&c))
    }

    /// io/protocol errors leave the stream in an unknown state, the
    /// connection cant be reused after one
    pub fn breaks_connection(&self) -> bool {
        !matches!(self, NntpError::Reply { .. })
    }
}

impl fmt::Display for NntpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NntpError::Io(e) => write!(f, "{e}"),
            NntpError::Reply { code, message } => write!(f, "{code} {message}"),
            NntpError::Protocol(m) => write!(f, "protocol error: {m}"),
            NntpError::Decompress(m) => write!(f, "compressed headers: {m}"),
        }
    }
}

impl std::error::Error for NntpError {}

impl From<io::Error> for NntpError {
    fn from(e: io::Error) -> Self {
        NntpError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, NntpError>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overview {
    pub number: u64,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub message_id: String,
    pub references: String,
    pub bytes: i64,
    pub lines: i64,
}

impl Overview {
    pub fn into_article(self) -> Article {
        Article {
            number: self.number,
            subject: self.subject,
            author: self.from,
            date: to_iso_date(&self.date),
            message_id: self.message_id,
            references: self.references,
            bytes: self.bytes,
            lines: self.lines,
            ..Default::default()
        }
    }
}

pub fn headers_to_articles(headers: Vec<Overview>) -> Vec<Article> {
    headers.into_iter().map(Overview::into_article).collect()
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// smallest XOVER slice handed to one connection
const MIN_CHUNK: u64 = 250;
/// a server whose login was rejected is left alone this long
const AUTH_REST: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// provider wont take another connection
    TooMany,
    /// wrong username / password
    Auth,
    Other,
}

/// Why a connect/login failed, from the reply text (codes overlap: some
/// providers use 481/502 for both a bad login and the connection limit).
fn refusal(e: &NntpError) -> Refusal {
    let NntpError::Reply { message, .. } = e else { return Refusal::Other };
    let m = message.to_lowercase();

    if ["too many", "connection limit", "number of connections", "max connections", "maximum connections"]
        .iter()
        .any(|k| m.contains(k))
    {
        Refusal::TooMany
    } else if ["auth", "denied", "login", "password", "credential", "not authorized", "unauthorized"]
        .iter()
        .any(|k| m.contains(k))
    {
        Refusal::Auth
    } else {
        Refusal::Other
    }
}

/// how long to wait for the end of a compressed stream once its last line arrived
const STREAM_END_WAIT: Duration = Duration::from_secs(2);
/// after a failed connect a server is skipped for this long (connect() still tries it)
const DOWN_FOR: Duration = Duration::from_secs(60);
/// GROUPs refused with 500/501 in a row before a server counts as article only
const GROUP_REFUSALS: usize = 3;
/// how often a request in flight looks at the stop flag
const STOP_POLL: Duration = Duration::from_millis(50);
/// fetched slices a pass lets wait for it to save them, besides the ones its
/// connections hold (the unsaved budget bounds them all, this keeps one pass
/// from taking a big share of it)
const SLICES_BUFFERED: usize = 4;

/// `fut`'s output, or None as soon as `stop` is set: stopping doesnt wait on
/// a provider that went quiet mid reply. Dropping a request part way is safe,
/// its lease throws the connection away instead of reusing it.
pub async fn unless_stopped<F: Future>(stop: &AtomicBool, fut: F) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let mut tick = tokio::time::interval(STOP_POLL);

    std::future::poll_fn(|cx| {
        // a reply that made it is kept even when stop came in at the same time
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        if stop.load(Ordering::Relaxed) {
            return Poll::Ready(None);
        }
        while tick.poll_tick(cx).is_ready() {}
        Poll::Pending
    })
    .await
}

fn timed_out() -> NntpError {
    NntpError::Io(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
}

async fn with_timeout<T, F>(limit: Duration, fut: F) -> Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r.map_err(NntpError::Io),
        Err(_) => Err(timed_out()),
    }
}

/// What a date search keeps of an XOVER listing: its first and its last
/// article with a post date, as (number, post time).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ends {
    pub first: Option<Dated>,
    pub last: Option<Dated>,
    /// rows read, and the highest number among them
    pub rows: usize,
    pub read_to: Option<u64>,
    /// every row was read (not cut short at the row limit)
    pub whole: bool,
}

impl Ends {
    /// nothing in the range
    fn empty() -> Ends {
        Ends { whole: true, ..Ends::default() }
    }

    /// fold in one overview row
    fn add(&mut self, row: &[u8]) {
        self.rows += 1;
        let mut fields = row.split(|b| *b == b'\t');
        let Some(number) = fields.next().and_then(|f| std::str::from_utf8(f).ok()?.trim().parse::<u64>().ok()) else {
            return;
        };
        self.read_to = self.read_to.max(Some(number));
        let Some(t) = fields.nth(2).and_then(|d| plausible_post_time(&String::from_utf8_lossy(d))) else {
            return;
        };
        if self.first.is_none_or(|f| number < f.0) {
            self.first = Some((number, t));
        }
        if self.last.is_none_or(|l| number > l.0) {
            self.last = Some((number, t));
        }
    }
}

/// One logged in connection to one server.
pub struct Conn {
    io: BufReader<Box<dyn AsyncStream>>,
    timeout: Duration,
    /// group selected on this connection, XOVER needs one
    pub group: Option<String>,
    /// server agreed to XFEATURE COMPRESS GZIP on this connection
    pub compressed: bool,
    /// bytes received off the socket, and the same after decompression,
    /// since the pool last collected them
    wire: u64,
    text: u64,
}

impl Conn {
    /// Connect and log in. With `compress` it also asks for gzip compressed
    /// header listings (XFEATURE COMPRESS GZIP), which servers may refuse.
    pub async fn open(server: &UsenetServer, timeout: Duration, compress: bool) -> Result<Conn> {
        let host = server.host.trim().trim_matches(['[', ']']).to_string();
        let tcp = with_timeout(timeout, TcpStream::connect((host.as_str(), server.port))).await?;
        let _ = tcp.set_nodelay(true);

        let stream: Box<dyn AsyncStream> = if server.use_ssl() {
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| NntpError::Protocol(format!("bad host name: {e}")))?;
            let connector = tokio_rustls::TlsConnector::from(tls_config());
            Box::new(with_timeout(timeout, connector.connect(name, tcp)).await?)
        } else {
            Box::new(tcp)
        };

        let mut conn = Conn::over(stream, timeout);

        let (code, message) = conn.read_status().await?;
        if code != 200 && code != 201 {
            return Err(NntpError::Reply { code, message });
        }

        if !server.username.is_empty() {
            let (code, message) = conn.command("AUTHINFO USER", Some(&server.username)).await?;
            let (code, message) = match code {
                381 => conn.command("AUTHINFO PASS", Some(&server.password)).await?,
                _ => (code, message),
            };

            if code != 281 {
                return Err(NntpError::Reply { code, message });
            }
        }

        if compress {
            let (code, _) = conn.command("XFEATURE COMPRESS GZIP", None).await?;
            conn.compressed = code == 290;
        }

        Ok(conn)
    }

    fn over(stream: Box<dyn AsyncStream>, timeout: Duration) -> Conn {
        Conn {
            io: BufReader::with_capacity(64 * 1024, stream),
            timeout,
            group: None,
            compressed: false,
            wire: 0,
            text: 0,
        }
    }

    /// Next chunk of raw bytes from the socket.
    async fn read_chunk(&mut self, limit: Duration) -> Result<Vec<u8>> {
        let io = &mut self.io;
        let chunk = with_timeout(limit, async move { Ok(io.fill_buf().await?.to_vec()) }).await?;

        if chunk.is_empty() {
            return Err(NntpError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed")));
        }

        self.io.consume(chunk.len());
        self.wire += chunk.len() as u64;
        Ok(chunk)
    }

    /// Body of a `[COMPRESS=GZIP]` multi-line response: a zlib (or gzip)
    /// stream holding the usual dot terminated, dot stuffed lines, handed to
    /// `line` one at a time (see `each_line`).
    async fn each_compressed_line(&mut self, mut line: impl FnMut(Vec<u8>) -> bool) -> Result<bool> {
        let mut inflater = Inflater::default();
        // inflated text not handed out yet: lines go as soon as they are
        // whole, soo it holds about one read's worth
        let mut text = Vec::new();
        let mut terminated = false;

        loop {
            if !terminated {
                let mut scanned = 0;
                while let Some(nl) = text[scanned..].iter().position(|b| *b == b'\n') {
                    let mut l = text[scanned..scanned + nl].to_vec();
                    scanned += nl + 1;

                    if l.last() == Some(&b'\r') {
                        l.pop();
                    }

                    if l == b"." {
                        terminated = true;
                        break;
                    }

                    if l.starts_with(b"..") {
                        l.remove(0);
                    }

                    self.text += l.len() as u64 + 2;
                    if !line(l) {
                        return Ok(false);
                    }
                }
                text.drain(..scanned);
            }

            // done once the terminator showed up and the compressed stream is fully read,
            // soo no stray trailer bytes are left for the next command to trip over
            if terminated && inflater.finished() {
                return Ok(true);
            }

            let chunk = if terminated {
                // all lines are in, only the end of the compressed stream is missing.
                // a server that never closes the stream properly sends nothing more
                match self.read_chunk(STREAM_END_WAIT).await {
                    Err(NntpError::Io(e)) if e.kind() == io::ErrorKind::TimedOut => return Ok(true),
                    r => r?,
                }
            } else {
                self.read_chunk(self.timeout).await?
            };
            let plain = inflater.feed(&chunk, &mut text)?;

            // TERMINATOR style servers send ".\r\n" uncompressed after the stream
            if !terminated {
                text.extend_from_slice(&plain);
            }
        }
    }

    async fn send_line(&mut self, line: &str) -> Result<()> {
        if line.contains(['\r', '\n']) {
            return Err(NntpError::Protocol("newline in command".into()));
        }

        let mut buf = Vec::with_capacity(line.len() + 2);
        buf.extend_from_slice(line.as_bytes());
        buf.extend_from_slice(b"\r\n");

        let s = self.io.get_mut();
        with_timeout(self.timeout, async {
            s.write_all(&buf).await?;
            s.flush().await
        })
        .await
    }

    async fn read_raw_line(&mut self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let n = with_timeout(self.timeout, self.io.read_until(b'\n', &mut buf)).await?;
        self.wire += n as u64;
        self.text += n as u64;

        if n == 0 {
            return Err(NntpError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed")));
        }

        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }

        Ok(buf)
    }

    async fn read_status(&mut self) -> Result<(u16, String)> {
        let line = self.read_raw_line().await?;
        let line = String::from_utf8_lossy(&line);
        let (code, message) = line.split_once(' ').unwrap_or((&line, ""));

        let code = code.parse::<u16>().map_err(|_| NntpError::Protocol(format!("bad status line: {line}")))?;

        Ok((code, message.to_string()))
    }

    /// A plain multi-line response, handed to `line` one at a time with
    /// dot-stuffing removed (see `each_line`).
    async fn each_plain_line(&mut self, mut line: impl FnMut(Vec<u8>) -> bool) -> Result<bool> {
        loop {
            let mut l = self.read_raw_line().await?;

            if l == b"." {
                return Ok(true);
            }

            if l.starts_with(b"..") {
                l.remove(0);
            }

            if !line(l) {
                return Ok(false);
            }
        }
    }

    pub async fn command(&mut self, verb: &str, args: Option<&str>) -> Result<(u16, String)> {
        match args {
            Some(a) if !a.is_empty() => self.send_line(&format!("{verb} {a}")).await?,
            _ => self.send_line(verb).await?,
        }

        self.read_status().await
    }

    pub async fn quit(&mut self) {
        if self.send_line("QUIT").await.is_ok() {
            let _ = self.read_status().await;
        }
    }

    /// GROUP -> (count, first, last, name)
    pub async fn select_group(&mut self, group: &str) -> Result<(u64, u64, u64, String)> {
        let (code, message) = self.command("GROUP", Some(group)).await?;

        if code != 211 {
            self.group = None;
            return Err(NntpError::Reply { code, message });
        }

        self.group = Some(group.to_string());

        let parts: Vec<&str> = message.split_whitespace().collect();
        // numbers aint always there soo dont crash on em
        let num = |i: usize| parts.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let name = parts.get(3).map(|s| s.to_string()).unwrap_or_else(|| group.to_string());

        Ok((num(0), num(1), num(2), name))
    }

    /// XOVER pulls a whole header range in one shot
    pub async fn xover(&mut self, start: u64, end: u64) -> Result<Vec<Overview>> {
        let (code, message) = self.command("XOVER", Some(&format!("{start}-{end}"))).await?;

        if code != 224 {
            return Err(NntpError::Reply { code, message });
        }

        let lines = self.read_response_body(&message).await?;

        Ok(lines.iter().filter_map(|l| parse_overview(l)).collect())
    }

    /// BODY, yEnc decoded when the article is yEnc.
    pub async fn body(&mut self, message_id: &str) -> Result<Vec<u8>> {
        let (code, message) = self.command("BODY", Some(message_id)).await?;

        if code != 222 {
            return Err(NntpError::Reply { code, message });
        }

        let lines = self.read_response_body(&message).await?;
        Ok(yenc_decode(&lines).unwrap_or_else(|| lines.join(&b'\n')))
    }

    /// Lines of a multi-line response. With XFEATURE COMPRESS GZIP on, servers
    /// mark any listing they compressed (XOVER, LIST, ...) with `[COMPRESS=GZIP]`
    /// on the status line.
    async fn read_response_body(&mut self, status_message: &str) -> Result<Vec<Vec<u8>>> {
        let mut lines = Vec::new();
        self.each_line(status_message, |l| {
            lines.push(l);
            true
        })
        .await?;
        Ok(lines)
    }

    /// The lines of a multi-line response handed to `line` one at a time
    /// until it returns false. True when the response was read to its end;
    /// false when `line` stopped it: the rest is left unread, soo the
    /// connection cant take another request.
    async fn each_line(&mut self, status_message: &str, line: impl FnMut(Vec<u8>) -> bool) -> Result<bool> {
        if status_message.to_ascii_uppercase().contains("COMPRESS=GZIP") {
            self.each_compressed_line(line).await
        } else {
            self.each_plain_line(line).await
        }
    }

    /// XOVER `start..=end` read for its first and last article with a post
    /// date, `rows` rows at most: a listing longer than that is left unread
    /// past them (`Ends::whole` is false) and the connection cant take
    /// another request. Listings come sorted by number (RFC 3977), soo
    /// `first` is the range's first even then.
    pub async fn xover_ends(&mut self, start: u64, end: u64, rows: usize) -> Result<Ends> {
        let (code, message) = self.command("XOVER", Some(&format!("{start}-{end}"))).await?;

        if code != 224 {
            return Err(NntpError::Reply { code, message });
        }

        let mut ends = Ends::default();
        let whole = self
            .each_line(&message, |line| {
                if ends.rows == rows {
                    return false;
                }
                ends.add(&line);
                true
            })
            .await?;
        ends.whole = whole;
        Ok(ends)
    }

    async fn list_active(&mut self, pattern: Option<&str>) -> Result<Vec<Vec<u8>>> {
        let args = match pattern {
            Some(p) => format!("ACTIVE {p}"),
            None => "ACTIVE".to_string(),
        };

        let (code, message) = self.command("LIST", Some(&args)).await?;

        if code != 215 {
            return Err(NntpError::Reply { code, message });
        }

        self.read_response_body(&message).await
    }

    /// LIST ACTIVE -> (name, approx article count), biggest first. Empty groups dropped.
    pub async fn list_groups(&mut self, pattern: Option<&str>) -> Result<Vec<(String, u64)>> {
        let lines = match self.list_active(pattern).await {
            Ok(lines) => lines,
            Err(NntpError::Reply { .. }) if pattern.is_some() => self.list_active(None).await?,
            Err(e) => return Err(e),
        };

        let mut groups = Vec::new();

        for line in lines {
            let line = String::from_utf8_lossy(&line);
            let parts: Vec<&str> = line.split_whitespace().collect();

            if parts.len() < 3 {
                continue;
            }

            let (Ok(high), Ok(low)) = (parts[1].parse::<i64>(), parts[2].parse::<i64>()) else {
                continue;
            };

            if high - low <= 0 {
                continue;
            }

            if let Some(p) = pattern
                && !wildmatch(parts[0], p)
            {
                continue;
            }

            groups.push((parts[0].to_string(), (high - low) as u64));
        }

        groups.sort_by_key(|g| std::cmp::Reverse(g.1));
        Ok(groups)
    }
}

/// Streaming inflate for compressed header listings. Works out zlib vs gzip
/// vs raw deflate from the first bytes.
#[derive(Default)]
struct Inflater {
    state: InflateState,
    /// bytes held back until the stream header can be read
    head: Vec<u8>,
    /// gzip ends with an 8 byte crc/size trailer after the deflate data
    trailer_left: usize,
}

#[derive(Default)]
enum InflateState {
    #[default]
    Start,
    Running(Box<flate2::Decompress>, bool),
    Done,
}

impl Inflater {
    fn finished(&self) -> bool {
        matches!(self.state, InflateState::Done) && self.trailer_left == 0
    }

    /// Inflate `input` into `out`. Returns bytes that came after the end of
    /// the compressed stream (plain text).
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Vec<u8>> {
        let bad = |m: &str| NntpError::Decompress(m.to_string());

        if let InflateState::Start = self.state {
            self.head.extend_from_slice(input);
            let head = std::mem::take(&mut self.head);

            if head.len() < 2 {
                self.head = head;
                return Ok(Vec::new());
            }

            let (decompress, gzip, skip) = if head[0] == 0x1f && head[1] == 0x8b {
                match gzip_header_len(&head) {
                    None => {
                        self.head = head;
                        return Ok(Vec::new());
                    }
                    Some(Err(e)) => return Err(e),
                    Some(Ok(n)) => (flate2::Decompress::new(false), true, n),
                }
            } else if head[0] & 0x0f == 8 && (u16::from(head[0]) << 8 | u16::from(head[1])) % 31 == 0 {
                (flate2::Decompress::new(true), false, 0)
            } else {
                (flate2::Decompress::new(false), false, 0)
            };

            self.state = InflateState::Running(Box::new(decompress), gzip);
            return self.feed(&head[skip..], out);
        }

        let mut rest: &[u8] = input;

        if let InflateState::Running(d, gzip) = &mut self.state {
            let gzip = *gzip;
            // keep going while there is input left OR the last round filled the
            // output space: headers compress way better than 8:1 and whatever
            // didnt fit stays buffered inside the decompressor
            loop {
                out.reserve(rest.len().saturating_mul(8).max(256 * 1024));
                let (in_before, out_before) = (d.total_in(), out.len());
                let status = d
                    .decompress_vec(rest, out, flate2::FlushDecompress::None)
                    .map_err(|e| NntpError::Decompress(e.to_string()))?;
                rest = &rest[(d.total_in() - in_before) as usize..];
                let produced = out.len() - out_before;

                if status == flate2::Status::StreamEnd {
                    self.state = InflateState::Done;
                    self.trailer_left = if gzip { 8 } else { 0 };
                    break;
                }

                if produced == 0 {
                    if rest.is_empty() {
                        // needs more input
                        return Ok(Vec::new());
                    }
                    if d.total_in() == in_before {
                        return Err(bad("stuck"));
                    }
                }
            }
        }

        // past the end of the compressed stream
        let skip = self.trailer_left.min(rest.len());
        self.trailer_left -= skip;
        Ok(rest[skip..].to_vec())
    }
}

/// Size of a gzip member header. None = need more bytes.
fn gzip_header_len(b: &[u8]) -> Option<Result<usize>> {
    if b.len() < 10 {
        return None;
    }

    if b[2] != 8 {
        return Some(Err(NntpError::Decompress("unknown gzip method".into())));
    }

    let flags = b[3];
    let mut pos = 10;

    if flags & 0x04 != 0 {
        let xlen = u16::from_le_bytes([*b.get(pos)?, *b.get(pos + 1)?]) as usize;
        pos += 2 + xlen;
    }

    for bit in [0x08, 0x10] {
        if flags & bit != 0 {
            pos += b.get(pos..)?.iter().position(|c| *c == 0)? + 1;
        }
    }

    if flags & 0x02 != 0 {
        pos += 2;
    }

    (b.len() >= pos).then_some(Ok(pos))
}

/// One server's connections, capped at its `connections` setting.
struct Server {
    cfg: UsenetServer,
    /// what names this server in stored state, see `server_keys`
    key: String,
    /// what earlier builds named it, see `legacy_server_keys`
    legacy_keys: Vec<String>,
    idle: Mutex<Vec<Conn>>,
    permits: Arc<Semaphore>,
    down_until: Mutex<Option<Instant>>,
    /// set when the server's compressed headers couldnt be read
    no_compress: AtomicBool,
    /// server answered GROUP with 500/501: an article only server (fill / bonus),
    /// used for article lookups but not for indexing
    no_index: AtomicBool,
    /// GROUPs answered 500/501 in a row; a real server can reject one odd group name
    group_refusals: AtomicUsize,
    /// login rejected or limit shrunk messages already printed
    warned_auth: AtomicBool,
    warned_limit: AtomicBool,
    /// connections atlas currently allows itself (starts at `connections`,
    /// shrinks when the provider refuses more)
    limit: AtomicUsize,
    /// requests waiting for a free connection right now
    waiting: AtomicUsize,
    /// for the stats dashboard
    headers: AtomicU64,
    wire: AtomicU64,
    text: AtomicU64,
}

/// Counts a request waiting for a connection while it lives (a request
/// dropped while it waits is counted out too).
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn on(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Waiting(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Server {
    fn compress(&self) -> bool {
        self.cfg.compress.unwrap_or(true) && !self.no_compress.load(Ordering::Relaxed)
    }
}

/// A connection borrowed from a server. Goes back to the idle list on drop
/// unless an error left it unusable, or a request on it never finished.
struct Lease {
    conn: Option<Conn>,
    server: Arc<Server>,
    _permit: OwnedSemaphorePermit,
    /// a request was sent and its reply not fully read yet
    busy: bool,
}

impl Lease {
    fn new(conn: Conn, server: Arc<Server>, permit: OwnedSemaphorePermit) -> Lease {
        Lease { conn: Some(conn), server, _permit: permit, busy: false }
    }

    /// The connection, for one request: `check` its result to end it. A
    /// request dropped part way (stopping) never reaches `check`, soo the
    /// lease knows the reply is still on the wire and throws the connection away.
    fn begin(&mut self) -> &mut Conn {
        self.busy = true;
        self.conn.as_mut().expect("lease without a connection")
    }

    fn group(&self) -> Option<&str> {
        self.conn.as_ref().and_then(|c| c.group.as_deref())
    }

    /// end the request: drop the connection when `r` broke it
    fn check<T>(&mut self, r: Result<T>) -> Result<T> {
        self.busy = false;
        if r.as_ref().err().is_some_and(NntpError::breaks_connection) {
            self.collect_traffic();
            self.conn = None;
        }
        r
    }

    /// move the connection's byte counts onto its server
    fn collect_traffic(&mut self) {
        if let Some(c) = self.conn.as_mut() {
            self.server.wire.fetch_add(std::mem::take(&mut c.wire), Ordering::Relaxed);
            self.server.text.fetch_add(std::mem::take(&mut c.text), Ordering::Relaxed);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.collect_traffic();
        // half a reply left unread would be the next request's answer
        if let Some(conn) = self.conn.take().filter(|_| !self.busy) {
            self.server.idle.lock().unwrap().push(conn);
        }
    }
}

/// One server's numbers for the stats dashboard.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ServerStat {
    pub host: String,
    pub priority: i64,
    pub connections: u32,
    /// what atlas allows itself now (lower than `connections` when the provider refused more)
    pub limit: usize,
    pub in_use: usize,
    pub open: usize,
    pub indexing: bool,
    pub state: &'static str,
    pub headers: u64,
    pub wire_bytes: u64,
    pub text_bytes: u64,
}

/// an article number and when it was posted (unix seconds)
type Dated = (u64, i64);

/// A Date header as unix seconds when it could be true: from 2000 on and not
/// later than a little past now. Posters set their own Dates, soo a date search
/// passes over the others as undated rather than be thrown off by them.
fn plausible_post_time(date: &str) -> Option<i64> {
    /// 2000-01-01, and the slack for clocks running ahead
    const EARLIEST: i64 = 946_684_800;
    const AHEAD: i64 = 2 * 86_400;
    let t = crate::dates::posted_timestamp(date)?;
    (EARLIEST..=chrono::Utc::now().timestamp() + AHEAD).contains(&t).then_some(t)
}

/// windows of `DATE_LOOK` numbers from a server's first article that judge
/// how far back it keeps a group (`Pool::retention`)
const RETENTION_WINDOWS: u64 = 3;
/// Dates of a window passed over as forged before its earliest counts
const RETENTION_OUTLIERS: usize = 3;

/// How far back a server keeps a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// it has no articles of it
    Empty,
    /// from about this time (unix seconds)
    Since(i64),
    /// its first articles' dates disagree too much to tell (a forged run, or
    /// dates far out of order): from about this time, if they can be trusted
    Unsure(i64),
}

/// When a window of articles (`dates` sorted) starts: its earliest date not
/// more than a day before the one `RETENTION_OUTLIERS` places in, soo that
/// many forged far older Dates dont move it, nor does one forged later.
fn window_start(dates: &[i64]) -> i64 {
    let floor = dates[RETENTION_OUTLIERS.min(dates.len() - 1)] - 86_400;
    dates.iter().copied().find(|&d| d >= floor).unwrap_or(floor + 86_400)
}

/// A server's retention from the date each of its first windows counts by,
/// in number order, and the date of its first article (`first`,
/// when no window had a date to go by).
fn retention_of(windows: &[i64], first: i64) -> Retention {
    let since = windows.iter().copied().min().unwrap_or(first);
    let mut newest = i64::MIN;
    for &q in windows {
        if q < newest.saturating_sub(86_400) {
            return Retention::Unsure(since);
        }
        newest = newest.max(q);
    }
    Retention::Since(since)
}

/// numbers per date search request at first, and at most
const DATE_LOOK: u64 = 100;
const DATE_SCAN_MAX: u64 = DATE_LOOK << 6;
/// rows a date search reads of one listing at most: whatever span it asks
/// for (stepped over numbers can hide a crowd), it holds no more. A window
/// of `DATE_SCAN_MAX` numbers is always read whole
const DATE_ROWS: usize = DATE_SCAN_MAX as usize;

/// Several providers tried in priority order, each with up to `connections`
/// requests in flight.
///
/// GROUP/XOVER/LIST run on one "active" server (article numbers are per
/// server). `connect` picks the first server that works, and a group missing
/// on the active server is looked up on the others. BODY is fetched by
/// message-id, which is the same everywhere, soo a missing article is asked
/// of every server in priority order.
pub struct Pool {
    servers: Vec<Arc<Server>>,
    active: AtomicUsize,
    ready: AtomicBool,
    failed_over_at: Mutex<Option<Instant>>,
    timeout: Duration,
    /// groups their first choice server doesnt carry: the server that does
    homes: Mutex<HashMap<String, usize>>,
    unsaved: Arc<UnsavedBudget>,
}

/// Headers fetched (or on their way) and not saved yet, over every pass on a
/// pool: what bounds the indexer's memory. A slice takes its article numbers
/// from it before its XOVER goes out, gives back what didnt come (gaps, an
/// empty or failed slice) once the reply is in, and the rest once it's saved.
struct UnsavedBudget {
    permits: Arc<Semaphore>,
    cap: usize,
    /// held right now, and the most ever held at once
    now: AtomicUsize,
    peak: AtomicUsize,
}

/// The unsaved headers cap a pool takes for `headers`: at least one, at
/// most what its semaphore holds. Config reads through this too, soo what it
/// asks for compares equal to what the pool has.
pub fn unsaved_cap(headers: usize) -> usize {
    headers.clamp(1, Semaphore::MAX_PERMITS.min(u32::MAX as usize))
}

impl UnsavedBudget {
    fn new(cap: usize) -> Arc<Self> {
        let cap = unsaved_cap(cap);
        Arc::new(UnsavedBudget {
            permits: Arc::new(Semaphore::new(cap)),
            cap,
            now: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }
}

/// A slice's room in the unsaved budget, given back when dropped.
pub struct Unsaved {
    permit: OwnedSemaphorePermit,
    budget: Arc<UnsavedBudget>,
    /// headers held past the room (a slice bigger than the whole budget),
    /// counted soo `now` and `peak` show what is really held
    over: usize,
}

impl Unsaved {
    /// give back all but `headers`
    fn keep(&mut self, headers: usize) {
        let extra = self.permit.num_permits().saturating_sub(headers);
        if extra > 0
            && let Some(back) = self.permit.split(extra)
        {
            self.budget.now.fetch_sub(extra, Ordering::Relaxed);
            drop(back);
        }
        self.over = headers.saturating_sub(self.permit.num_permits());
        if self.over > 0 {
            let now = self.budget.now.fetch_add(self.over, Ordering::Relaxed) + self.over;
            self.budget.peak.fetch_max(now, Ordering::Relaxed);
        }
    }
}

impl Drop for Unsaved {
    fn drop(&mut self) {
        // counted down before the permit goes back (right after this), soo
        // `now` never shows more than is held
        self.budget.now.fetch_sub(self.permit.num_permits() + self.over, Ordering::Relaxed);
    }
}

/// What names each server in stored state (cursors, chunk claims, sweeps),
/// from its own settings alone: adding or removing another server never
/// changes it (that would orphan its cursors). The plain host on 563 with ssl
/// (what it always was for the usual server), else `host:port` (so a plain
/// 119 and an ssl 563 on one host are two servers), and `#key` after either when the server sets `key`. Two
/// accounts on the same host and port share it (the same provider numbers
/// articles the same) unless one sets a `key`. Never the user or password.
pub fn server_keys(servers: &[UsenetServer]) -> Vec<String> {
    servers.iter().map(server_key).collect()
}

fn server_key(s: &UsenetServer) -> String {
    let base = if s.port == 563 && s.use_ssl() { s.host.clone() } else { format!("{}:{}", s.host, s.port) };
    match &s.key {
        Some(k) => format!("{base}#{k}"),
        None => base,
    }
}

/// Per server, lowercased, what earlier builds named it in stored state
/// (`host`, `host:port`, `user@host:port`, depending on the other servers
/// then) and that no server is named now: cursors saved under one are its
/// own, adopted when none are saved under its key (see `indexer`)
pub fn legacy_server_keys(servers: &[UsenetServer]) -> Vec<Vec<String>> {
    let now: Vec<String> = servers.iter().map(|s| server_key(s).to_lowercase()).collect();
    servers
        .iter()
        .map(|s| {
            let host = s.host.to_lowercase();
            let hp = format!("{host}:{}", s.port);
            let user = format!("{}@{hp}", s.username.to_lowercase());
            let mut old = Vec::new();
            for k in [host, hp, user] {
                if !now.contains(&k) && !old.contains(&k) {
                    old.push(k);
                }
            }
            old
        })
        .collect()
}

impl Pool {
    pub fn new(servers: &[UsenetServer]) -> Pool {
        let keys = server_keys(servers);
        let legacy = legacy_server_keys(servers);
        Pool {
            servers: servers
                .iter()
                .zip(keys)
                .zip(legacy)
                .map(|((cfg, key), legacy_keys)| {
                    Arc::new(Server {
                        cfg: cfg.clone(),
                        key,
                        legacy_keys,
                        idle: Mutex::new(Vec::new()),
                        permits: Arc::new(Semaphore::new(cfg.connections().max(1) as usize)),
                        down_until: Mutex::new(None),
                        no_compress: AtomicBool::new(false),
                        no_index: AtomicBool::new(false),
                        group_refusals: AtomicUsize::new(0),
                        warned_auth: AtomicBool::new(false),
                        warned_limit: AtomicBool::new(false),
                        limit: AtomicUsize::new(cfg.connections().max(1) as usize),
                        waiting: AtomicUsize::new(0),
                        headers: AtomicU64::new(0),
                        wire: AtomicU64::new(0),
                        text: AtomicU64::new(0),
                    })
                })
                .collect(),
            active: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            failed_over_at: Mutex::new(None),
            timeout: DEFAULT_TIMEOUT,
            homes: Mutex::new(HashMap::new()),
            unsaved: UnsavedBudget::new(crate::config::DEFAULT_MAX_UNSAVED_HEADERS as usize),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// At most `headers` fetched and not saved yet, over every pass at once.
    pub fn with_max_unsaved(mut self, headers: usize) -> Self {
        self.unsaved = UnsavedBudget::new(headers);
        self
    }

    /// the cap on headers fetched and not saved yet
    pub fn max_unsaved(&self) -> usize {
        self.unsaved.cap
    }

    /// headers fetched (or being fetched) and not saved yet, right now
    pub fn unsaved_headers(&self) -> usize {
        self.unsaved.now.load(Ordering::Relaxed)
    }

    /// the most headers that were ever unsaved at once
    pub fn unsaved_peak(&self) -> usize {
        self.unsaved.peak.load(Ordering::Relaxed)
    }

    /// Room for a slice of `numbers` article numbers in the unsaved budget,
    /// waiting until there is. A slice bigger than the whole budget waits for
    /// all of it.
    async fn reserve_unsaved(&self, numbers: u64) -> Unsaved {
        let budget = self.unsaved.clone();
        let n = usize::try_from(numbers).unwrap_or(usize::MAX).clamp(1, budget.cap);
        let permit = budget.permits.clone().acquire_many_owned(n as u32).await.expect("semaphore closed");
        let now = budget.now.fetch_add(n, Ordering::Relaxed) + n;
        budget.peak.fetch_max(now, Ordering::Relaxed);
        Unsaved { permit, budget, over: 0 }
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn active_host(&self) -> String {
        self.servers.get(self.active_index()).map(|s| s.cfg.host.clone()).unwrap_or_default()
    }

    /// what server `i` was named in stored state before (see `legacy_server_keys`)
    pub fn legacy_keys(&self, i: usize) -> Vec<String> {
        self.servers.get(i).map(|s| s.legacy_keys.clone()).unwrap_or_default()
    }

    /// server `i`'s name in stored state (see `server_keys`)
    pub fn host(&self, i: usize) -> String {
        self.servers.get(i).map(|s| s.key.clone()).unwrap_or_default()
    }

    /// requests server `i` takes at once
    /// requests server `i` takes at once
    pub fn connections(&self, i: usize) -> usize {
        self.servers.get(i).map(|s| s.limit.load(Ordering::Relaxed)).unwrap_or(1)
    }

    fn is_down(&self, i: usize) -> bool {
        self.servers[i].down_until.lock().unwrap().is_some_and(|t| Instant::now() < t)
    }

    /// Servers that take part in indexing (`index` not turned off), in config
    /// order. Falls back to every server when all of them opted out.
    pub fn indexing_servers(&self) -> Vec<usize> {
        let enabled: Vec<usize> = (0..self.servers.len())
            .filter(|&i| self.servers[i].cfg.indexes() && !self.servers[i].no_index.load(Ordering::Relaxed))
            .collect();
        if enabled.is_empty() { (0..self.servers.len()).collect() } else { enabled }
    }

    /// Indexing servers usable right now: the ones that havent failed recently
    /// (all of them when every one is down).
    pub fn indexing_tier(&self) -> Vec<usize> {
        let all = self.indexing_servers();
        let up: Vec<usize> = all.iter().copied().filter(|&i| !self.is_down(i)).collect();
        if up.is_empty() { all } else { up }
    }

    /// Requests in flight across every indexing server.
    pub fn indexing_connections(&self) -> usize {
        self.indexing_servers().iter().map(|&i| self.connections(i)).sum::<usize>().max(1)
    }

    /// Server a group gets indexed on, see `pick_server_in`.
    pub fn pick_server(&self, group: &str) -> usize {
        self.pick_server_in(&self.indexing_tier(), group)
    }

    /// Every indexing server takes part at once: groups are spread over `tier`
    /// in proportion to each server's connections, and a group keeps its
    /// server (its cursors are per server) as long as that server is healthy.
    pub fn pick_server_in(&self, tier: &[usize], group: &str) -> usize {
        // a group its first choice doesnt carry lives where it was found
        if let Some(&home) = self.homes.lock().unwrap().get(group)
            && tier.contains(&home)
        {
            return home;
        }

        // a server that is down only moves its own groups: the others keep the
        // server they get with every indexing server up
        let first = self.spread_over(&self.indexing_servers(), group);
        if tier.contains(&first) {
            return first;
        }
        self.spread_over(tier, group)
    }

    /// `group`'s server among `tier`, by its hash weighted by connections.
    fn spread_over(&self, tier: &[usize], group: &str) -> usize {
        let total: u64 = tier.iter().map(|&i| self.connections(i) as u64).sum();
        if total == 0 {
            return tier.first().copied().unwrap_or(0);
        }

        // fnv-1a: stable across runs and rust versions
        let hash =
            group.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        let mut point = hash % total;

        for &i in tier {
            let c = self.connections(i) as u64;
            if point < c {
                return i;
            }
            point -= c;
        }
        tier[0]
    }

    /// `candidates` in the order a group should try them: rendezvous hashing
    /// weighted by connections, soo groups that need another server spread
    /// over all of them in proportion (not all onto the first in the list),
    /// and the same group always gets the same order.
    fn ranked(&self, candidates: &[usize], group: &str) -> Vec<usize> {
        let fnv = |bytes: &[u8]| {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
        };
        let mut scored: Vec<(f64, usize)> = candidates
            .iter()
            .map(|&i| {
                // by host (and an explicit `key`), not port: a group's home
                // stays where it was before ports were part of server keys
                let cfg = &self.servers[i].cfg;
                let key =
                    format!("{group}\0{}{}", cfg.host, cfg.key.as_ref().map(|k| format!("#{k}")).unwrap_or_default());
                // splitmix64's finalizer: fnv alone barely changes between hosts
                let mut z = fnv(key.as_bytes());
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^= z >> 31;
                // a well mixed number in (0, 1]
                let h = ((z >> 11) + 1) as f64 / (1u64 << 53) as f64;
                let weight = self.connections(i).max(1) as f64;
                (-(h.max(f64::MIN_POSITIVE)).ln() / weight, i)
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    /// requests the active server takes at once
    pub fn concurrency(&self) -> usize {
        self.servers.get(self.active_index()).map(|s| s.cfg.connections() as usize).unwrap_or(1)
    }

    pub fn is_connected(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn no_servers() -> NntpError {
        NntpError::Protocol("no usenet servers configured".into())
    }

    /// Borrow a connection to server `i`, opening one if none is idle.
    /// `force` ignores the "recently failed" mark.
    async fn lease(&self, i: usize, force: bool) -> Result<Lease> {
        self.lease_for(i, None, force).await
    }

    /// Like `lease`, but an idle connection that already has `group` selected
    /// is taken first (saves a GROUP round trip when many groups share a server).
    async fn lease_for(&self, i: usize, group: Option<&str>, force: bool) -> Result<Lease> {
        let server = self.servers.get(i).cloned().ok_or_else(Self::no_servers)?;

        loop {
            let waited = Instant::now();
            let permit = {
                let _waiting = Waiting::on(&server.waiting);
                server.permits.clone().acquire_owned().await.expect("semaphore closed")
            };
            crate::profile::Load::add_since(&crate::profile::LOAD.lease_wait_ns, waited);

            let idle = {
                let mut idle = server.idle.lock().unwrap();
                let on_group = group.and_then(|g| idle.iter().rposition(|c| c.group.as_deref() == Some(g)));
                match on_group {
                    Some(pos) => Some(idle.swap_remove(pos)),
                    None => idle.pop(),
                }
            };
            if let Some(conn) = idle {
                return Ok(Lease::new(conn, server, permit));
            }

            if !force && server.down_until.lock().unwrap().is_some_and(|t| Instant::now() < t) {
                return Err(NntpError::Io(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("{} failed recently, skipping it", server.cfg.host),
                )));
            }

            let e = match Conn::open(&server.cfg, self.timeout, server.compress()).await {
                Ok(conn) => {
                    *server.down_until.lock().unwrap() = None;
                    return Ok(Lease::new(conn, server, permit));
                }
                Err(e) => e,
            };

            let limit = server.limit.load(Ordering::Relaxed);
            let others_open = server.permits.available_permits() + 1 < limit;

            match refusal(&e) {
                // the provider allows fewer connections than configured (or another
                // app on the same account uses some): use one less from now on and
                // wait for a connection that is already open
                Refusal::TooMany if others_open && limit > 1 => {
                    permit.forget();
                    let now = server.limit.fetch_sub(1, Ordering::Relaxed) - 1;
                    if !server.warned_limit.swap(true, Ordering::Relaxed) {
                        println!("{}: provider refused more connections ({e}), lowering to {now}", server.cfg.host);
                    }
                    continue;
                }
                Refusal::Auth => {
                    *server.down_until.lock().unwrap() = Some(Instant::now() + AUTH_REST);
                    if !server.warned_auth.swap(true, Ordering::Relaxed) {
                        println!(
                            "{}: login rejected ({e}). not using it for {}m, check the username/password",
                            server.cfg.host,
                            AUTH_REST.as_secs() / 60
                        );
                    }
                    return Err(e);
                }
                _ => {
                    if !others_open {
                        *server.down_until.lock().unwrap() = Some(Instant::now() + DOWN_FOR);
                    }
                    return Err(e);
                }
            }
        }
    }

    fn switch_to(&self, i: usize) {
        let old = self.active.swap(i, Ordering::Relaxed);
        if old != i
            && let Some(s) = self.servers.get(old)
        {
            s.idle.lock().unwrap().clear();
        }
        *self.failed_over_at.lock().unwrap() = (i > 0).then(Instant::now);
    }

    /// Use the highest priority server that answers.
    pub async fn connect(&self) -> Result<()> {
        let mut last_err = Self::no_servers();

        for i in 0..self.servers.len() {
            match self.lease(i, true).await {
                Ok(_) => {
                    if i > 0 {
                        println!("using fallback server {}", self.servers[i].cfg.host);
                    }
                    self.switch_to(i);
                    self.ready.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                Err(e) => {
                    if self.servers.len() > 1 {
                        println!("couldnt connect to {}: {e}", self.servers[i].cfg.host);
                    }
                    last_err = e;
                }
            }
        }

        Err(last_err)
    }

    /// Drop every idle connection (without QUIT).
    pub fn disconnect(&self) {
        self.ready.store(false, Ordering::Relaxed);
        for s in &self.servers {
            s.idle.lock().unwrap().clear();
        }
    }

    /// QUIT and drop every idle connection.
    pub async fn close(&self) {
        self.ready.store(false, Ordering::Relaxed);
        for s in &self.servers {
            let conns: Vec<Conn> = std::mem::take(&mut *s.idle.lock().unwrap());
            for mut c in conns {
                c.quit().await;
            }
        }
    }

    /// Back to the top priority server once it has had `after` to recover.
    pub async fn restore_primary(&self, after: Duration) -> bool {
        let since = *self.failed_over_at.lock().unwrap();
        if self.active_index() == 0 || since.is_none_or(|t| t.elapsed() < after) {
            return false;
        }

        match self.lease(0, true).await {
            Ok(_) => {
                println!("back on primary server {}", self.servers[0].cfg.host);
                self.switch_to(0);
                true
            }
            Err(_) => {
                *self.failed_over_at.lock().unwrap() = Some(Instant::now());
                false
            }
        }
    }

    async fn select_on(&self, i: usize, group: &str) -> Result<(u64, u64, u64, String)> {
        let mut lease = self.lease_for(i, Some(group), false).await?;
        let r = lease.begin().select_group(group).await;

        // GROUP not supported at all: an article only (fill / bonus) server. one
        // refusal can be a group name the server doesnt like, soo it takes
        // GROUP_REFUSALS in a row with no GROUP working in between
        let server = &self.servers[i];
        match &r {
            Err(e) if matches!(e.code(), Some(500 | 501)) => {
                let refusals = server.group_refusals.fetch_add(1, Ordering::Relaxed) + 1;
                if refusals >= GROUP_REFUSALS && !server.no_index.swap(true, Ordering::Relaxed) {
                    println!("{}: doesnt support GROUP ({e}), using it for article lookups only", server.cfg.host);
                }
            }
            Ok(_) => server.group_refusals.store(0, Ordering::Relaxed),
            Err(_) => {}
        }

        lease.check(r)
    }

    /// GROUP on server `i` alone, for probing a server: no falling over to
    /// another server, soo where the group lives stays as it is.
    pub async fn group_on(&self, i: usize, group: &str) -> Result<(u64, u64, u64, String)> {
        self.select_on(i, group).await
    }

    /// GROUP on the active server, falling over to the next server that carries it
    /// (which then becomes the active one).
    pub async fn select_group(&self, group: &str) -> Result<(u64, u64, u64, String)> {
        let (i, info) = self.select_group_on(self.active_index(), group).await?;
        if i != self.active_index() {
            self.switch_to(i);
        }
        Ok(info)
    }

    /// GROUP on server `prefer`, falling over to an indexing server that
    /// carries it. Returns the server used.
    pub async fn select_group_on(&self, prefer: usize, group: &str) -> Result<(usize, (u64, u64, u64, String))> {
        let first_err = match self.select_on(prefer, group).await {
            Ok(r) => return Ok((prefer, r)),
            // 411 = this provider doesnt carry the group, 500/501 = no GROUP at all
            Err(e) if matches!(e.code(), Some(411 | 500 | 501)) => e,
            Err(e) => return Err(e),
        };

        // indexing servers only: one with `index: false` (a metered block
        // account) is for article lookups, never a group's home
        let indexing: Vec<usize> = self.indexing_servers().into_iter().filter(|&i| i != prefer).collect();
        for i in self.ranked(&indexing, group) {
            if let Ok(r) = self.select_on(i, group).await {
                println!("{group} not on {}, using {}", self.servers[prefer].cfg.host, self.servers[i].cfg.host);
                self.homes.lock().unwrap().insert(group.to_string(), i);
                return Ok((i, r));
            }
        }

        Err(first_err)
    }

    /// The first and the last dated article of `start..=end` on server `i`,
    /// from an XOVER read `DATE_ROWS` rows at most (see `Conn::xover_ends`):
    /// a date search never holds a listing, however many numbers it asks
    /// for. A listing cut short costs its connection, which still has the
    /// rest of it on the wire.
    async fn ends(&self, i: usize, group: &str, start: u64, end: u64) -> Result<Ends> {
        let r = match self.ends_once(i, group, start, end).await {
            Err(NntpError::Decompress(e)) => {
                self.no_compression(i, &e);
                self.ends_once(i, group, start, end).await
            }
            r => r,
        };
        match r {
            Err(e) if e.is_empty_range() => Ok(Ends::empty()),
            r => r,
        }
    }

    async fn ends_once(&self, i: usize, group: &str, start: u64, end: u64) -> Result<Ends> {
        let mut lease = self.lease_for(i, Some(group), false).await?;

        if lease.group() != Some(group) {
            let r = lease.begin().select_group(group).await;
            lease.check(r)?;
        }

        let sent = Instant::now();
        let r = lease.begin().xover_ends(start, end, DATE_ROWS).await;
        crate::profile::Load::add_since(&crate::profile::LOAD.xover_ns, sent);
        crate::profile::LOAD.xovers.fetch_add(1, Ordering::Relaxed);
        match r {
            Ok(ends) => {
                lease.server.headers.fetch_add(ends.rows as u64, Ordering::Relaxed);
                // the rest of a listing cut short is still coming: the lease
                // stays busy, soo the connection is thrown away
                if ends.whole { lease.check(Ok(ends)) } else { Ok(ends) }
            }
            r => lease.check(r),
        }
    }

    /// The first article at or after `number` within the next `look` numbers on
    /// server `i`, with its post time. None when there is none. One request:
    /// listings are sorted, the first rows say.
    async fn posted_at(&self, i: usize, group: &str, number: u64, look: u64) -> Result<Option<Dated>> {
        let end = number.saturating_add(look - 1);
        let mut from = number;
        loop {
            let ends = self.ends(i, group, from, end).await?;
            match (ends.first, ends.read_to) {
                (Some(first), _) => return Ok(Some(first)),
                // a cut short listing whose rows had no date: on past them
                (None, Some(n)) if !ends.whole && n < end => from = n + 1,
                _ => return Ok(None),
            }
        }
    }

    /// The last article in `start..start + look` on server `i`, with its post
    /// time. None when there is none. A listing too long to read whole is
    /// halved, the upper half first, until the halves are: what was read of
    /// it answers when nothing above it is found.
    async fn last_at(&self, i: usize, group: &str, start: u64, look: u64) -> Result<Option<Dated>> {
        enum Todo {
            /// numbers still to read
            Span(u64, u64),
            /// the last article below the spans above it in the stack
            Found(Dated),
        }
        let mut todo = vec![Todo::Span(start, start.saturating_add(look))];
        while let Some(next) = todo.pop() {
            let (a, b) = match next {
                Todo::Found(d) => return Ok(Some(d)),
                Todo::Span(a, b) => (a, b),
            };
            let ends = self.ends(i, group, a, b - 1).await?;
            if ends.whole {
                if ends.last.is_some() {
                    return Ok(ends.last);
                }
                continue;
            }
            // everything up to `read_to` was read: the rest of the span, in halves
            if let Some(last) = ends.last {
                todo.push(Todo::Found(last));
            }
            let from = ends.read_to.map_or(a, |n| n + 1);
            if from < b {
                let mid = from + (b - from) / 2;
                if from < mid {
                    todo.push(Todo::Span(from, mid));
                }
                todo.push(Todo::Span(mid, b));
            }
        }
        Ok(None)
    }

    /// When the first and the last article in the `DATE_LOOK` numbers from
    /// `number` on server `i` were posted (unix seconds), in one request.
    /// Empty when there is none.
    pub async fn posted_dates(&self, i: usize, group: &str, number: u64) -> Result<Vec<i64>> {
        let ends = self.ends(i, group, number, number.saturating_add(DATE_LOOK - 1)).await?;
        Ok(ends.first.into_iter().chain(ends.last).map(|(_, t)| t).collect())
    }

    /// The first article in `from..end` on server `i`, with its post time.
    /// Missing numbers are skipped in windows that double from `DATE_LOOK` up
    /// to `DATE_SCAN_MAX` numbers; past that the windows spread out (each
    /// starts twice as far from `from` as the last ended, the last ends at
    /// `end`), soo a hole of millions takes a few dozen small requests. The
    /// numbers stepped over are read before a hit is taken as the first (see
    /// `first_before`), and before the range is taken for empty.
    async fn first_in(&self, i: usize, group: &str, from: u64, end: u64) -> Result<Option<Dated>> {
        // the numbers between windows that weren't read, lowest first
        let mut skipped = Vec::new();
        // every number from `from` up to here was read or stepped over
        let (mut at, mut size, mut read_to) = (from, DATE_LOOK, from);
        while at < end {
            if at > read_to {
                skipped.push((read_to, at));
            }
            let look = size.min(end - at);
            if let Some(hit) = self.posted_at(i, group, at, look).await? {
                return self.first_before(i, group, &skipped, hit).await.map(Some);
            }
            read_to = at + look;
            if size < DATE_SCAN_MAX {
                at = read_to;
                size *= 2;
            } else {
                at = (read_to + (read_to - from)).min(end.saturating_sub(DATE_SCAN_MAX)).max(read_to);
            }
        }
        // no window found one: the first article, if any, was stepped over
        for &(from, to) in &skipped {
            if let Some(found) = self.posted_at(i, group, from, to - from).await? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// The first article on server `i`, given the first one a window found
    /// (`hit`) and the numbers stepped over before it (`skipped`, lowest
    /// first): an article stepped over comes first. Those between two empty
    /// windows are read in one request each (quick when there's nothing, as
    /// in a hole). The run up to the hit is narrowed down by bisecting,
    /// taking it as one run of missing numbers followed by articles, and what
    /// the bisect took for missing is read once.
    async fn first_before(&self, i: usize, group: &str, skipped: &[(u64, u64)], hit: Dated) -> Result<Dated> {
        let Some((&(start, end), between)) = skipped.split_last() else { return Ok(hit) };
        for &(from, to) in between {
            if let Some(found) = self.posted_at(i, group, from, to - from).await? {
                return Ok(found);
            }
        }
        let (mut lo, mut hi, mut best) = (start, end, hit);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let look = DATE_LOOK.min(hi - mid);
            match self.posted_at(i, group, mid, look).await? {
                Some(found) => (best, hi) = (found, found.0),
                None => lo = mid + look,
            }
        }
        if best.0 > start
            && let Some(f) = self.posted_at(i, group, start, best.0 - start).await?
        {
            best = f;
        }
        Ok(best)
    }

    /// The last article in `from..end` on server `i`, with its post time: like
    /// `first_in`, going backwards from `end`.
    async fn last_in(&self, i: usize, group: &str, from: u64, end: u64) -> Result<Option<Dated>> {
        // the numbers between windows that weren't read, highest first
        let mut skipped = Vec::new();
        // every number from here up to `end` was read or stepped over
        let (mut to, mut size, mut read_from) = (end, DATE_LOOK, end);
        while to > from {
            if to < read_from {
                skipped.push((to, read_from));
            }
            let look = size.min(to - from);
            if let Some(hit) = self.last_at(i, group, to - look, look).await? {
                return self.last_after(i, group, &skipped, hit).await.map(Some);
            }
            read_from = to - look;
            if size < DATE_SCAN_MAX {
                to = read_from;
                size *= 2;
            } else {
                to = end.saturating_sub(2 * (end - read_from)).max(from.saturating_add(DATE_SCAN_MAX)).min(read_from);
            }
        }
        // no window found one: the last article, if any, was stepped over
        for &(from, to) in &skipped {
            if let Some(found) = self.last_at(i, group, from, to - from).await? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// `first_before` going backwards: the last article, given the last one
    /// a window found and the numbers stepped over after it (highest first).
    /// The run from the hit up is taken as articles followed by one run of
    /// missing numbers.
    async fn last_after(&self, i: usize, group: &str, skipped: &[(u64, u64)], hit: Dated) -> Result<Dated> {
        let Some((&(start, end), between)) = skipped.split_last() else { return Ok(hit) };
        for &(from, to) in between {
            if let Some(found) = self.last_at(i, group, from, to - from).await? {
                return Ok(found);
            }
        }
        let (mut lo, mut hi, mut best) = (start, end, hit);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let look = DATE_LOOK.min(hi - mid);
            match self.last_at(i, group, mid, look).await? {
                Some(found) => (best, lo) = (found, found.0 + 1),
                None => hi = mid,
            }
        }
        if end > best.0 + 1
            && let Some(l) = self.last_at(i, group, best.0 + 1, end - best.0 - 1).await?
        {
            best = l;
        }
        Ok(best)
    }

    /// When the first article in `low..=high` on server `i` was posted (unix
    /// seconds): how far back the server keeps the group. None when it has none.
    pub async fn first_post(&self, i: usize, group: &str, low: u64, high: u64) -> Result<Option<i64>> {
        Ok(self.first_in(i, group, low, high + 1).await?.map(|(_, t)| t))
    }

    /// How far back server `i` keeps the group (unix seconds), judged from
    /// the dates of its first `RETENTION_WINDOWS` windows of `DATE_LOOK`
    /// numbers rather than from its first Date alone: posters set their own,
    /// and one forged on the first article (later or earlier than the truth)
    /// would make the server look shallower or deeper than it is. Each
    /// window counts from its earliest date that isnt an outlier (see
    /// `window_start`), the earliest of those answers. Unsure when the windows disagree (one
    /// more than a day older than an earlier one): a forged run, or dates too
    /// far out of order to judge by. `low` and `high` are the server's marks.
    pub async fn retention(&self, i: usize, group: &str, low: u64, high: u64) -> Result<Retention> {
        let Some((first, t)) = self.first_in(i, group, low, high + 1).await? else { return Ok(Retention::Empty) };
        let mut earliest = Vec::new();
        for w in 0..RETENTION_WINDOWS {
            let from = first + w * DATE_LOOK;
            if from > high {
                break;
            }
            let rows = self.listing(i, group, from, (from + DATE_LOOK - 1).min(high)).await?;
            let mut dates: Vec<i64> = rows.iter().filter_map(|o| plausible_post_time(&o.date)).collect();
            if dates.is_empty() {
                continue;
            }
            dates.sort_unstable();
            earliest.push(window_start(&dates));
        }
        Ok(retention_of(&earliest, t))
    }

    /// The first article number in `low..=high` on server `i` posted at or after
    /// `when` (unix seconds), `high + 1` when there is none. A binary search over
    /// small article requests (about 35 for a billion numbers); a probe that
    /// lands in a hole in the numbering finds the articles on both sides of it,
    /// soo the hole is crossed once. Post dates are only roughly in order, soo
    /// the answer is approximate near the edges: callers overlap their ranges.
    pub async fn article_at(&self, i: usize, group: &str, low: u64, high: u64, when: i64) -> Result<u64> {
        // every article before lo is older than `when`, none from hi on is;
        // first is the first article from hi on (high + 1 when there is none)
        let (mut lo, mut hi, mut first) = (low, high + 1, high + 1);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let found = self.first_in(i, group, mid, hi).await?;
            // the first article found older: the window from it says whether
            // all of it is (a single forged Date doesnt move lo past the
            // articles below mid), else its first one that isnt answers
            let mut newer = found.map(|(n, _)| n);
            if let Some((n, t)) = found
                && t < when
            {
                match self.first_dated_from(i, group, n, hi, when).await? {
                    None => {
                        lo = n + 1;
                        continue;
                    }
                    Some(m) => newer = Some(m),
                }
            }
            // nothing from mid up to the article found (or up to hi)
            let gap_end = found.map_or(hi, |(n, _)| n);
            if let Some(n) = newer {
                first = n;
            }
            hi = mid;
            // mid is in a hole: the article before it decides which side
            // the answer is on, instead of halving through the hole
            if gap_end - mid > DATE_LOOK {
                match self.last_in(i, group, lo, mid).await? {
                    Some((m, t)) if t >= when => (hi, first) = (m, m),
                    _ => lo = mid,
                }
            }
        }
        Ok(first)
    }

    /// The articles of `start..=end` on server `i`, empty when there are none.
    async fn listing(&self, i: usize, group: &str, start: u64, end: u64) -> Result<Vec<Overview>> {
        match self.xover_on(i, group, start, end).await {
            Err(e) if e.is_empty_range() => Ok(Vec::new()),
            r => r,
        }
    }

    /// The first article in the `DATE_LOOK` numbers from `n` (below `hi`) on
    /// server `i` posted at or after `when`, by its plausible Date. None when
    /// every one of them was posted before.
    async fn first_dated_from(&self, i: usize, group: &str, n: u64, hi: u64, when: i64) -> Result<Option<u64>> {
        let end = n.saturating_add(DATE_LOOK - 1).min(hi.saturating_sub(1)).max(n);
        let rows = self.listing(i, group, n, end).await?;
        Ok(rows.iter().find(|o| plausible_post_time(&o.date).is_some_and(|t| t >= when)).map(|o| o.number))
    }

    /// Compressed listings from server `i` couldnt be read: it gets plain ones from now on.
    fn no_compression(&self, i: usize, e: &str) {
        let server = &self.servers[i];
        if !server.no_compress.swap(true, Ordering::Relaxed) {
            println!("{}: couldnt read compressed headers ({e}), turning compression off", server.cfg.host);
        }
        server.idle.lock().unwrap().retain(|c| !c.compressed);
    }

    async fn xover_on(&self, i: usize, group: &str, start: u64, end: u64) -> Result<Vec<Overview>> {
        match self.xover_once(i, group, start, end).await {
            Err(NntpError::Decompress(e)) => {
                // turn compression off for this server and redo the slice uncompressed
                self.no_compression(i, &e);
                self.xover_once(i, group, start, end).await
            }
            r => r,
        }
    }

    async fn xover_once(&self, i: usize, group: &str, start: u64, end: u64) -> Result<Vec<Overview>> {
        let mut lease = self.lease_for(i, Some(group), false).await?;

        if lease.group() != Some(group) {
            let r = lease.begin().select_group(group).await;
            lease.check(r)?;
        }

        let sent = Instant::now();
        let r = lease.begin().xover(start, end).await;
        crate::profile::Load::add_since(&crate::profile::LOAD.xover_ns, sent);
        crate::profile::LOAD.xovers.fetch_add(1, Ordering::Relaxed);
        if let Ok(rows) = &r {
            lease.server.headers.fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        lease.check(r)
    }

    /// Per server numbers for the stats dashboard.
    pub fn server_stats(&self) -> Vec<ServerStat> {
        let indexing = self.indexing_servers();
        self.servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let limit = s.limit.load(Ordering::Relaxed);
                let in_use = limit.saturating_sub(s.permits.available_permits());
                let down = s.down_until.lock().unwrap().is_some_and(|t| Instant::now() < t);
                let state = if s.no_index.load(Ordering::Relaxed) {
                    "article only"
                } else if down && s.warned_auth.load(Ordering::Relaxed) {
                    "login rejected"
                } else if down {
                    "resting"
                } else {
                    "ok"
                };
                ServerStat {
                    host: s.cfg.host.clone(),
                    priority: s.cfg.priority,
                    connections: s.cfg.connections(),
                    limit,
                    in_use,
                    open: in_use + s.idle.lock().unwrap().len(),
                    indexing: indexing.contains(&i),
                    state,
                    headers: s.headers.load(Ordering::Relaxed),
                    wire_bytes: s.wire.load(Ordering::Relaxed),
                    text_bytes: s.text.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// Fetch every `(start, end)` slice of `group` on `server` with
    /// up to `connections` requests in flight, sending each slice to the
    /// receiver the moment it arrives. A connection picks up the next slice as
    /// soon as it is free. No new slices are started once `stop` is set or a
    /// slice fails with anything but 423/420 (empty) / 5xx (not available), and
    /// once `stop` is set the slices still in flight are dropped unsent.
    ///
    /// Every slice holds its room in the pool's unsaved budget (`Unsaved`)
    /// until the receiver drops it, soo a receiver keeps it until the slice is
    /// saved. The room is taken before a connection, and the slices of one
    /// stream take it in order, one waiting at a time. Nothing that holds room
    /// waits for more: a slice holding some is fetching (and needs only a
    /// connection, which no one waiting for room holds), waiting in the
    /// channel for its receiver, or being saved, soo room always comes free.
    pub fn stream_headers(
        self: &Arc<Self>,
        group: &str,
        server: usize,
        slices: Vec<(u64, u64)>,
        stop: Arc<AtomicBool>,
    ) -> tokio::sync::mpsc::Receiver<HeaderSlice> {
        let i = server;
        // several groups can stream from one server at once, the server's
        // semaphore keeps the total at its `connections`
        let workers = self.connections(i).max(1).min(slices.len().max(1));
        let (tx, rx) = tokio::sync::mpsc::channel(SLICES_BUFFERED);
        let queue = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from(slices)));
        let halt = Arc::new(AtomicBool::new(false));

        for _ in 0..workers {
            let (pool, queue, halt, stop, tx, group) =
                (self.clone(), queue.clone(), halt.clone(), stop.clone(), tx.clone(), group.to_string());

            tokio::spawn(async move {
                loop {
                    if stop.load(Ordering::Relaxed) || halt.load(Ordering::Relaxed) {
                        return;
                    }

                    // the next slice and its room, the queue held meanwhile
                    let ((start, end), mut unsaved) = {
                        let mut queue = queue.lock().await;
                        let Some((start, end)) = queue.pop_front() else { return };
                        let room = pool.reserve_unsaved(end.saturating_sub(start).saturating_add(1));
                        let Some(unsaved) = unless_stopped(&stop, room).await else { return };
                        ((start, end), unsaved)
                    };
                    if halt.load(Ordering::Relaxed) {
                        return;
                    }

                    let Some(result) = unless_stopped(&stop, pool.xover_on(i, &group, start, end)).await else {
                        return;
                    };
                    unsaved.keep(result.as_ref().map_or(0, Vec::len));

                    if let Err(e) = &result
                        && !e.is_empty_range()
                        && !e.is_permanent()
                    {
                        halt.store(true, Ordering::Relaxed);
                    }

                    if tx.send(HeaderSlice { start, end, result, unsaved }).await.is_err() {
                        return;
                    }
                }
            });
        }

        rx
    }

    /// XOVER `start..=end` of `group` on the active server, split into slices
    /// fetched in parallel over its connections. Slices with no articles
    /// (423/420) come back empty; any other failure fails the whole range.
    pub async fn fetch_headers(self: &Arc<Self>, group: &str, start: u64, end: u64) -> Result<Vec<Overview>> {
        if end < start {
            return Ok(Vec::new());
        }

        let i = self.active_index();
        let total = end - start + 1;
        let chunk = total.div_ceil(self.concurrency().max(1) as u64).max(MIN_CHUNK);

        let mut tasks = JoinSet::new();
        let mut slices = 0;
        let mut a = start;

        while a <= end {
            let b = end.min(a + chunk - 1);
            let pool = self.clone();
            let group = group.to_string();
            let idx = slices;
            tasks.spawn(async move { (idx, pool.xover_on(i, &group, a, b).await) });
            slices += 1;
            a = b + 1;
        }

        let mut parts: Vec<Vec<Overview>> = vec![Vec::new(); slices];
        let mut failure = None;

        // let every slice finish, an aborted one would return a half read connection to the pool
        while let Some(joined) = tasks.join_next().await {
            let (idx, result) = joined.map_err(|e| NntpError::Protocol(format!("xover task failed: {e}")))?;
            match result {
                Ok(rows) => parts[idx] = rows,
                Err(e) if e.is_empty_range() => {}
                Err(e) => {
                    failure.get_or_insert(e);
                }
            }
        }

        match failure {
            Some(e) => Err(e),
            None => Ok(parts.into_iter().flatten().collect()),
        }
    }

    /// How busy server `i` is: requests on it and waiting for it, per connection.
    fn busy(&self, i: usize) -> f64 {
        let s = &self.servers[i];
        let limit = s.limit.load(Ordering::Relaxed).max(1);
        let in_use = limit.saturating_sub(s.permits.available_permits());
        (in_use + s.waiting.load(Ordering::Relaxed)) as f64 / limit as f64
    }

    /// BODY from the least busy indexing server first, soo name lookups dont
    /// all queue on one server (equally busy ones in priority order), then
    /// from the others in priority order until one has it.
    pub async fn fetch_body(&self, message_id: &str) -> Result<Vec<u8>> {
        let mut order = self.indexing_tier();
        order.sort_by(|&a, &b| self.busy(a).total_cmp(&self.busy(b)));
        let rest: Vec<usize> = (0..self.servers.len()).filter(|i| !order.contains(i)).collect();
        order.extend(rest);
        let mut last_err = Self::no_servers();

        for i in order {
            let mut lease = match self.lease(i, false).await {
                Ok(l) => l,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };

            let r = lease.begin().body(message_id).await;
            match lease.check(r) {
                Ok(body) => return Ok(body),
                Err(e) => last_err = e,
            }
        }

        Err(last_err)
    }

    pub async fn list_groups(&self, pattern: Option<&str>) -> Result<Vec<(String, u64)>> {
        let mut lease = self.lease(self.active_index(), false).await?;
        let r = lease.begin().list_groups(pattern).await;
        lease.check(r)
    }
}

/// Turns a body into a name (par2 / nfo parsers).
pub type Extract = fn(&[u8]) -> Option<String>;

/// One XOVER slice from `Pool::stream_headers`.
pub struct HeaderSlice {
    pub start: u64,
    pub end: u64,
    pub result: Result<Vec<Overview>>,
    /// the slice's room in the unsaved budget: keep it until the slice is saved
    pub unsaved: Unsaved,
}

/// For each job, fetch its message-ids in order until `extract` gives a
/// name. Jobs run concurrently, limited by each server's connections.
pub async fn first_names(pool: Arc<Pool>, jobs: Vec<Vec<(String, Extract)>>) -> Vec<Option<String>> {
    let mut names = vec![None; jobs.len()];
    let mut tasks = JoinSet::new();

    for (idx, job) in jobs.into_iter().enumerate().filter(|(_, j)| !j.is_empty()) {
        let pool = pool.clone();
        tasks.spawn(async move {
            for (message_id, extract) in job {
                if let Ok(body) = pool.fetch_body(&message_id).await
                    && let Some(name) = extract(&body)
                {
                    return (idx, Some(name));
                }
            }
            (idx, None)
        });
    }

    while let Some(joined) = tasks.join_next().await {
        if let Ok((idx, name)) = joined {
            names[idx] = name;
        }
    }

    names
}

/// The pool driven from sync code on its own tokio runtime.
pub struct BlockingPool {
    rt: Runtime,
    pub pool: Arc<Pool>,
}

impl BlockingPool {
    pub fn new(servers: &[UsenetServer]) -> Self {
        Self::from_pool(Pool::new(servers))
    }

    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self::from_pool(Pool::new(&cfg.servers).with_max_unsaved(cfg.max_unsaved_headers()))
    }

    pub fn from_pool(pool: Pool) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("atlas-nntp")
            .enable_all()
            .build()
            .expect("couldnt start tokio runtime");

        BlockingPool { rt, pool: Arc::new(pool) }
    }

    pub fn len(&self) -> usize {
        self.pool.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.pool.active_index()
    }

    pub fn active_host(&self) -> String {
        self.pool.active_host()
    }

    pub fn is_connected(&self) -> bool {
        self.pool.is_connected()
    }

    pub fn connect(&self) -> Result<()> {
        self.rt.block_on(self.pool.connect())
    }

    pub fn disconnect(&self) {
        self.rt.block_on(self.pool.close());
    }

    pub fn restore_primary(&self, after: Duration) -> bool {
        self.rt.block_on(self.pool.restore_primary(after))
    }

    pub fn select_group(&self, group: &str) -> Result<(u64, u64, u64, String)> {
        self.rt.block_on(self.pool.select_group(group))
    }

    pub fn fetch_headers(&self, group: &str, start: u64, end: u64) -> Result<Vec<Overview>> {
        self.rt.block_on(self.pool.fetch_headers(group, start, end))
    }

    pub fn fetch_body(&self, message_id: &str) -> Result<Vec<u8>> {
        self.rt.block_on(self.pool.fetch_body(message_id))
    }

    pub fn list_groups(&self, pattern: Option<&str>) -> Result<Vec<(String, u64)>> {
        self.rt.block_on(self.pool.list_groups(pattern))
    }

    /// Real names for each job, see [`first_names`].
    pub fn first_names(&self, jobs: Vec<Vec<(String, Extract)>>) -> Vec<Option<String>> {
        self.rt.block_on(first_names(self.pool.clone(), jobs))
    }

    /// Run a future on this pool's runtime.
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// Log into one server directly (selftest).
    pub fn check_server(server: &UsenetServer) -> Result<()> {
        let pool = BlockingPool::new(std::slice::from_ref(server));
        pool.rt.block_on(async {
            let mut conn = Conn::open(server, DEFAULT_TIMEOUT, false).await?;
            conn.quit().await;
            Ok(())
        })
    }
}

impl Drop for BlockingPool {
    fn drop(&mut self) {
        self.pool.disconnect();
    }
}

/// guess if the server wants ssl before we connect
pub fn detect_use_ssl(host: &str, port: u16) -> bool {
    // 563 is the ssl port, 119 is the plain one
    match port {
        563 => return true,
        119 => return false,
        _ => {}
    }

    let host = host.trim().trim_matches(['[', ']']).trim_end_matches('.').to_ascii_lowercase();

    // local servers are plain usually
    !(matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") || host.ends_with(".local"))
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .with_root_certificates(roots)
            .with_no_client_auth();

        Arc::new(config)
    });

    CONFIG.clone()
}

fn parse_overview(line: &[u8]) -> Option<Overview> {
    let fields: Vec<String> = line.split(|b| *b == b'\t').map(|f| String::from_utf8_lossy(f).into_owned()).collect();
    let field = |i: usize| fields.get(i).cloned().unwrap_or_default();
    let int = |i: usize| fields.get(i).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0);

    Some(Overview {
        number: fields.first()?.trim().parse().ok()?,
        subject: field(1),
        from: field(2),
        date: field(3),
        message_id: field(4),
        references: field(5),
        bytes: int(6),
        lines: int(7),
    })
}

/// fnmatch-ish match supporting `*` and `?`
pub fn wildmatch(name: &str, pattern: &str) -> bool {
    static CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Regex>>> =
        LazyLock::new(Default::default);

    let mut cache = CACHE.lock().unwrap();
    let re = cache.entry(pattern.to_string()).or_insert_with(|| {
        let escaped = regex::escape(pattern).replace(r"\*", ".*").replace(r"\?", ".");
        Regex::new(&format!("^(?is:{escaped})$")).unwrap()
    });

    re.is_match(name)
}

/// Decode a yEnc article body. None when there is no `=ybegin` line.
pub fn yenc_decode(lines: &[Vec<u8>]) -> Option<Vec<u8>> {
    let start = lines.iter().position(|l| l.starts_with(b"=ybegin "))?;
    let mut out = Vec::new();

    for line in &lines[start + 1..] {
        if line.starts_with(b"=ypart ") {
            continue;
        }

        if line.starts_with(b"=yend") {
            break;
        }

        let mut escaped = false;
        for &b in line {
            if escaped {
                out.push(b.wrapping_sub(64).wrapping_sub(42));
                escaped = false;
            } else if b == b'=' {
                escaped = true;
            } else if b != b'\r' && b != b'\n' {
                out.push(b.wrapping_sub(42));
            }
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    /// the dates the first windows count by judge a server's retention: the earliest
    /// answers, and one more than a day older than an earlier one is unsure
    #[test]
    fn a_servers_key_is_its_own_and_doesnt_change_with_the_others() {
        let s = |h: &str, user: &str, port: u16| UsenetServer::new(h, user, "secret", port);
        let first = s("A.example", "x", 563);
        // 563 with ssl: the plain host, like always
        assert_eq!(server_keys(std::slice::from_ref(&first)), ["A.example"]);
        // a second account on the same host and port, another on another
        // port, and one with an explicit key: the first one's key stays
        let mut keyed = s("a.example", "y", 563);
        keyed.key = Some("block".into());
        let all = [first.clone(), s("a.example", "y", 563), s("a.example", "z", 119), s("a.example", "w", 443), keyed];
        assert_eq!(server_keys(&all), ["A.example", "a.example", "a.example:119", "a.example:443", "a.example#block"]);
        // a plain 119 and an ssl 563 on one host never share a key
        let mut plain119 = s("a.example", "u", 119);
        plain119.ssl = Some(false);
        let keys = server_keys(&[first.clone(), plain119]);
        assert_eq!(keys, ["A.example", "a.example:119"]);
        let mut plain = s("a.example", "v", 563);
        plain.ssl = Some(false);
        assert_eq!(server_keys(&[plain]), ["a.example:563"], "563 isnt the default without ssl");
        assert_eq!(Pool::new(&all).host(4), "a.example#block");
        // what the keys used to be, for cursors saved under them: never
        // another server's key now
        let old = legacy_server_keys(&all);
        assert_eq!(old[0], ["a.example:563", "x@a.example:563"]);
        assert_eq!(old[3], ["w@a.example:443"]);
    }

    #[test]
    fn retention_from_the_first_windows() {
        use super::{Retention, retention_of};
        assert_eq!(retention_of(&[], 50), Retention::Since(50), "no dates: the first article's");
        assert_eq!(retention_of(&[1_000, 2_000, 90_000], 7), Retention::Since(1_000));
        assert_eq!(retention_of(&[1_000, 500, 2_000], 7), Retention::Since(500), "within a day out of order");
        assert_eq!(retention_of(&[200_000, 300_000, 100_000], 7), Retention::Unsure(100_000));
        assert_eq!(retention_of(&[200_000, 100_000], 7), Retention::Unsure(100_000));
    }

    /// a window starts at its earliest date, unless a few are far before the rest
    #[test]
    fn a_window_starts_at_its_earliest_date_but_a_forged_one() {
        use super::window_start;
        assert_eq!(window_start(&[10, 3_600, 7_200, 10_800, 14_400]), 10);
        assert_eq!(window_start(&[10, 900_000, 900_100, 900_200, 900_300]), 900_000, "one forged far back");
        assert_eq!(window_start(&[5]), 5);
    }

    use super::*;
    use tokio::io::AsyncWriteExt;

    fn yenc_encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in data {
            let e = b.wrapping_add(42);
            if matches!(e, 0 | b'\n' | b'\r' | b'=') {
                out.push(b'=');
                out.push(e.wrapping_add(64));
            } else {
                out.push(e);
            }
        }
        out
    }

    #[test]
    fn yenc_roundtrip() {
        let data: Vec<u8> = (0..=255u8).collect();
        let lines = vec![
            b"=ybegin part=1 line=128 size=256 name=x.bin".to_vec(),
            b"=ypart begin=1 end=256".to_vec(),
            yenc_encode(&data[..100]),
            yenc_encode(&data[100..]),
            b"=yend size=256 part=1".to_vec(),
        ];
        assert_eq!(yenc_decode(&lines).unwrap(), data);
        assert!(yenc_decode(&[b"plain text".to_vec()]).is_none());
    }

    #[test]
    fn ssl_detection() {
        assert!(detect_use_ssl("news.example.com", 563));
        assert!(!detect_use_ssl("news.example.com", 119));
        assert!(!detect_use_ssl("localhost", 1190));
        assert!(!detect_use_ssl("box.local", 4000));
        assert!(detect_use_ssl("news.example.com", 443));
    }

    #[test]
    fn wildcard() {
        assert!(wildmatch("alt.binaries.movies", "*movies*"));
        assert!(wildmatch("alt.binaries.tv", "alt.binaries*"));
        assert!(!wildmatch("comp.lang.rust", "alt.binaries*"));
        assert!(wildmatch("a.b", "a?b"));
    }

    #[test]
    fn overview_parse() {
        let line = b"42\tsubj\tme@x\tFri, 02 Oct 2026 10:11:12 +0000\t<id@x>\t\t1234\t10";
        let o = parse_overview(line).unwrap();
        assert_eq!(o.number, 42);
        assert_eq!(o.message_id, "<id@x>");
        assert_eq!(o.bytes, 1234);
        let a = o.into_article();
        assert_eq!(a.date, "2026-10-02 10:11:12");
    }

    fn run<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn listing(n: usize) -> Vec<u8> {
        let mut text = Vec::new();
        for i in 1..=n {
            text.extend_from_slice(
                format!("{i}\t\"file{i}.rar\" yEnc (1/1)\tme\tdate\t<{i}@x>\t\t100\t1\r\n").as_bytes(),
            );
        }
        text.extend_from_slice(b"..dot stuffed\r\n.\r\n");
        text
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::GzBuilder::new().filename("x.txt").write(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Feed `wire` to a connection in `piece` sized writes and read it back.
    /// Also checks nothing past the response got eaten.
    fn read_compressed(wire: Vec<u8>, piece: usize) -> Vec<Vec<u8>> {
        run(async move {
            let (client, mut server) = tokio::io::duplex(1 << 20);
            let mut conn = Conn::over(Box::new(client), Duration::from_secs(5));
            tokio::spawn(async move {
                for chunk in wire.chunks(piece) {
                    server.write_all(chunk).await.unwrap();
                    tokio::task::yield_now().await;
                }
                server.write_all(b"205 next\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            let lines = conn.read_response_body("[COMPRESS=GZIP]").await.unwrap();
            assert_eq!(conn.read_status().await.unwrap().0, 205, "read past the end of the response");
            lines
        })
    }

    #[test]
    fn compressed_listings() {
        let plain = listing(500);

        for (name, wire) in [
            ("zlib", zlib(&plain)),
            ("gzip", gzip(&plain)),
            ("zlib + plain terminator", {
                let mut w = zlib(&plain[..plain.len() - 3]);
                w.extend_from_slice(b".\r\n");
                w
            }),
        ] {
            for piece in [1, 7, 4096, 1 << 20] {
                let lines = read_compressed(wire.clone(), piece);
                assert_eq!(lines.len(), 501, "{name} piece {piece}");
                assert_eq!(lines.last().unwrap(), b".dot stuffed", "{name}");
                assert!(parse_overview(&lines[0]).is_some());
            }
        }
    }

    #[test]
    fn highly_compressible_listing() {
        // repetitive headers inflate far past what one output reservation holds
        let mut plain = Vec::new();
        for i in 0..20_000 {
            plain.extend_from_slice(
                format!("{i}\tsame subject over and over again yEnc (1/1)\tme\tdate\t<{i}@x>\t\t1\t1\r\n").as_bytes(),
            );
        }
        plain.extend_from_slice(b".\r\n");
        let wire = zlib(&plain);
        assert!(plain.len() / wire.len() > 8, "test data should compress better than 8:1");

        for piece in [1 << 20, 4096] {
            assert_eq!(read_compressed(wire.clone(), piece).len(), 20_000, "piece {piece}");
        }
    }

    /// A date search reads a listing's first rows and no more: plain or
    /// compressed, a listing longer than its rows is left unread.
    #[test]
    fn a_long_listing_is_read_only_as_far_as_the_row_limit() {
        let dated = |n: usize| {
            let mut text = Vec::new();
            for i in 1..=n {
                let row = format!("{i}\ts{i}\tme\tFri, 02 Oct 2026 10:{:02}:00 +0000\t<{i}@x>\t\t100\t1\r\n", i % 60);
                text.extend_from_slice(row.as_bytes());
            }
            text.extend_from_slice(b".\r\n");
            text
        };
        let read = |status: &'static str, wire: Vec<u8>, rows: usize| {
            run(async move {
                let (client, mut server) = tokio::io::duplex(1 << 16);
                let mut conn = Conn::over(Box::new(client), Duration::from_secs(5));
                tokio::spawn(async move {
                    let mut cmd = [0u8; 64];
                    let _ = tokio::io::AsyncReadExt::read(&mut server, &mut cmd).await;
                    let _ = server.write_all(status.as_bytes()).await;
                    let _ = server.write_all(&wire).await;
                });
                conn.xover_ends(1, 50_000, rows).await.unwrap()
            })
        };
        let posted = |n: u64| {
            (
                n,
                chrono::DateTime::parse_from_rfc2822(&format!("Fri, 02 Oct 2026 10:{:02}:00 +0000", n % 60))
                    .unwrap()
                    .timestamp(),
            )
        };

        for (name, status, wire) in [
            ("plain", "224 overview follows\r\n", dated(50_000)),
            ("compressed", "224 overview follows [COMPRESS=GZIP]\r\n", zlib(&dated(50_000))),
        ] {
            let cut = read(status, wire.clone(), 100);
            assert_eq!((cut.whole, cut.rows, cut.read_to), (false, 100, Some(100)), "{name}");
            assert_eq!((cut.first, cut.last), (Some(posted(1)), Some(posted(100))), "{name}");

            let whole = read(status, wire, 50_000);
            assert_eq!((whole.whole, whole.rows), (true, 50_000), "{name}");
            assert_eq!((whole.first, whole.last), (Some(posted(1)), Some(posted(50_000))), "{name}");
        }
    }

    #[test]
    fn garbage_is_a_decompress_error() {
        run(async {
            let (client, mut server) = tokio::io::duplex(1 << 16);
            let mut conn = Conn::over(Box::new(client), Duration::from_secs(2));
            server.write_all(&[0x78, 0x9c, 0xff, 0xff, 0xff, 0xff, 0x00, 0x01]).await.unwrap();
            let err = conn.read_response_body("[COMPRESS=GZIP]").await.unwrap_err();
            assert!(matches!(err, NntpError::Decompress(_)), "{err}");
        });
    }

    #[test]
    fn gzip_header_needs_whole_header() {
        let g = gzip(b"hi");
        assert!(gzip_header_len(&g[..5]).is_none());
        // 10 fixed bytes + "x.txt\0"
        assert_eq!(gzip_header_len(&g).unwrap().unwrap(), 16);
    }

    #[test]
    fn groups_spread_over_every_indexing_server_by_connections() {
        let server = |host: &str, conns: u32, priority: i64, index: Option<bool>| {
            let mut s = UsenetServer::new(host, "u", "p", 563);
            s.connections = Some(conns);
            s.priority = priority;
            s.index = index;
            s
        };
        let pool = Pool::new(&[
            server("a", 10, 1, None),
            server("b", 10, 2, None),
            server("c", 30, 4, None),
            server("block", 50, 9, Some(false)),
        ]);

        assert_eq!(pool.indexing_servers(), vec![0, 1, 2], "index:false stays out");
        assert_eq!(pool.indexing_connections(), 50);

        let mut counts = [0usize; 4];
        for i in 0..5000 {
            counts[pool.pick_server(&format!("alt.binaries.group{i}"))] += 1;
        }
        // 10:10:30 connections -> roughly 20% / 20% / 60%, every priority used
        assert_eq!(counts[3], 0);
        for (i, want) in [(0, 0.2), (1, 0.2), (2, 0.6)] {
            let got = counts[i] as f64 / 5000.0;
            assert!((got - want).abs() < 0.05, "server {i}: {got} vs {want} ({counts:?})");
        }

        // stable: same group, same server
        assert_eq!(pool.pick_server("alt.binaries.x"), pool.pick_server("alt.binaries.x"));

        // groups that need another server spread over the others by connections,
        // not all onto the first in the list
        let mut first = [0usize; 4];
        for i in 0..5000 {
            first[pool.ranked(&[1, 2], &format!("alt.binaries.group{i}"))[0]] += 1;
        }
        let share_c = first[2] as f64 / 5000.0;
        assert!((share_c - 0.75).abs() < 0.05, "10:30 connections -> about 25% / 75%: {first:?}");
        assert_eq!(pool.ranked(&[1, 2], "alt.binaries.x"), pool.ranked(&[1, 2], "alt.binaries.x"), "stable order");

        // once found elsewhere, a group is picked there
        pool.homes.lock().unwrap().insert("alt.binaries.moved".into(), 1);
        assert_eq!(pool.pick_server("alt.binaries.moved"), 1);
    }

    #[test]
    fn a_server_going_down_only_moves_its_own_groups() {
        let server = |host: &str| {
            let mut s = UsenetServer::new(host, "u", "p", 563);
            s.connections = Some(4);
            s
        };
        let pool = Pool::new(&[server("a"), server("b"), server("c")]);
        let groups: Vec<String> = (0..300).map(|i| format!("alt.binaries.group{i}")).collect();
        let before: Vec<usize> = groups.iter().map(|g| pool.pick_server(g)).collect();

        *pool.servers[2].down_until.lock().unwrap() = Some(Instant::now() + DOWN_FOR);
        assert_eq!(pool.indexing_tier(), vec![0, 1]);
        for (g, &was) in groups.iter().zip(&before) {
            let now = pool.pick_server(g);
            if was == 2 {
                assert_ne!(now, 2, "{g}: its server is down, it goes to another");
            } else {
                assert_eq!(now, was, "{g}: its server is up, it stays there");
            }
        }

        // back up: every group is where it was
        *pool.servers[2].down_until.lock().unwrap() = None;
        assert_eq!(groups.iter().map(|g| pool.pick_server(g)).collect::<Vec<_>>(), before);
    }

    #[test]
    fn compressed_group_list() {
        let mut listing = Vec::new();
        for (name, hi, lo) in
            [("alt.binaries.movies", 900, 100), ("alt.binaries.movies.4k", 50, 1), ("alt.binaries.empty", 5, 5)]
        {
            listing.extend_from_slice(format!("{name} {hi} {lo} y\r\n").as_bytes());
        }
        listing.extend_from_slice(b".\r\n");
        let mut wire = b"215 newsgroups follow [COMPRESS=GZIP]\r\n".to_vec();
        wire.extend(zlib(&listing));

        let groups = run(async move {
            let (client, mut server) = tokio::io::duplex(1 << 16);
            let mut conn = Conn::over(Box::new(client), Duration::from_secs(5));
            tokio::spawn(async move {
                let mut cmd = vec![0u8; 64];
                let _ = tokio::io::AsyncReadExt::read(&mut server, &mut cmd).await;
                server.write_all(&wire).await.unwrap();
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            conn.list_groups(Some("*movies*")).await.unwrap()
        });

        assert_eq!(groups, vec![("alt.binaries.movies".to_string(), 800), ("alt.binaries.movies.4k".to_string(), 49)]);
    }

    #[test]
    fn refusals_from_real_providers() {
        let r = |code: u16, m: &str| refusal(&NntpError::Reply { code, message: m.into() });
        // seen from the providers in use
        let many = [
            (
                481,
                "(remote) (aucl:newsgroupdirect.com;someone@newsgroupdirect.com) exceeded maximum number of connections per user",
            ),
            (502, "Too many connections."),
            (482, "too many connections for your user"),
            (502, "connection limit (100) reached"),
            (482, "Connection limit(50) reached."),
            (502, "bonus.frugalusenet.com: too many connections - support@frugalusenet.com"),
        ];
        for (code, m) in many {
            assert_eq!(r(code, m), Refusal::TooMany, "{m}");
        }
        assert_eq!(r(502, "Authentication Failed"), Refusal::Auth);
        assert_eq!(r(502, "Access Denied. Please check your login/pw."), Refusal::Auth);
        assert_eq!(r(400, "service temporarily unavailable"), Refusal::Other);
        assert_eq!(refusal(&timed_out()), Refusal::Other);
    }
}
