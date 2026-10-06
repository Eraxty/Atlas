use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use indexmap::IndexMap;

use crate::atomic::write_atomic;
use crate::dates::article_timestamp;
use crate::search::{ArticleRow, ReleaseRow, get_articles, get_release};
use crate::ui;

pub fn nzb_filename(name: &str, release_id: Option<i64>) -> String {
    let bad = "<>:\"/\\|?*";
    let safe: String = name.chars().map(|c| if bad.contains(c) || (c as u32) < 32 { '_' } else { c }).collect();
    let safe = safe.trim_matches([' ', '.']);
    let safe = if safe.is_empty() { "release" } else { safe };

    match release_id {
        Some(id) if id != 0 => format!("{safe}.{id}.nzb"),
        _ => format!("{safe}.nzb"),
    }
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if is_xml_char(c) => out.push(c),
            _ => {}
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#09;"),
            c if is_xml_char(c) => out.push(c),
            _ => {}
        }
    }
    out
}

/// xml 1.0 cant carry most control chars, even escaped
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

pub fn render_nzb(release: &ReleaseRow, articles: &[ArticleRow]) -> String {
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    x.push_str(
        "<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.1//EN\" \"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd\">\n",
    );
    x.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");
    let _ = writeln!(x, "  <head>\n    <meta type=\"category\">{}</meta>\n  </head>", escape_text(&release.group_name));

    // one <file> per physical file, articles are already sorted filename then part
    let mut by_filename: IndexMap<Option<&str>, Vec<&ArticleRow>> = IndexMap::new();
    for a in articles {
        by_filename.entry(a.filename.as_deref()).or_default().push(a);
    }

    for file_articles in by_filename.values() {
        let first = file_articles[0];
        let _ = writeln!(
            x,
            "  <file poster=\"{}\" date=\"{}\" subject=\"{}\">",
            escape_attr(first.poster.as_deref().unwrap_or("")),
            article_timestamp(first.posted_date.as_deref().unwrap_or("")),
            escape_attr(first.subject.as_deref().unwrap_or("")),
        );
        let _ = writeln!(x, "    <groups>\n      <group>{}</group>\n    </groups>", escape_text(&release.group_name));
        x.push_str("    <segments>\n");

        for a in file_articles {
            // nzb doesnt want the <>
            let _ = writeln!(
                x,
                "      <segment bytes=\"{}\" number=\"{}\">{}</segment>",
                a.bytes.unwrap_or(0),
                a.part.map(|p| p.to_string()).unwrap_or_else(|| "None".into()),
                escape_text(a.message_id.trim_matches(['<', '>'])),
            );
        }

        x.push_str("    </segments>\n  </file>\n");
    }

    x.push_str("</nzb>");
    x
}

pub fn build_nzb(release_id: i64) -> Result<Option<String>> {
    let Some(release) = get_release(release_id)? else { return Ok(None) };
    let articles = get_articles(release_id)?;

    if articles.is_empty() {
        return Ok(None);
    }

    Ok(Some(render_nzb(&release, &articles)))
}

/// Write the nzb for a release, into `output_dir` or the current dir.
pub fn generate_nzb(release_id: i64, output_dir: Option<&Path>) -> Result<PathBuf> {
    let release = get_release(release_id)?.ok_or_else(|| {
        ui::error("Release not found");
        anyhow!("release not found")
    })?;

    let Some(content) = build_nzb(release_id)? else {
        ui::error("No articles found");
        return Err(anyhow!("no articles found"));
    };

    let filename = nzb_filename(&release.name, Some(release.id));
    let target = match output_dir {
        Some(dir) => dir.join(&filename),
        None => PathBuf::from(&filename),
    };

    if let Err(e) = write_atomic(&target, content) {
        ui::error(&format!("couldnt save nzb: {e}"));
        return Err(e.into());
    }

    ui::success(&format!("Saved {}", target.display()));
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_safe() {
        assert_eq!(nzb_filename("a/b:c?.mkv", Some(7)), "a_b_c_.mkv.7.nzb");
        assert_eq!(nzb_filename(" .. ", None), "release.nzb");
        assert_eq!(nzb_filename("x\u{1}y", Some(0)), "x_y.nzb");
    }

    #[test]
    fn renders_files_and_segments() {
        let release = ReleaseRow {
            id: 1,
            name: "Rel".into(),
            group_name: "alt.binaries.test".into(),
            poster: None,
            posted_date: None,
            size: Some(3),
            complete: true,
            parts: Some(3),
        };
        let art = |file: &str, part: i64, id: &str| ArticleRow {
            message_id: id.into(),
            filename: Some(file.into()),
            part: Some(part),
            total_parts: Some(2),
            bytes: Some(1),
            subject: Some(format!("\"{file}\" yEnc ({part}/2)")),
            poster: Some("me <me@x>".into()),
            posted_date: Some("2026-10-02 10:00:00".into()),
        };
        let xml =
            render_nzb(&release, &[art("a.rar", 1, "<a1@x>"), art("a.rar", 2, "<a2@x>"), art("b.rar", 1, "<b1@x>")]);

        assert_eq!(xml.matches("<file ").count(), 2);
        assert!(xml.contains("<segment bytes=\"1\" number=\"2\">a2@x</segment>"));
        assert!(xml.contains("poster=\"me &lt;me@x&gt;\""));
        assert!(xml.contains("subject=\"&quot;a.rar&quot; yEnc (1/2)\""));
        assert!(xml.contains("<meta type=\"category\">alt.binaries.test</meta>"));
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE nzb"));
    }
}
