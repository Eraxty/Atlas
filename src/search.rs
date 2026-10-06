use rusqlite::{Connection, OptionalExtension, Row, params_from_iter, types::Value};

use crate::db::{self, Result};
use crate::store;

/// One search hit. `name` is the display name when we have one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRow {
    pub id: i64,
    pub name: String,
    pub group_name: String,
    pub poster: Option<String>,
    pub posted_date: Option<String>,
    pub size: Option<i64>,
    pub complete: bool,
    pub parts: Option<i64>,
}

/// One article of a release, joined with its release's poster/date.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArticleRow {
    pub message_id: String,
    pub filename: Option<String>,
    pub part: Option<i64>,
    pub total_parts: Option<i64>,
    pub bytes: Option<i64>,
    pub subject: Option<String>,
    pub poster: Option<String>,
    pub posted_date: Option<String>,
}

const COLUMNS: &str =
    "r.id, coalesce(r.display_name, r.name), r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts";

/// hide obfuscated junk unless we managed to get a real name for it
const VISIBLE_RELEASE: &str = "(
    r.display_name is not null
    or (r.is_obfuscated = 0 and (length(r.name) < 16 or r.name glob '*[^A-Za-z0-9]*'))
)";

fn release_row(r: &Row) -> rusqlite::Result<ReleaseRow> {
    Ok(ReleaseRow {
        id: r.get(0)?,
        name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        group_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        poster: r.get(3)?,
        posted_date: r.get(4)?,
        size: r.get(5)?,
        complete: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
        parts: r.get(7)?,
    })
}

pub fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|raw| raw.trim_matches('"').replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub fn like_query(query: &str) -> String {
    let q = query.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{q}%")
}

fn query_rows(conn: &Connection, sql: &str, vals: &[Value]) -> Result<Vec<ReleaseRow>> {
    let mut stmt = conn.prepare(sql)?;

    stmt.query_map(params_from_iter(vals.iter()), release_row)?.collect()
}

/// try fts first, fall back to LIKE if fts chokes on the query
fn fts_or_like_rows(fts: (&str, Vec<Value>), like: (&str, Vec<Value>)) -> Result<Vec<ReleaseRow>> {
    let conn = db::open()?;
    query_rows(&conn, fts.0, &fts.1).or_else(|_| query_rows(&conn, like.0, &like.1))
}

fn fts_or_like_count(fts: (&str, Vec<Value>), like: (&str, Vec<Value>)) -> Result<i64> {
    let conn = db::open()?;
    let count = |sql: &str, vals: &[Value]| conn.query_row(sql, params_from_iter(vals.iter()), |r| r.get(0));
    count(fts.0, &fts.1).or_else(|_| count(like.0, &like.1))
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

/// The shards to look in: just the group's when there is one.
fn shards(groups: Option<&[&str]>) -> Vec<usize> {
    match groups {
        Some(list) => {
            let mut s: Vec<usize> = list.iter().map(|g| store::shard_of(g)).collect();
            s.sort_unstable();
            s.dedup();
            s
        }
        None => (0..store::SHARDS).collect(),
    }
}

/// `part` for each of `shards`, joined with union all
fn union(shards: &[usize], part: impl Fn(usize) -> String) -> String {
    let parts: Vec<String> = shards.iter().map(|&i| part(i)).collect();
    if parts.is_empty() { "select null where 0".into() } else { parts.join("\nunion all\n") }
}

/// Search within one group (`Some`) or all of them (`None`). `page` is 0 based.
///
/// Parameters: ?1 the query, ?2 the group, ?3 limit, ?4 offset.
pub fn search(query: &str, group: Option<&str>, page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let group_filter = if group.is_some() { "r.group_name = ?2 and " } else { "" };
    let in_shards = shards(group.map(|g| [g]).as_ref().map(|a| a.as_slice()));

    // fts tables hold the text, the release data lives in releases
    let fts_sql = format!(
        "select * from ({})
        order by rank
        limit ?3 offset ?4",
        union(&in_shards, |i| format!(
            "select {COLUMNS}, ranked.rank as rank
            from s{i}.releases r
            join (select rowid, bm25(releases_fts) as rank from s{i}.releases_fts where releases_fts match ?1) ranked
                on ranked.rowid = r.id
            where {group_filter}{VISIBLE_RELEASE}"
        ))
    );

    let like_sql = format!(
        "select * from ({})
        order by sort_name
        limit ?3 offset ?4",
        union(&in_shards, |i| format!(
            "select {COLUMNS}, r.name as sort_name
            from s{i}.releases r
            where (r.name like ?1 escape '\\' or r.display_name like ?1 escape '\\')
            and {group_filter}{VISIBLE_RELEASE}"
        ))
    );

    let group_value = group.map(text).unwrap_or(Value::Null);
    let tail = [group_value, Value::Integer(page_size), Value::Integer(page * page_size)];
    let fts_vals: Vec<Value> = std::iter::once(text(fts_query(query))).chain(tail.clone()).collect();
    let like_vals: Vec<Value> = std::iter::once(text(like_query(query))).chain(tail).collect();

    fts_or_like_rows((&fts_sql, fts_vals), (&like_sql, like_vals))
}

/// Parameters: ?1 the query, ?2 the group.
pub fn count(query: &str, group: Option<&str>) -> Result<i64> {
    if query.trim().is_empty() {
        return Ok(0);
    }

    let group_filter = if group.is_some() { "r.group_name = ?2 and " } else { "" };
    let in_shards = shards(group.map(|g| [g]).as_ref().map(|a| a.as_slice()));

    let fts_sql = format!(
        "select coalesce(sum(c), 0) from ({})",
        union(&in_shards, |i| format!(
            "select count(*) as c from s{i}.releases r
            join (select rowid from s{i}.releases_fts where releases_fts match ?1) matches on matches.rowid = r.id
            where {group_filter}{VISIBLE_RELEASE}"
        ))
    );

    let like_sql = format!(
        "select coalesce(sum(c), 0) from ({})",
        union(&in_shards, |i| format!(
            "select count(*) as c from s{i}.releases r
            where (r.name like ?1 escape '\\' or r.display_name like ?1 escape '\\')
            and {group_filter}{VISIBLE_RELEASE}"
        ))
    );

    // ?2 is only in the sql with a group
    let mut fts_vals = vec![text(fts_query(query))];
    let mut like_vals = vec![text(like_query(query))];
    if let Some(g) = group {
        fts_vals.push(text(g));
        like_vals.push(text(g));
    }

    fts_or_like_count((&fts_sql, fts_vals), (&like_sql, like_vals))
}

pub fn search_releases(query: &str, group: &str, page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    search(query, Some(group), page, page_size)
}

pub fn search_all_releases(query: &str, page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    search(query, None, page, page_size)
}

pub fn count_releases(query: &str, group: &str) -> Result<i64> {
    count(query, Some(group))
}

pub fn count_all_releases(query: &str) -> Result<i64> {
    count(query, None)
}

/// The newest releases matching `filter` (sql on `r`), across `in_shards`:
/// each shard's newest page worth, then merged. Ids grow in the order releases
/// were added whatever the shard, soo newest first is id order.
/// Parameters: ?1 limit, ?2 offset, then `extra` from ?3.
fn newest(in_shards: &[usize], filter: &str, extra: Vec<Value>, page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    let conn = db::open()?;
    let sql = format!(
        "select * from ({}) order by 1 desc limit ?1 offset ?2",
        union(in_shards, |i| format!(
            "select * from (select {COLUMNS} from s{i}.releases r where {filter} order by r.id desc limit ?1 + ?2)"
        ))
    );
    let mut vals = vec![Value::Integer(page_size), Value::Integer(page * page_size)];
    vals.extend(extra);
    query_rows(&conn, &sql, &vals)
}

pub fn all_releases(page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    newest(&shards(None), VISIBLE_RELEASE, Vec::new(), page, page_size)
}

pub fn search_obfuscated(page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    newest(&shards(None), "r.is_obfuscated = 1", Vec::new(), page, page_size)
}

pub fn count_obfuscated() -> Result<i64> {
    let sql = format!(
        "select coalesce(sum(c), 0) from ({})",
        union(&shards(None), |i| format!("select count(*) as c from s{i}.releases where is_obfuscated = 1"))
    );
    db::open()?.query_row(&sql, [], |r| r.get(0))
}

pub fn get_release(id: i64) -> Result<Option<ReleaseRow>> {
    get_release_with(&db::open()?, id)
}

/// `get_release` on a connection with the shards attached.
pub fn get_release_with(conn: &Connection, id: i64) -> Result<Option<ReleaseRow>> {
    let sql = format!("select {COLUMNS} from s{}.releases r where r.id = ?", store::shard_of_id(id));
    conn.query_row(&sql, [id], release_row).optional()
}

pub fn recent_in_groups(groups: &[String], page: i64, page_size: i64) -> Result<Vec<ReleaseRow>> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }

    let names: Vec<&str> = groups.iter().map(String::as_str).collect();
    let qs = (0..groups.len()).map(|n| format!("?{}", n + 3)).collect::<Vec<_>>().join(",");
    let extra = groups.iter().map(|g| text(g.as_str())).collect();
    newest(&shards(Some(&names)), &format!("r.group_name in ({qs})"), extra, page, page_size)
}

/// Articles ordered by filename then part soo files can be reassembled.
pub fn get_articles(release_id: i64) -> Result<Vec<ArticleRow>> {
    store::articles(&db::open()?, release_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_terms() {
        assert_eq!(fts_query(r#"the "matrix" 1999"#), r#""the"* AND "matrix"* AND "1999"*"#);
        assert_eq!(fts_query("   "), "");
    }

    #[test]
    fn like_query_escapes() {
        assert_eq!(like_query(" 50%_off\\ "), "%50\\%\\_off\\\\%");
    }
}
