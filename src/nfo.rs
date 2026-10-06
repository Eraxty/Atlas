//! Pull a release name out of an .nfo file.

use std::sync::LazyLock;

use regex::Regex;

static NFO_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\.(?:nfo|txt)(?:"|\s|$)"#).unwrap());
static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<(?:title|name|release(?:name)?)>\s*(.*?)\s*</").unwrap());
static LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:release\s*name|release|title|movie|series)\s*:\s*(.+?)\s*$").unwrap());
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static FILLER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-=_*# .]+$").unwrap());

pub fn is_nfo(subject: &str) -> bool {
    NFO_RE.is_match(subject)
}

pub fn display_name(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }

    let text = String::from_utf8_lossy(data);
    let mut candidates: Vec<String> = TAG_RE.captures_iter(&text).map(|c| c[1].to_string()).collect();

    for line in text.split(['\n', '\r', '\x0b', '\x0c']) {
        if let Some(c) = LINE_RE.captures(line) {
            candidates.push(c[1].to_string());
        }
    }

    candidates
        .into_iter()
        .map(|c| WS_RE.replace_all(&c, " ").trim_matches([' ', '.', '\t', '\r', '\n']).to_string())
        .find(|name| valid_name(name))
}

pub fn valid_name(name: &str) -> bool {
    let len = name.chars().count();

    if !(3..=240).contains(&len) {
        return false;
    }

    if FILLER_RE.is_match(name) {
        return false;
    }

    !matches!(name.to_lowercase().as_str(), "nfo" | "release" | "title" | "unknown" | "none")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_nfo_subjects() {
        assert!(is_nfo(r#""abc.nfo" yEnc (1/1)"#));
        assert!(is_nfo("abc.TXT"));
        assert!(!is_nfo(r#""abc.nfox" yEnc"#));
    }

    #[test]
    fn finds_names() {
        assert_eq!(display_name(b"<title> Cool.Movie.2024 </title>").as_deref(), Some("Cool.Movie.2024"));
        assert_eq!(
            display_name(b"=====\n  Release Name :  Some   Show S01E01  \nfoo").as_deref(),
            Some("Some Show S01E01")
        );
        assert_eq!(display_name(b"title: ----\nmovie: Real Name..").as_deref(), Some("Real Name"));
        assert_eq!(display_name(b"title: none"), None);
        assert_eq!(display_name(b""), None);
    }
}
