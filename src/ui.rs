//! Tiny stand-in for python `rich`: colored markup, panels, tables and a
//! readline prompt.

use std::io::{self, IsTerminal, Write};
use std::sync::{LazyLock, Mutex};

use ratatui::crossterm::terminal;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<u8>,
    pub bold: bool,
    pub dim: bool,
}

impl Style {
    pub const PLAIN: Style = Style { fg: None, bold: false, dim: false };

    pub fn parse(spec: &str) -> Option<Style> {
        let mut style = Style::PLAIN;
        for word in spec.split_whitespace() {
            match word {
                "bold" => style.bold = true,
                "dim" => style.dim = true,
                w => style.fg = Some(color_code(w)?),
            }
        }
        Some(style)
    }

    fn merge(self, other: Style) -> Style {
        Style { fg: other.fg.or(self.fg), bold: self.bold || other.bold, dim: self.dim || other.dim }
    }

    fn ansi(self) -> String {
        if self == Style::PLAIN {
            return String::new();
        }
        let mut codes = Vec::new();
        if self.bold {
            codes.push("1".to_string());
        }
        if self.dim {
            codes.push("2".to_string());
        }
        if let Some(c) = self.fg {
            codes.push(c.to_string());
        }
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn color_code(name: &str) -> Option<u8> {
    Some(match name {
        "black" => 30,
        "red" => 31,
        "green" => 32,
        "yellow" => 33,
        "blue" => 34,
        "magenta" => 35,
        "cyan" => 36,
        "white" => 37,
        "bright_black" | "grey" | "gray" => 90,
        "bright_red" => 91,
        "bright_green" => 92,
        "bright_yellow" => 93,
        "bright_cyan" => 96,
        _ => return None,
    })
}

pub fn color_enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal() && {
            #[cfg(windows)]
            {
                ratatui::crossterm::ansi_support::supports_ansi()
            }
            #[cfg(not(windows))]
            {
                true
            }
        }
    });
    *ENABLED
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line(pub Vec<(String, Style)>);

impl Line {
    pub fn plain(s: impl Into<String>) -> Line {
        Line(vec![(s.into(), Style::PLAIN)])
    }

    pub fn styled(s: impl Into<String>, style: &str) -> Line {
        Line(vec![(s.into(), Style::parse(style).unwrap_or_default())])
    }

    pub fn push(&mut self, s: impl Into<String>, style: &str) -> &mut Self {
        self.0.push((s.into(), Style::parse(style).unwrap_or_default()));
        self
    }

    pub fn append(&mut self, other: Line) -> &mut Self {
        self.0.extend(other.0);
        self
    }

    pub fn width(&self) -> usize {
        self.0.iter().map(|(s, _)| s.width()).sum()
    }

    pub fn text(&self) -> String {
        self.0.iter().map(|(s, _)| s.as_str()).collect()
    }

    /// cut to `max` columns, ending in "..." when something was dropped
    pub fn truncate(&self, max: usize) -> Line {
        if self.width() <= max {
            return self.clone();
        }

        let budget = max.saturating_sub(3);
        let mut used = 0;
        let mut out = Line::default();

        'outer: for (s, style) in &self.0 {
            let mut piece = String::new();
            for c in s.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > budget {
                    out.0.push((piece, *style));
                    break 'outer;
                }
                used += w;
                piece.push(c);
            }
            out.0.push((piece, *style));
        }

        out.0.push(("...".chars().take(max.min(3)).collect(), Style::PLAIN));
        out
    }

    pub fn render(&self) -> String {
        let color = color_enabled();
        let mut out = String::new();
        for (s, style) in &self.0 {
            if color && *style != Style::PLAIN {
                out.push_str(&style.ansi());
                out.push_str(s);
                out.push_str("\x1b[0m");
            } else {
                out.push_str(s);
            }
        }
        out
    }
}

/// Parse rich-style markup like `[bold cyan]hi[/bold cyan]` into lines.
/// Brackets that arent a known style stay literal (`[enter]`, `[broken]`).
pub fn markup(text: &str) -> Vec<Line> {
    let mut lines = vec![Line::default()];
    let mut stack: Vec<Style> = vec![Style::PLAIN];
    let mut buf = String::new();
    let mut rest = text;

    let flush = |buf: &mut String, lines: &mut Vec<Line>, style: Style| {
        if buf.is_empty() {
            return;
        }
        for (i, part) in buf.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            if !part.is_empty() {
                lines.last_mut().unwrap().0.push((part.to_string(), style));
            }
        }
        buf.clear();
    };

    while let Some(open) = rest.find('[') {
        buf.push_str(&rest[..open]);
        let after = &rest[open + 1..];

        let Some(close) = after.find(']') else {
            buf.push_str(&rest[open..]);
            rest = "";
            break;
        };

        let tag = &after[..close];
        let current = *stack.last().unwrap();

        if let Some(closing) = tag.strip_prefix('/') {
            if (closing.is_empty() || Style::parse(closing).is_some()) && stack.len() > 1 {
                flush(&mut buf, &mut lines, current);
                stack.pop();
                rest = &after[close + 1..];
                continue;
            }
        } else if !tag.trim().is_empty()
            && let Some(style) = Style::parse(tag)
        {
            flush(&mut buf, &mut lines, current);
            stack.push(current.merge(style));
            rest = &after[close + 1..];
            continue;
        }

        buf.push('[');
        rest = after;
    }

    buf.push_str(rest);
    let style = *stack.last().unwrap();
    flush(&mut buf, &mut lines, style);
    lines
}

pub fn print(text: &str) {
    for line in markup(text) {
        println!("{}", line.render());
    }
}

pub fn print_lines(lines: &[Line]) {
    for line in lines {
        println!("{}", line.render());
    }
}

pub fn error(msg: &str) {
    print_lines(&[Line::styled(msg, "red")]);
}

pub fn success(msg: &str) {
    print_lines(&[Line::styled(msg, "green")]);
}

pub fn warn(msg: &str) {
    print_lines(&[Line::styled(msg, "yellow")]);
}

pub fn dim(msg: &str) {
    print_lines(&[Line::styled(msg, "dim")]);
}

pub fn clear() {
    if io::stdout().is_terminal() {
        print!("\x1b[2J\x1b[3J\x1b[H");
        let _ = io::stdout().flush();
    }
}

pub fn term_size() -> (usize, usize) {
    terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((80, 24))
}

/// Rounded box around `lines`, full terminal width.
pub fn panel(lines: &[Line], border: &str) -> Vec<Line> {
    let width = term_size().0.max(20);
    let inner = width - 4;
    let border = Style::parse(border).unwrap_or_default();

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(Line(vec![(format!("╭{}╮", "─".repeat(width - 2)), border)]));

    for line in lines {
        let line = line.truncate(inner);
        let pad = inner.saturating_sub(line.width());
        let mut row = Line(vec![("│ ".into(), border)]);
        row.0.extend(line.0);
        row.0.push((format!("{} ", " ".repeat(pad)), Style::PLAIN));
        row.0.push(("│".into(), border));
        out.push(row);
    }

    out.push(Line(vec![(format!("╰{}╯", "─".repeat(width - 2)), border)]));
    out
}

pub fn print_panel(text: &str, border: &str) {
    print_lines(&panel(&markup(text), border));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Left,
    Right,
}

pub struct Column {
    pub header: String,
    pub width: Option<usize>,
    pub justify: Justify,
    /// flexible columns shrink first when the table is too wide
    pub flex: bool,
    pub style: Style,
}

impl Column {
    pub fn new(header: &str) -> Column {
        Column { header: header.into(), width: None, justify: Justify::Left, flex: false, style: Style::PLAIN }
    }

    pub fn width(mut self, w: usize) -> Self {
        self.width = Some(w);
        self
    }

    pub fn right(mut self) -> Self {
        self.justify = Justify::Right;
        self
    }

    pub fn flex(mut self) -> Self {
        self.flex = true;
        self
    }

    pub fn style(mut self, s: &str) -> Self {
        self.style = Style::parse(s).unwrap_or_default();
        self
    }
}

pub struct Table {
    pub title: Option<String>,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Line>>,
    pub show_header: bool,
    pub header_style: Style,
    pub indent: usize,
    pub gap: usize,
}

impl Table {
    pub fn new(columns: Vec<Column>) -> Table {
        Table {
            title: None,
            columns,
            rows: Vec::new(),
            show_header: true,
            header_style: Style::parse("bold cyan").unwrap(),
            indent: 2,
            gap: 2,
        }
    }

    /// two column numbered menu like `1.  Start Indexing`
    pub fn menu(items: &[(&str, &str)]) -> Table {
        let mut t =
            Table::new(vec![Column::new("num").width(3).style("bold cyan"), Column::new("label").style("white")]);
        t.show_header = false;
        for (k, v) in items {
            t.add_row(vec![Line::plain(*k), Line::plain(*v)]);
        }
        t
    }

    pub fn title(mut self, t: impl Into<String>) -> Self {
        self.title = Some(t.into());
        self
    }

    pub fn no_header(mut self) -> Self {
        self.show_header = false;
        self
    }

    pub fn add_row(&mut self, cells: Vec<Line>) {
        self.rows.push(cells);
    }

    pub fn add_text_row(&mut self, cells: &[&str]) {
        self.rows.push(cells.iter().map(|c| Line::plain(*c)).collect());
    }

    pub fn render(&self, max_width: usize) -> Vec<Line> {
        let n = self.columns.len();
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let content = self.rows.iter().filter_map(|r| r.get(i)).map(Line::width).max().unwrap_or(0);
                let header = if self.show_header { c.header.width() } else { 0 };
                c.width.unwrap_or(0).max(content.max(header))
            })
            .collect();

        let overhead = self.indent + self.gap * n.saturating_sub(1);
        let total = |w: &[usize]| w.iter().sum::<usize>() + overhead;

        // shrink flexible (then any) columns until it fits
        while total(&widths) > max_width {
            let pick = (0..n)
                .filter(|&i| self.columns[i].flex && widths[i] > 8)
                .max_by_key(|&i| widths[i])
                .or_else(|| (0..n).filter(|&i| widths[i] > 8).max_by_key(|&i| widths[i]));
            match pick {
                Some(i) => widths[i] -= 1,
                None => break,
            }
        }

        let mut out = Vec::new();

        if let Some(title) = &self.title {
            let w = total(&widths);
            let pad = w.saturating_sub(title.width()) / 2;
            out.push(Line::styled(format!("{}{}", " ".repeat(pad), title), "bold"));
        }

        let row_line = |cells: &[Line], header: bool| {
            let mut line = Line::plain(" ".repeat(self.indent));
            for (i, col) in self.columns.iter().enumerate() {
                let mut cell = cells.get(i).cloned().unwrap_or_default().truncate(widths[i]);
                let style = if header { self.header_style } else { col.style };
                for span in cell.0.iter_mut() {
                    span.1 = style.merge(span.1);
                }
                let pad = " ".repeat(widths[i].saturating_sub(cell.width()));
                if col.justify == Justify::Right {
                    line.0.push((pad, Style::PLAIN));
                    line.0.extend(cell.0);
                } else {
                    line.0.extend(cell.0);
                    if i + 1 < n {
                        line.0.push((pad, Style::PLAIN));
                    }
                }
                if i + 1 < n {
                    line.0.push((" ".repeat(self.gap), Style::PLAIN));
                }
            }
            line
        };

        if self.show_header {
            let headers: Vec<Line> = self.columns.iter().map(|c| Line::plain(c.header.clone())).collect();
            out.push(row_line(&headers, true));
        }

        for row in &self.rows {
            out.push(row_line(row, false));
        }

        out
    }

    pub fn print(&self) {
        print_lines(&self.render(term_size().0));
    }
}

static EDITOR: LazyLock<Mutex<Option<DefaultEditor>>> = LazyLock::new(|| Mutex::new(DefaultEditor::new().ok()));

/// Read a line. Ctrl+C / Ctrl+D / EOF read as "0" (back), like the python version.
pub fn prompt(text: &str) -> String {
    let mut editor = EDITOR.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(ed) = editor.as_mut() {
        return match ed.readline(text) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = ed.add_history_entry(line.as_str());
                }
                line
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => "0".into(),
            Err(_) => "0".into(),
        };
    }

    print!("{text}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => "0".into(),
        Ok(_) => line.trim_end_matches(['\r', '\n']).to_string(),
    }
}

/// Ask for a number. Empty input gives `default` (or 0).
pub fn ask(text: &str, default: Option<i64>) -> i64 {
    loop {
        let value = prompt(text);
        let value = value.trim();

        if value.is_empty() {
            return default.unwrap_or(0);
        }

        match value.parse() {
            Ok(n) => return n,
            Err(_) => print("[red]that aint a number[/red]\n"),
        }
    }
}

pub fn pause() {
    prompt("[enter]");
}

pub fn fmt_size(size: Option<i64>) -> String {
    let Some(size) = size else { return "?".into() };

    if size <= 0 {
        return "0 B".into();
    }

    let mut size = size as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if size < 1024.0 {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }

    format!("{size:.1} PB")
}

pub fn fmt_date(value: Option<&str>) -> String {
    value.map(|v| v.chars().take(10).collect()).unwrap_or_default()
}

/// cut a plain string to `max` display columns with a trailing "..."
pub fn ellipsize(s: &str, max: usize) -> String {
    Line::plain(s).truncate(max).text()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_parses_known_tags_only() {
        let lines = markup("[red]bad[/red] [enter] [bold cyan]x[/bold cyan]");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "bad [enter] x");
        assert_eq!(lines[0].0[0].1.fg, Some(31));
        assert!(lines[0].0[2].1.bold);
    }

    #[test]
    fn markup_multiline_and_nesting() {
        let lines = markup("[green]a\n[dim]b[/dim][/green]\nc");
        assert_eq!(lines.iter().map(Line::text).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert!(lines[1].0[0].1.dim && lines[1].0[0].1.fg == Some(32));
    }

    #[test]
    fn truncation() {
        assert_eq!(ellipsize("hello world", 8), "hello...");
        assert_eq!(ellipsize("hi", 8), "hi");
    }

    #[test]
    fn sizes() {
        assert_eq!(fmt_size(None), "?");
        assert_eq!(fmt_size(Some(0)), "0 B");
        assert_eq!(fmt_size(Some(1536)), "1.5 KB");
        assert_eq!(fmt_size(Some(5 * 1024 * 1024 * 1024)), "5.0 GB");
    }

    #[test]
    fn table_fits_width() {
        let mut t = Table::new(vec![Column::new("#").width(4).right(), Column::new("Name").flex()]);
        t.add_text_row(&["1", &"x".repeat(200)]);
        for line in t.render(60) {
            assert!(line.width() <= 60);
        }
    }
}
