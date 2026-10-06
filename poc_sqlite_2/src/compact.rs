//! The compact article layout.
//!
//! Today every article is a row in `articles` holding its message-id, its
//! subject and its filename, plus a second copy of the message-id in the
//! unique (release_id, message_id) index, plus one `release_files` row per file.
//!
//! Compact:
//! - `files`: one row per file of a release: filename, the subject the NZB
//!   uses (the first article's in (part, message-id) order, the way today's
//!   query sorts them), and the part bitmap that `release_files` holds today.
//! - `segments`: one row per article, keyed by (file, message-id) in a
//!   WITHOUT ROWID table, soo the message-id is stored once and there is no
//!   second index. Just the part number and size besides.
//! - `Packed` also splits message-ids into a local part and a domain
//!   (`@ngPost>`, `@nyuu>`, ...) kept once in `domains`, and stores hex local
//!   parts as bytes (half the size). Every message-id comes back exactly.

use std::collections::HashMap;

use atlas::parser::{Article, Release};
use atlas::search::ArticleRow;
use rusqlite::{Connection, OptionalExtension, Result, params};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// one subject per file, message-id stored once
    Plain,
    /// and message-ids split into a shared domain plus a packed local part
    Packed,
}

/// Add the compact tables to a database made by `atlas::db::create_db_at`
/// (releases, the search index and its triggers stay as they are).
pub fn create(conn: &Connection, flavor: Flavor) -> Result<()> {
    conn.execute_batch(
        "create table if not exists files (
            id INTEGER PRIMARY KEY,
            release_id INTEGER NOT NULL,
            filename TEXT NOT NULL,
            subject TEXT,
            subject_part INTEGER,
            subject_mid TEXT,
            expected INTEGER,
            file_total INTEGER,
            seen BLOB NOT NULL
        );
        create unique index if not exists files_key on files(release_id, filename);",
    )?;
    match flavor {
        Flavor::Plain => conn.execute_batch(
            "create table if not exists segments (
                file_id INTEGER NOT NULL,
                message_id TEXT NOT NULL,
                part INTEGER,
                bytes INTEGER,
                primary key (file_id, message_id)
            ) without rowid;",
        ),
        Flavor::Packed => conn.execute_batch(
            "create table if not exists domains (id INTEGER PRIMARY KEY, suffix TEXT NOT NULL UNIQUE);
            create table if not exists segments (
                file_id INTEGER NOT NULL,
                local BLOB NOT NULL,
                domain INTEGER NOT NULL,
                part INTEGER,
                bytes INTEGER,
                primary key (file_id, local, domain)
            ) without rowid;",
        ),
    }
}

// ---------------------------------------------------------------- message-ids

const TEXT: u8 = 0;
const HEX_LOWER: u8 = 1;
const HEX_UPPER: u8 = 2;

fn pack_local(local: &str) -> Vec<u8> {
    let bytes = local.as_bytes();
    let even = !bytes.is_empty() && bytes.len().is_multiple_of(2);
    let lower = even && bytes.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    let upper = even && !lower && bytes.iter().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(b));
    if lower || upper {
        let nibble = |c: u8| (c as char).to_digit(16).unwrap() as u8;
        let mut out = Vec::with_capacity(1 + bytes.len() / 2);
        out.push(if lower { HEX_LOWER } else { HEX_UPPER });
        out.extend(bytes.chunks(2).map(|p| nibble(p[0]) << 4 | nibble(p[1])));
        out
    } else {
        let mut out = Vec::with_capacity(1 + bytes.len());
        out.push(TEXT);
        out.extend_from_slice(bytes);
        out
    }
}

fn unpack_local(blob: &[u8]) -> String {
    match blob.first() {
        Some(&HEX_LOWER) => blob[1..].iter().map(|b| format!("{b:02x}")).collect(),
        Some(&HEX_UPPER) => blob[1..].iter().map(|b| format!("{b:02X}")).collect(),
        _ => String::from_utf8_lossy(blob.get(1..).unwrap_or_default()).into_owned(),
    }
}

/// `<local@domain>` -> (local, "@domain>"); anything else stays whole (domain 0).
fn split_message_id(id: &str) -> Option<(&str, &str)> {
    let inner = id.strip_prefix('<')?;
    if !id.ends_with('>') {
        return None;
    }
    let at = inner.rfind('@')?;
    Some((&inner[..at], &inner[at..]))
}

/// The domains of a Packed database, cached.
#[derive(Default)]
pub struct Domains {
    ids: HashMap<String, i64>,
    suffixes: HashMap<i64, String>,
    loaded: bool,
}

impl Domains {
    fn load(&mut self, conn: &Connection) -> Result<()> {
        if self.loaded {
            return Ok(());
        }
        let mut stmt = conn.prepare("select id, suffix from domains")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, suffix) = row?;
            self.ids.insert(suffix.clone(), id);
            self.suffixes.insert(id, suffix);
        }
        self.loaded = true;
        Ok(())
    }

    /// (local blob, domain id) for a message-id, adding new domains
    fn encode(&mut self, conn: &Connection, id: &str) -> Result<(Vec<u8>, i64)> {
        let Some((local, suffix)) = split_message_id(id) else { return Ok((pack_text(id), 0)) };
        let domain = match self.ids.get(suffix) {
            Some(d) => *d,
            None => {
                conn.execute("insert into domains (suffix) values (?)", [suffix])?;
                let d = conn.last_insert_rowid();
                self.ids.insert(suffix.to_string(), d);
                self.suffixes.insert(d, suffix.to_string());
                d
            }
        };
        Ok((pack_local(local), domain))
    }

    fn decode(&self, local: &[u8], domain: i64) -> String {
        if domain == 0 {
            return unpack_local(local);
        }
        format!("<{}{}", unpack_local(local), self.suffixes.get(&domain).map(String::as_str).unwrap_or("@?>"))
    }
}

fn pack_text(s: &str) -> Vec<u8> {
    let mut out = vec![TEXT];
    out.extend_from_slice(s.as_bytes());
    out
}

// ---------------------------------------------------------------- part bitmaps

/// Same as atlas's PartSet: one bit per part number.
fn insert_part(bits: &mut Vec<u8>, part: i64) {
    if !(0..=1_000_000).contains(&part) {
        return;
    }
    let (byte, bit) = ((part / 8) as usize, part % 8);
    if bits.len() <= byte {
        bits.resize(byte + 1, 0);
    }
    bits[byte] |= 1 << bit;
}

/// exactly parts 1..=expected, nothing else
fn is_exactly(bits: &[u8], expected: i64) -> bool {
    if expected <= 0 || bits.first().is_some_and(|b| b & 1 == 1) {
        return false;
    }
    let count: i64 = bits.iter().map(|b| i64::from(b.count_ones())).sum();
    let highest = bits.iter().rposition(|b| *b != 0).map(|i| i as i64 * 8 + 7 - i64::from(bits[i].leading_zeros()));
    count == expected && highest == Some(expected)
}

// ---------------------------------------------------------------- saving

/// Saves releases into a compact database, the same way
/// `atlas::db::save_release_batches` does into today's layout: upsert the
/// release, add its new articles, update its size / parts / completeness.
pub struct Writer {
    pub flavor: Flavor,
    domains: Domains,
}

impl Writer {
    pub fn new(flavor: Flavor) -> Writer {
        Writer { flavor, domains: Domains::default() }
    }

    pub fn save<'a>(&mut self, conn: &mut Connection, batches: impl IntoIterator<Item = &'a [Release]>) -> Result<()> {
        if self.flavor == Flavor::Packed {
            self.domains.load(conn)?;
        }
        let tx = conn.transaction()?;
        {
            let mut upsert = tx.prepare_cached(
                "insert into releases
                    (name, size, complete, group_name, poster, posted_date, display_name, is_obfuscated)
                    values (?, ?, ?, ?, ?, ?, ?, ?)
                    on conflict(name, group_name) do update set
                    poster = excluded.poster,
                    posted_date = excluded.posted_date,
                    display_name = coalesce(excluded.display_name, releases.display_name),
                    is_obfuscated = excluded.is_obfuscated
                    returning id, size, parts, file_total",
            )?;
            let mut get_file = tx.prepare_cached(
                "select id, subject, subject_part, subject_mid, expected, file_total, seen from files
                 where release_id = ? and filename = ?",
            )?;
            let mut new_file = tx.prepare_cached(
                "insert into files (release_id, filename, seen) values (?, ?, ?)",
            )?;
            let mut put_file = tx.prepare_cached(
                "update files set subject = ?, subject_part = ?, subject_mid = ?, expected = ?, file_total = ?, seen = ?
                 where id = ?",
            )?;
            let mut plain_segment = match self.flavor {
                Flavor::Plain => Some(tx.prepare_cached(
                    "insert or ignore into segments (file_id, message_id, part, bytes) values (?, ?, ?, ?)",
                )?),
                Flavor::Packed => None,
            };
            let mut packed_segment = match self.flavor {
                Flavor::Packed => Some(tx.prepare_cached(
                    "insert or ignore into segments (file_id, local, domain, part, bytes) values (?, ?, ?, ?, ?)",
                )?),
                Flavor::Plain => None,
            };
            let mut files_of = tx.prepare_cached("select expected, seen from files where release_id = ?")?;
            let mut update_stats =
                tx.prepare_cached("update releases set size = ?, complete = ?, parts = ?, file_total = ? where id = ?")?;

            for release in batches.into_iter().flatten() {
                type Saved = (i64, Option<i64>, Option<i64>, Option<i64>);
                let row: Option<Saved> = upsert
                    .query_row(
                        params![
                            release.name,
                            release.size,
                            release.complete as i64,
                            release.group,
                            release.poster,
                            release.date,
                            release.display_name,
                            release.is_obfuscated as i64,
                        ],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((release_id, old_size, old_parts, old_file_total)) = row else { continue };

                // articles by file, in arrival order
                let mut by_file: Vec<(&str, Vec<&Article>)> = Vec::new();
                for a in release.articles.iter().filter(|a| !a.message_id.is_empty()) {
                    let name = a.filename.as_deref().unwrap_or("");
                    match by_file.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, list)) => list.push(a),
                        None => by_file.push((name, vec![a])),
                    }
                }

                let mut added: Vec<&Article> = Vec::new();
                for (name, articles) in by_file {
                    type FileRow =
                        (i64, Option<String>, Option<i64>, Option<String>, Option<i64>, Option<i64>, Vec<u8>);
                    let existing: Option<FileRow> = get_file
                        .query_row(params![release_id, name], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                        })
                        .optional()?;
                    let (file_id, mut subject, mut subject_part, mut subject_mid, mut expected, mut file_total, mut seen) =
                        match existing {
                            Some(f) => f,
                            None => {
                                new_file.execute(params![release_id, name, Vec::<u8>::new()])?;
                                (tx.last_insert_rowid(), None, None, None, None, None, Vec::new())
                            }
                        };

                    let mut changed = false;
                    for a in articles {
                        let inserted = match self.flavor {
                            Flavor::Plain => plain_segment.as_mut().unwrap().execute(params![
                                file_id,
                                a.message_id,
                                a.part,
                                a.bytes
                            ])?,
                            Flavor::Packed => {
                                let (local, domain) = self.domains.encode(&tx, &a.message_id)?;
                                packed_segment.as_mut().unwrap().execute(params![file_id, local, domain, a.part, a.bytes])?
                            }
                        };
                        if inserted == 0 {
                            continue;
                        }
                        changed = true;
                        added.push(a);
                        // the nzb takes the subject of a file's first article, sorted
                        // by part (a missing part first) then message-id
                        let key = |part: Option<i64>, mid: &str| (part.is_some(), part.unwrap_or(0), mid.to_string());
                        if subject.is_none()
                            || key(a.part, &a.message_id) < key(subject_part, subject_mid.as_deref().unwrap_or(""))
                        {
                            subject = Some(a.subject.clone());
                            subject_part = a.part;
                            subject_mid = Some(a.message_id.clone());
                        }
                        if let Some(t) = a.total_parts {
                            expected = Some(expected.map_or(t, |e| e.max(t)));
                        }
                        if let Some(p) = a.part {
                            insert_part(&mut seen, p);
                        }
                        if let Some(ft) = a.file_total {
                            file_total = Some(file_total.map_or(ft, |e| e.max(ft)));
                        }
                    }
                    if changed {
                        put_file.execute(params![subject, subject_part, subject_mid, expected, file_total, seen, file_id])?;
                    }
                }

                if added.is_empty() {
                    continue;
                }

                let file_total = added.iter().filter_map(|a| a.file_total).chain(old_file_total).max();
                let old_parts = old_parts.unwrap_or(0);
                let earlier_size = if old_parts > 0 { old_size.unwrap_or(0) } else { 0 };

                let mut files = 0;
                let mut all_complete = true;
                let mut rows = files_of.query([release_id])?;
                while let Some(r) = rows.next()? {
                    files += 1;
                    let expected: Option<i64> = r.get(0)?;
                    let seen: Vec<u8> = r.get(1)?;
                    if !expected.is_some_and(|e| is_exactly(&seen, e)) {
                        all_complete = false;
                    }
                }
                let complete = all_complete && files > 0 && file_total.is_none_or(|ft| files == ft);

                update_stats.execute(params![
                    earlier_size + added.iter().map(|a| a.bytes).sum::<i64>(),
                    complete as i64,
                    old_parts + added.len() as i64,
                    file_total,
                    release_id
                ])?;
            }
        }
        tx.commit()
    }
}

// ---------------------------------------------------------------- reading

/// A release's articles in the order and shape `atlas::search::get_articles`
/// gives them, for building its NZB.
pub fn articles(conn: &Connection, flavor: Flavor, domains: &mut Domains, release_id: i64) -> Result<Vec<ArticleRow>> {
    let sql = match flavor {
        Flavor::Plain => {
            "select s.message_id, null, f.filename, s.part, f.expected, s.bytes, f.subject, r.poster, r.posted_date
             from files f join segments s on s.file_id = f.id join releases r on r.id = f.release_id
             where f.release_id = ? order by f.filename, s.part"
        }
        Flavor::Packed => {
            domains.load(conn)?;
            "select s.local, s.domain, f.filename, s.part, f.expected, s.bytes, f.subject, r.poster, r.posted_date
             from files f join segments s on s.file_id = f.id join releases r on r.id = f.release_id
             where f.release_id = ? order by f.filename, s.part"
        }
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([release_id], |r| {
        let message_id = match flavor {
            Flavor::Plain => r.get::<_, String>(0)?,
            Flavor::Packed => domains.decode(&r.get::<_, Vec<u8>>(0)?, r.get(1)?),
        };
        let filename: String = r.get(2)?;
        Ok(ArticleRow {
            message_id,
            filename: (!filename.is_empty()).then_some(filename),
            part: r.get(3)?,
            total_parts: r.get(4)?,
            bytes: r.get(5)?,
            subject: r.get(6)?,
            poster: r.get(7)?,
            posted_date: r.get(8)?,
        })
    })?;
    // today's query sorts by filename then part, ties in message-id order (the
    // order it reads them in); packed message-ids dont sort that way in sql
    let mut rows: Vec<ArticleRow> = rows.collect::<Result<_>>()?;
    rows.sort_by(|a, b| (&a.filename, a.part, &a.message_id).cmp(&(&b.filename, b.part, &b.message_id)));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_come_back_exactly() {
        let ids = [
            "<1064b678f3f54e28a5afd48a3a986076@ngPost>",
            "<DR59tDkIGMDKQS1YflogRq2MTqVgoHslO@aJQEZYp->",
            "<nnd$009a5634$43be2fd7@264e870e7f90d1fc>",
            "<ABCDEF0123@x>",
            "<abc@def@ghi>",
            "no-brackets@x",
            "<no-at-sign>",
            "<@empty-local>",
            "<odd123@x>",
            "",
        ];
        let conn = Connection::open_in_memory().unwrap();
        create(&conn, Flavor::Packed).unwrap();
        let mut domains = Domains::default();
        domains.load(&conn).unwrap();
        for id in ids {
            let (local, domain) = domains.encode(&conn, id).unwrap();
            assert_eq!(domains.decode(&local, domain), id);
        }
        // hex local parts take half the space
        assert_eq!(pack_local("1064b678f3f54e28a5afd48a3a986076").len(), 17);
    }
}
