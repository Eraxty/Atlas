//! Lenient RFC 2822 date handling, close to python's email.utils.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// Parse a usenet `Date:` header. Returns the wall clock time and its
/// utc offset in seconds (None when the zone is `-0000`/unknown).
pub fn parse_rfc2822(value: &str) -> Option<(NaiveDateTime, Option<i32>)> {
    // drop (comments) and commas, then walk tokens
    let mut cleaned = String::with_capacity(value.len());
    let mut depth = 0;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            ',' => cleaned.push(' '),
            _ => cleaned.push(c),
        }
    }

    let mut tokens: Vec<&str> = cleaned.split_whitespace().collect();

    // optional day name
    if tokens.first().is_some_and(|t| t.chars().all(|c| c.is_ascii_alphabetic())) {
        let t = tokens[0].to_ascii_lowercase();
        if !MONTHS.iter().any(|m| t.starts_with(m)) {
            tokens.remove(0);
        }
    }

    if tokens.len() < 4 {
        return None;
    }

    let month_of = |t: &str| {
        let t = t.to_ascii_lowercase();
        MONTHS.iter().position(|m| t.starts_with(m)).map(|i| i as u32 + 1)
    };

    // "2 Oct 2026" or the odd "Oct 2 2026"
    let (day, month) = match (tokens[0].parse::<u32>(), month_of(tokens[1])) {
        (Ok(d), Some(m)) => (d, m),
        _ => (tokens[1].parse::<u32>().ok()?, month_of(tokens[0])?),
    };

    let (year_tok, time_tok, rest) = if tokens[2].contains(':') {
        // "Oct 2 10:00:00 2026" style
        (tokens.get(3).copied()?, tokens[2], &tokens[4..])
    } else {
        (tokens[2], tokens[3], &tokens[4..])
    };

    let mut year: i32 = year_tok.parse().ok()?;
    if year_tok.len() <= 2 {
        year += if year > 68 { 1900 } else { 2000 };
    } else if year_tok.len() == 3 {
        year += 1900;
    }

    let mut hms = time_tok.split(':').map(|p| p.parse::<u32>());
    let h = hms.next()?.ok()?;
    let m = hms.next()?.ok()?;
    let s = match hms.next() {
        Some(v) => v.ok()?,
        None => 0,
    };

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_opt(h, m, s.min(59))?;

    let offset = rest.first().and_then(|z| parse_zone(z)).unwrap_or(None);

    Some((NaiveDateTime::new(date, time), offset))
}

fn parse_zone(z: &str) -> Option<Option<i32>> {
    let upper = z.to_ascii_uppercase();

    if upper == "-0000" {
        return Some(None);
    }

    if let Some(sign) = upper.strip_prefix('+').map(|r| (1, r)).or_else(|| upper.strip_prefix('-').map(|r| (-1, r))) {
        let (sign, digits) = sign;
        if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_digit()) {
            let hh: i32 = digits[..2].parse().ok()?;
            let mm: i32 = digits[2..].parse().ok()?;
            return Some(Some(sign * (hh * 3600 + mm * 60)));
        }
        return None;
    }

    let hours = match upper.as_str() {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "EST" => -5,
        "EDT" => -4,
        "CST" => -6,
        "CDT" => -5,
        "MST" => -7,
        "MDT" => -6,
        "PST" => -8,
        "PDT" => -7,
        _ => return None,
    };

    Some(Some(hours * 3600))
}

fn parse_iso(value: &str) -> Option<NaiveDateTime> {
    let value = value.trim();

    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, fmt) {
            return Some(dt);
        }
    }

    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok().map(|d| d.and_time(NaiveTime::MIN))
}

/// Header date -> `YYYY-MM-DD HH:MM:SS` (wall clock of the poster), or
/// the raw value when it cant be parsed.
pub fn to_iso_date(value: &str) -> String {
    match parse_rfc2822(value) {
        Some((dt, _)) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => value.to_string(),
    }
}

fn format_rfc2822(dt: &DateTime<FixedOffset>, negative_zero: bool) -> String {
    if negative_zero {
        dt.format("%a, %d %b %Y %H:%M:%S -0000").to_string()
    } else {
        dt.format("%a, %d %b %Y %H:%M:%S %z").to_string()
    }
}

/// Stored release date -> newznab `<pubDate>`. Naive dates are treated as utc.
pub fn pub_date(value: &str) -> String {
    let utc = FixedOffset::east_opt(0).unwrap();

    if let Some((dt, offset)) = parse_rfc2822(value) {
        let tz = offset.and_then(FixedOffset::east_opt).unwrap_or(utc);
        if let Some(aware) = tz.from_local_datetime(&dt).single() {
            return format_rfc2822(&aware, offset.is_none());
        }
    }

    if let Some(dt) = parse_iso(value) {
        return format_rfc2822(&utc.from_utc_datetime(&dt), false);
    }

    format_rfc2822(&Utc::now().fixed_offset(), false)
}

/// Stored release date -> unix timestamp for the nzb `date` attribute, now
/// when it cant be read.
pub fn article_timestamp(value: &str) -> i64 {
    posted_timestamp(value).unwrap_or_else(|| Utc::now().timestamp())
}

/// A header or stored date -> unix timestamp, None when it cant be read.
/// Naive dates are local time, same as python's datetime.timestamp().
pub fn posted_timestamp(value: &str) -> Option<i64> {
    if let Some((dt, offset)) = parse_rfc2822(value) {
        let ts = match offset.and_then(FixedOffset::east_opt) {
            Some(tz) => tz.from_local_datetime(&dt).single().map(|d| d.timestamp()),
            None => Local.from_local_datetime(&dt).earliest().map(|d| d.timestamp()),
        };
        if ts.is_some() {
            return ts;
        }
    }

    parse_iso(value).and_then(|dt| Local.from_local_datetime(&dt).earliest()).map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2822_variants() {
        assert_eq!(to_iso_date("Fri, 02 Oct 2026 10:11:12 +0200"), "2026-10-02 10:11:12");
        assert_eq!(to_iso_date("2 Oct 2026 10:11:12 GMT"), "2026-10-02 10:11:12");
        assert_eq!(to_iso_date("Fri, 2 Oct 26 10:11 +0000 (UTC)"), "2026-10-02 10:11:00");
        assert_eq!(to_iso_date("not a date"), "not a date");
        assert_eq!(to_iso_date(""), "");
    }

    #[test]
    fn pub_date_formats() {
        assert_eq!(pub_date("2026-10-02 10:11:12"), "Fri, 02 Oct 2026 10:11:12 +0000");
        assert_eq!(pub_date("Fri, 02 Oct 2026 10:11:12 +0200"), "Fri, 02 Oct 2026 10:11:12 +0200");
    }

    #[test]
    fn timestamp_with_offset() {
        assert_eq!(article_timestamp("Thu, 01 Jan 1970 01:00:00 +0100"), 0);
    }
}
