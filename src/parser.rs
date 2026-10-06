use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

/// most parts a file can claim. a subject claiming more is forged: the part
/// bitmap grows with the part number, so it would cost storage per header
pub const MAX_PARTS: i64 = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Article {
    pub number: u64,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub message_id: String,
    pub references: String,
    pub bytes: i64,
    pub lines: i64,
    pub filename: Option<String>,
    pub release_name: Option<String>,
    pub part: Option<i64>,
    pub total_parts: Option<i64>,
    pub file_index: Option<i64>,
    pub file_total: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSubject {
    pub filename: String,
    pub release_name: String,
    pub part: i64,
    pub total_parts: i64,
    pub file_index: Option<i64>,
    pub file_total: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Release {
    pub name: String,
    pub articles: Vec<Article>,
    pub size: i64,
    pub is_obfuscated: bool,
    pub display_name: Option<String>,
    pub complete: bool,
    pub group: String,
    pub poster: String,
    pub date: String,
}

// every client posts different subjects soo try all these patterns, in order
static SUBJECT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#""(.+?)"\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#"\[\d+\s*/\s*\d+\]\s+"(.+?)"\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#"\[\d+\s*/\s*\d+\]\s+-\s+"(.+?)"\s+-\s+[\d.]+\s+\w+\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#"yEnc\s+"(.+?)"\s+[\d.]+\s+\w+\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#""(.+?)"\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#"([^\s"]+\.[^\s"]+)\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#"([^\s"]+\.[^\s"]+)\s+\((\d+)\s*/\s*(\d+)\)"#,
        r#"\[\d+\s*/\s*\d+\]\s+(?:-\s+)?([^\s"]+\.[^\s"]+)\s+yEnc"#,
        r#"\[\d+\s*/\s*\d+\]\s+(?:-\s+)?([^\s"]+\.[^\s"]+)\s+\("#,
        // python had a (?=\s|$) lookahead here, consuming it is equivalent for a search
        r#"\[\d+\s*/\s*\d+\]\s+(?:-\s+)?["]([^"]+)["](?:\s|$)"#,
    ]
    .iter()
    .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
    .collect()
});

static FILE_COUNT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[(\d+)\s*/\s*(\d+)\]").unwrap());

static RELEASE_SUFFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.(part\d+|r\d+|vol\d+\+\d+|par2|nfo|sfv|rar|0\d\d)$").unwrap());

static OBFUSCATED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{16,}$").unwrap());

pub fn is_obfuscated(name: &str) -> bool {
    OBFUSCATED_RE.is_match(name)
}

pub fn parse_subject(subject: &str) -> Option<ParsedSubject> {
    if subject.is_empty() {
        return None;
    }

    let caps = SUBJECT_PATTERNS.iter().find_map(|re| re.captures(subject))?;
    let filename = caps.get(1)?.as_str().to_string();

    // strip .part1/.r00 type shi soo every part of a release maps to one name
    let mut release_name = filename.clone();
    loop {
        let stripped = RELEASE_SUFFIX_RE.replace(&release_name, "").into_owned();
        if stripped == release_name {
            break;
        }
        release_name = stripped;
    }

    // some formats only give a filename, no part numbers
    let (part, total_parts) = match (caps.get(2), caps.get(3)) {
        (Some(p), Some(t)) => (p.as_str().parse().ok()?, t.as_str().parse().ok()?),
        _ => (1, 1),
    };
    // a part past the file's own total, or past any real file, is forged
    if part > MAX_PARTS || total_parts > MAX_PARTS || (total_parts != 0 && part > total_parts) {
        return None;
    }

    let (mut file_index, mut file_total) = (None, None);

    if let Some(c) = FILE_COUNT_RE.captures(subject.trim()) {
        file_index = c[1].parse().ok();
        file_total = c[2].parse().ok();
    }

    if let Some(total) = file_total.filter(|t| *t != 0)
        && release_name.chars().count() <= 2
    {
        release_name = format!("{release_name} [{total}]");
    }

    Some(ParsedSubject { filename, release_name, part, total_parts, file_index, file_total })
}

/// Posts where every file is a single article with only a `[n/m]` counter
/// are really one file split across articles. Turn the counter into part numbers.
fn remap_bracket_parts(articles: &mut [Article]) {
    let mut by_filename: IndexMap<Option<String>, Vec<usize>> = IndexMap::new();

    for (i, a) in articles.iter().enumerate() {
        by_filename.entry(a.filename.clone()).or_default().push(i);
    }

    for idxs in by_filename.values() {
        if idxs.len() < 2 {
            continue;
        }

        if idxs.iter().any(|&i| articles[i].total_parts != Some(1)) {
            continue;
        }

        if idxs.iter().any(|&i| articles[i].file_index.is_none()) {
            continue;
        }

        // a bracket counter past any real file is forged, not a part number
        if idxs.iter().any(|&i| articles[i].file_index.is_some_and(|n| n > MAX_PARTS)) {
            continue;
        }

        let total = idxs
            .iter()
            .map(|&i| articles[i].file_total.unwrap_or(0))
            .chain(std::iter::once(idxs.len() as i64))
            .max()
            .unwrap_or(0);
        if total > MAX_PARTS {
            continue;
        }

        for &i in idxs {
            let a = &mut articles[i];
            a.part = a.file_index;
            a.total_parts = Some(total);
            a.file_index = None;
            a.file_total = None;
        }
    }
}

/// Bucket articles into releases by their parsed release name, keeping
/// first-seen order.
pub fn group_articles(articles: Vec<Article>) -> IndexMap<String, Release> {
    let total = articles.len();
    let mut releases: IndexMap<String, Release> = IndexMap::new();
    let mut dropped = 0;

    for mut article in articles {
        let Some(parsed) = parse_subject(&article.subject) else {
            dropped += 1;
            continue;
        };

        let release = releases.entry(parsed.release_name.clone()).or_insert_with(|| Release {
            name: parsed.release_name.clone(),
            is_obfuscated: is_obfuscated(&parsed.release_name),
            ..Default::default()
        });

        // stash the parsed bits on the article soo the nzb builder can use em later
        article.filename = Some(parsed.filename);
        article.release_name = Some(parsed.release_name);
        article.part = Some(parsed.part);
        article.total_parts = Some(parsed.total_parts);
        article.file_index = parsed.file_index;
        article.file_total = parsed.file_total;

        release.size += article.bytes;
        release.articles.push(article);
    }

    for release in releases.values_mut() {
        remap_bracket_parts(&mut release.articles);
    }

    if dropped > 0 {
        println!("Dropped {dropped}/{total} unparsable");
    }

    releases
}

pub fn is_complete(articles: &[Article]) -> bool {
    if articles.is_empty() {
        return false;
    }

    // split parts per file soo each file is checked on its own
    let mut by_filename: HashMap<Option<&str>, Vec<&Article>> = HashMap::new();
    for a in articles {
        by_filename.entry(a.filename.as_deref()).or_default().push(a);
    }

    for file_articles in by_filename.values() {
        let Some(expected) = file_articles.iter().filter_map(|a| a.total_parts).max() else {
            return false;
        };

        // python compared {a.part} (which can hold None) to range(1, n+1)
        let mut parts: BTreeSet<Option<i64>> = BTreeSet::new();
        parts.extend(file_articles.iter().map(|a| a.part));
        let wanted: BTreeSet<Option<i64>> = (1..=expected).map(Some).collect();

        if parts != wanted {
            return false;
        }
    }

    let file_totals: BTreeSet<i64> = articles.iter().filter_map(|a| a.file_total).collect();

    if file_totals.len() == 1 {
        let expected_files = *file_totals.iter().next().unwrap();
        if by_filename.len() as i64 != expected_files {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(subject: &str, bytes: i64) -> Article {
        Article { subject: subject.into(), bytes, message_id: format!("<{subject}>"), ..Default::default() }
    }

    #[test]
    fn parses_quoted_yenc() {
        let p = parse_subject(r#"[01/10] - "Movie.2024.1080p.part01.rar" yEnc (1/50)"#).unwrap();
        assert_eq!(p.filename, "Movie.2024.1080p.part01.rar");
        assert_eq!(p.release_name, "Movie.2024.1080p");
        assert_eq!((p.part, p.total_parts), (1, 50));
        assert_eq!((p.file_index, p.file_total), (Some(1), Some(10)));
    }

    #[test]
    fn forged_part_numbers_are_unparsable() {
        assert!(parse_subject(r#""a.rar" yEnc (1000000/1)"#).is_none());
        assert!(parse_subject(r#""a.rar" yEnc (2/1)"#).is_none());
        assert!(parse_subject(r#""a.rar" yEnc (1/1000000)"#).is_none());
        assert!(parse_subject(r#""a.rar" yEnc (1/1)"#).is_some());
    }

    #[test]
    fn forged_headers_dont_grow_seen_blobs() {
        let mut f = crate::store::FileState::default();
        for i in 0..1000 {
            let subject = format!(r#""a.rar" yEnc (1000000/1) {i}"#);
            if let Some(p) = parse_subject(&subject) {
                f.add(&Article { part: Some(p.part), total_parts: Some(p.total_parts), ..Default::default() });
            }
        }
        assert!(f.seen.is_empty());
    }

    #[test]
    fn strips_stacked_suffixes() {
        let p = parse_subject(r#""show.s01e01.vol03+04.par2" yEnc (2/3)"#).unwrap();
        assert_eq!(p.release_name, "show.s01e01");
    }

    #[test]
    fn bare_filename_without_parts() {
        let p = parse_subject("[3/7] - file.name.mkv yEnc").unwrap();
        assert_eq!(p.filename, "file.name.mkv");
        assert_eq!((p.part, p.total_parts), (1, 1));
        assert_eq!((p.file_index, p.file_total), (Some(3), Some(7)));
    }

    #[test]
    fn last_pattern_quoted_bracket() {
        let p = parse_subject(r#"[1/2] "abc def""#).unwrap();
        assert_eq!(p.filename, "abc def");
        assert!(parse_subject(r#"[1/2] "abc"def"#).is_none());
    }

    #[test]
    fn short_names_get_file_total() {
        let p = parse_subject(r#"[1/4] "a.rar" yEnc (1/1)"#).unwrap();
        assert_eq!(p.release_name, "a [4]");
    }

    #[test]
    fn unparsable() {
        assert!(parse_subject("hello world").is_none());
        assert!(parse_subject("").is_none());
    }

    #[test]
    fn obfuscation() {
        assert!(is_obfuscated("a1B2c3D4e5F6g7H8"));
        assert!(!is_obfuscated("a1B2c3D4e5F6g7H"));
        assert!(!is_obfuscated("Movie.2024.1080p.WEB"));
    }

    #[test]
    fn grouping_and_completeness() {
        let arts = vec![
            art(r#"[1/2] - "rel.part1.rar" yEnc (1/2)"#, 10),
            art(r#"[1/2] - "rel.part1.rar" yEnc (2/2)"#, 10),
            art(r#"[2/2] - "rel.part2.rar" yEnc (1/1)"#, 5),
            art("garbage", 1),
        ];
        let rels = group_articles(arts);
        assert_eq!(rels.len(), 1);
        let r = &rels["rel"];
        assert_eq!(r.size, 25);
        assert!(is_complete(&r.articles));

        let rels = group_articles(vec![art(r#"[1/2] - "why.part1.rar" yEnc (1/2)"#, 10)]);
        assert!(!is_complete(&rels["why"].articles));
    }

    #[test]
    fn bracket_parts_remap() {
        let arts = vec![
            art(r#"[1/3] - "same.bin" yEnc (1/1)"#, 1),
            art(r#"[2/3] - "same.bin" yEnc (1/1)"#, 1),
            art(r#"[3/3] - "same.bin" yEnc (1/1)"#, 1),
        ];
        let rels = group_articles(arts);
        let r = &rels["same.bin"];
        let parts: Vec<_> = r.articles.iter().map(|a| (a.part, a.total_parts, a.file_total)).collect();
        assert_eq!(parts, vec![(Some(1), Some(3), None), (Some(2), Some(3), None), (Some(3), Some(3), None)]);
        assert!(is_complete(&r.articles));
    }
}
