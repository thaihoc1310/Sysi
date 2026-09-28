//! Chatbot answers are copied as Markdown: pipe tables, `**bold**`, `<br>`,
//! `$\rightarrow$`. A note is plain text, so all of that would sit on the desk
//! as raw punctuation. This turns a pasted answer into the text it was meant to
//! read as, and a pipe table into a box-drawn one that lines up in the note's
//! fixed-width face.
//!
//! Anything that does not carry an unmistakable Markdown mark is left alone:
//! a pasted `# comment` or `**kwargs` is code, not a document.

use regex::{Captures, Regex};
use std::sync::OnceLock;

/// Where a protected run (code, maths, a URL) waits out the inline rewrites.
const HOLD_OPEN: char = '\u{E000}';
const HOLD_CLOSE: char = '\u{E001}';
/// Marks a line of fenced code, which keeps its blank lines and trailing
/// spaces through the tidy-up that running text gets.
const VERBATIM: char = '\u{E002}';

/// Everything a paste into a note is tidied with: Markdown rendered as text,
/// and a drawing's first line given back the indent the copy left behind.
/// `None` means "paste the text as it is".
pub fn clean_paste(text: &str, width: usize) -> Option<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let rendered = clean_pasted_markdown(&text, width).unwrap_or_else(|| text.clone());
    let restored = restore_drawing_indent(&rendered).unwrap_or(rendered);
    (restored != text).then_some(restored)
}

/// `width` is the note's line length in fixed-width cells; 0 means unknown,
/// and a table is then drawn at its natural width. `None` means "paste the
/// text as it is".
pub fn clean_pasted_markdown(text: &str, width: usize) -> Option<String> {
    if text.contains([HOLD_OPEN, HOLD_CLOSE, VERBATIM]) {
        return None;
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if !looks_like_markdown(&text) {
        return None;
    }
    let cleaned = render_blocks(&text, width);
    (cleaned != text).then_some(cleaned)
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("markdown regex"))
}

macro_rules! regex {
    ($pattern:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        re(&CELL, $pattern)
    }};
}

fn fence_re() -> &'static Regex {
    regex!(r"^( {0,3})(`{3,}|~{3,})")
}

fn separator_re() -> &'static Regex {
    regex!(r"^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$")
}

/// Bold as a chatbot writes it: `**` straight onto a letter, a digit or an
/// opening quote. A glob such as `**/node_modules/**` or Python's
/// `f(**kwargs)` never matches.
const BOLD: &str = r#"(?:^|[^\w*])\*\*[\p{L}\p{N}"'(“\[](?:[^*\n]*[^\s*/])?\*\*"#;
/// `__bold__` only around a phrase: without the space it is `__init__`.
const BOLD_UNDERSCORE: &str = r"(^|[^\w])__([\p{L}\p{N}][^_\n]*\s[^_\n]*[^\s_])__($|[^\w])";

/// Whether a `\(...\)` or `$$...$$` body is TeX rather than a regex group or
/// a shell's PID: a command, a script or a brace.
fn is_tex(body: &str) -> bool {
    regex!(r"\\[A-Za-z]|[\^_{]").is_match(body)
}

fn looks_like_markdown(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    let bold = regex!(BOLD);
    let bold_underscore = regex!(BOLD_UNDERSCORE);
    let delimited = regex!(r"\$\$([^$\n]+)\$\$|\\\(([^\n]*?)\\\)|\\\[([^\n]*?)\\\]");
    let dollar = regex!(r"\$[^$\s][^$\n]*\\[A-Za-z]+[^$\n]*\$");
    let link = regex!(r"\[[^\]\n]+\]\((?:https?://|mailto:|#|/)[^)\s]*\)");
    let tex = |line: &str| {
        dollar.is_match(line)
            || delimited.captures_iter(line).any(|caps| {
                caps.get(1).or(caps.get(2)).or(caps.get(3)).is_some_and(|m| is_tex(m.as_str()))
            })
    };
    // `<br>` is not on the list: on its own it is as likely an HTML file.
    for (index, line) in lines.iter().enumerate() {
        if fence_re().is_match(line) {
            // A fence alone is enough: the code inside comes out verbatim.
            return true;
        }
        if index + 1 < lines.len()
            && has_unescaped_pipe(line)
            && lines[index + 1].contains('|')
            && separator_re().is_match(lines[index + 1])
        {
            return true;
        }
        if bold.is_match(line) || bold_underscore.is_match(line) || tex(line) || link.is_match(line) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- blocks

fn render_blocks(text: &str, width: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let drawing = diagram_lines(text);
    let mut out: Vec<String> = Vec::new();
    // Whether the last source line was running paragraph text, which makes a
    // following `---` a setext underline rather than a rule.
    let mut after_paragraph = false;
    let heading = regex!(r"^ {0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$");
    let rule = regex!(r"^ {0,3}([-*_])(?:\s*[-*_]){2,}\s*$");
    let setext = regex!(r"^ {0,3}(=+|-+)\s*$");
    let quote = regex!(r"^ {0,3}((?:>\s?)+)(.*)$");
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(fence) = fence_re().captures(line) {
            let indent = fence[1].len();
            let marker = &fence[2];
            let close = marker.chars().next().unwrap();
            i += 1;
            while i < lines.len() {
                let body = lines[i];
                let trimmed = body.trim();
                if trimmed.len() >= marker.len() && trimmed.chars().all(|c| c == close) {
                    i += 1;
                    break;
                }
                out.push(format!("{VERBATIM}{}", strip_indent(body, indent)));
                i += 1;
            }
            after_paragraph = false;
            continue;
        }
        let trimmed = line.trim();
        if trimmed == "$$" || trimmed == "\\[" {
            let end = if trimmed == "$$" { "$$" } else { "\\]" };
            if let Some(offset) = lines[i + 1..].iter().position(|l| l.trim() == end) {
                let body = lines[i + 1..i + 1 + offset].join(" ");
                out.push(latex_to_text(&body));
                i += offset + 2;
                after_paragraph = false;
                continue;
            }
        }
        if is_table_start(&lines, i) {
            let (block, next) = render_table(&lines, i, width);
            out.extend(block);
            i = next;
            after_paragraph = false;
            continue;
        }
        // A drawing pasted without its fence carries no Markdown, and its
        // spacing is the picture: `*` or `_` inside a box is not emphasis.
        if drawing.binary_search(&i).is_ok() {
            out.push(format!("{VERBATIM}{line}"));
            after_paragraph = false;
            i += 1;
            continue;
        }
        if trimmed.is_empty() {
            out.push(String::new());
            after_paragraph = false;
            i += 1;
            continue;
        }
        if after_paragraph && setext.is_match(line) {
            // The line above was a heading; the underline carries nothing.
            after_paragraph = false;
            i += 1;
            continue;
        }
        if rule.is_match(line) {
            out.push("─".repeat(if width > 0 { width.min(40) } else { 24 }));
            after_paragraph = false;
            i += 1;
            continue;
        }
        if let Some(caps) = heading.captures(line) {
            out.push(clean_inline(&caps[2]));
            after_paragraph = false;
            i += 1;
            continue;
        }
        if let Some(caps) = quote.captures(line) {
            let depth = caps[1].matches('>').count();
            let body = clean_line(&caps[2]);
            out.push(format!("{}{}", "▏ ".repeat(depth), body));
            after_paragraph = false;
            i += 1;
            continue;
        }
        let (text, list) = clean_line_kind(line);
        out.push(text);
        after_paragraph = !list;
        i += 1;
    }
    finish(out, text.ends_with('\n'))
}

fn strip_indent(line: &str, indent: usize) -> &str {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    &line[spaces.min(indent)..]
}

/// Squeeze runs of blank lines to one and trim them off both ends, then give
/// back the trailing newline the paste had.
fn finish(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in lines.into_iter().flat_map(|line| {
        line.split('\n').map(str::to_owned).collect::<Vec<_>>()
    }) {
        if let Some(code) = line.strip_prefix(VERBATIM) {
            out.push(code.to_owned());
            continue;
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() && out.last().is_none_or(|last: &String| last.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|last| last.is_empty()) {
        out.pop();
    }
    let mut text = out.join("\n");
    if trailing_newline {
        text.push('\n');
    }
    text
}

fn clean_line(line: &str) -> String {
    clean_line_kind(line).0
}

/// One line of running text or a list item, and whether it was a list item.
fn clean_line_kind(line: &str) -> (String, bool) {
    let item = regex!(r"^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$");
    let task = regex!(r"^\[([ xX])\]\s+(.*)$");
    let Some(caps) = item.captures(line) else {
        let indent = &line[..line.len() - line.trim_start().len()];
        return (format!("{}{}", indent.replace('\t', "    "), clean_inline(line.trim())), false);
    };
    let indent = caps[1].replace('\t', "    ");
    let marker = &caps[2];
    let mut body = caps[3].to_owned();
    let mut bullet = if marker.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        marker.to_owned()
    } else if indent.len() < 2 {
        "•".to_owned()
    } else {
        "◦".to_owned()
    };
    if let Some(task) = task.captures(&body.clone()) {
        let done = &task[1] != " ";
        bullet = format!("{bullet} {}", if done { "☑" } else { "☐" });
        body = task[2].to_owned();
    }
    (format!("{indent}{bullet} {}", clean_inline(&body)), true)
}

// ---------------------------------------------------------------- drawings

fn is_box_stroke(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// Arrows, and the triangles chatbots draw arrowheads with (`▼`, `►`).
fn is_arrow(c: char) -> bool {
    matches!(c, '\u{2190}'..='\u{21FF}' | '\u{25A0}'..='\u{25FF}' | '\u{27F0}'..='\u{27FF}')
}

/// A line with strokes in it: box-drawing characters, or an ASCII box corner
/// (`+--`, `--+`).
fn is_drawn(line: &str) -> bool {
    line.chars().any(is_box_stroke) || regex!(r"\+[-=]{2,}|[-=]{2,}\+").is_match(line)
}

/// A line that may sit inside a drawing without strokes of its own: a row
/// of arrows, or labels laid out with runs of spaces. Four spaces in a row
/// is the usual tell of text art; running prose never has them.
fn fits_drawing(line: &str) -> bool {
    !line.trim().is_empty()
        && (is_drawn(line) || line.chars().any(is_arrow) || line.contains("    "))
}

/// The lines of `text` that belong to a drawing and only line up in a
/// fixed-width face: every line with strokes, and the arrow rows and spaced
/// labels next to them, up to the first blank or ordinary line.
pub fn diagram_lines(text: &str) -> Vec<usize> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_drawn(lines[i]) {
            i += 1;
            continue;
        }
        let first = found.last().map_or(0, |last: &usize| last + 1);
        let mut start = i;
        while start > first && fits_drawing(lines[start - 1]) {
            start -= 1;
        }
        let mut end = i;
        while end + 1 < lines.len() && fits_drawing(lines[end + 1]) {
            end += 1;
        }
        found.extend(start..=end);
        i = end + 1;
    }
    found
}

/// Strokes that go on into the line below, and ones that come from above.
fn reaches_down(c: char) -> bool {
    matches!(c, '┌' | '┐' | '┬' | '├' | '┤' | '┼' | '│' | '╭' | '╮' | '╔' | '╗' | '╦' | '╠'
        | '╣' | '╬' | '║' | '┏' | '┓' | '┳' | '┣' | '┫' | '╋' | '┃' | '+' | '|')
}

fn reaches_up(c: char) -> bool {
    matches!(c, '│' | '├' | '┤' | '┼' | '┴' | '└' | '┘' | '╰' | '╯' | '║' | '╚' | '╝' | '╩'
        | '╠' | '╣' | '╬' | '┃' | '┗' | '┛' | '┻' | '┣' | '┫' | '╋' | '▼' | '▲' | '↓' | '+' | '|')
}

/// The character in each fixed-width cell of a line (`None` for the second
/// half of a wide character).
fn cells(line: &str) -> Vec<Option<char>> {
    let mut out = Vec::new();
    for c in line.chars() {
        match char_width(c) {
            0 => {}
            1 => out.push(Some(c)),
            _ => out.extend([Some(c), None]),
        }
    }
    out
}

/// A copy that starts at the first thing drawn leaves the first line's indent
/// behind: the selection begins at its `┌`, not at the spaces before it, so
/// the top of a box lands at the margin while its sides stay where they were.
/// Put the indent back where the strokes of the line below say it was.
pub fn restore_drawing_indent(text: &str) -> Option<String> {
    let mut lines = text.split('\n');
    let first = lines.next()?;
    let below = cells(lines.next()?);
    if first.starts_with([' ', '\t']) || !first.chars().any(is_box_stroke) {
        return None;
    }
    let strokes: Vec<usize> = cells(first)
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_some_and(reaches_down))
        .map(|(x, _)| x)
        .collect();
    // One stroke lines up with something almost anywhere.
    if strokes.len() < 2 {
        return None;
    }
    let meets = |shift: usize| {
        strokes
            .iter()
            .all(|x| below.get(x + shift).copied().flatten().is_some_and(reaches_up))
    };
    if meets(0) {
        return None;
    }
    let shift = (1..below.len()).find(|&shift| meets(shift))?;
    Some(format!("{}{text}", " ".repeat(shift)))
}

// ---------------------------------------------------------------- tables

#[derive(Clone, Copy, PartialEq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

fn has_unescaped_pipe(line: &str) -> bool {
    split_unescaped_pipes(line).len() > 1
}

fn split_unescaped_pipes(line: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cells.last_mut().unwrap().push_str("\\|");
                chars.next();
            }
            '|' => cells.push(String::new()),
            _ => cells.last_mut().unwrap().push(c),
        }
    }
    cells
}

fn split_row(row: &str) -> Vec<String> {
    let mut cells = split_unescaped_pipes(row.trim());
    if cells.len() > 1 && cells.first().is_some_and(|c| c.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|c| c.trim().is_empty()) {
        cells.pop();
    }
    cells.into_iter().map(|c| c.trim().to_owned()).collect()
}

fn ends_with_unescaped_pipe(text: &str) -> bool {
    let text = text.trim_end();
    text.ends_with('|') && !text.ends_with("\\|")
}

fn is_table_start(lines: &[&str], i: usize) -> bool {
    i + 1 < lines.len()
        && has_unescaped_pipe(lines[i])
        && lines[i + 1].contains('|')
        && separator_re().is_match(lines[i + 1])
}

/// Draws the table that starts at `start` and returns it with the index of
/// the first line after it.
fn render_table(lines: &[&str], start: usize, width: usize) -> (Vec<String>, usize) {
    let header = split_row(lines[start]);
    let aligns: Vec<Align> = split_row(lines[start + 1])
        .iter()
        .map(|cell| match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .collect();
    let mut rows = Vec::new();
    let mut j = start + 2;
    while j < lines.len() {
        let line = lines[j];
        if line.trim().is_empty() || !has_unescaped_pipe(line) {
            break;
        }
        let mut row = line.to_owned();
        j += 1;
        // A chatbot's `<br>` often lands the rest of a row on a later line,
        // with blank lines between. The row is still open while it lacks its
        // closing pipe; take lines until one supplies it, but never swallow
        // the next row.
        if row.trim_start().starts_with('|') && !ends_with_unescaped_pipe(&row) {
            let mut k = j;
            while k < lines.len() && k < j + 40 {
                let next = lines[k];
                if next.trim_start().starts_with('|') || separator_re().is_match(next) && next.contains('|') {
                    break;
                }
                if ends_with_unescaped_pipe(next) {
                    row = std::iter::once(row.as_str())
                        .chain(lines[j..=k].iter().copied())
                        .collect::<Vec<_>>()
                        .join("\n");
                    j = k + 1;
                    break;
                }
                k += 1;
            }
        }
        rows.push(split_row(&row));
    }
    let columns = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(1);
    let cell_lines = |cells: &[String]| -> Vec<Vec<String>> {
        (0..columns)
            .map(|c| {
                cells
                    .get(c)
                    .map(|cell| {
                        clean_inline(cell)
                            .split('\n')
                            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
                            .filter(|l| !l.is_empty())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect()
    };
    let header = cell_lines(&header);
    let body: Vec<Vec<Vec<String>>> = rows.iter().map(|row| cell_lines(row)).collect();
    let aligns: Vec<Align> = (0..columns).map(|c| aligns.get(c).copied().unwrap_or(Align::Left)).collect();
    let drawn = draw_box_table(&header, &body, &aligns, width)
        .unwrap_or_else(|| draw_records(&header, &body));
    (drawn, j)
}

fn draw_box_table(
    header: &[Vec<String>],
    body: &[Vec<Vec<String>>],
    aligns: &[Align],
    width: usize,
) -> Option<Vec<String>> {
    let columns = header.len();
    let natural: Vec<usize> = (0..columns)
        .map(|c| {
            std::iter::once(&header[c])
                .chain(body.iter().map(|row| &row[c]))
                .flatten()
                .map(|line| display_width(line))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    // "│ a │ b │": two cells of padding and one border per column, plus one.
    let chrome = 3 * columns + 1;
    let mut widths = natural.clone();
    if width > 0 && chrome + widths.iter().sum::<usize>() > width {
        let room = width.checked_sub(chrome)?;
        // Narrowest a column is worth drawing; below this the records read better.
        const FLOOR: usize = 6;
        while widths.iter().sum::<usize>() > room {
            let (widest, &w) = widths.iter().enumerate().max_by_key(|(_, w)| **w)?;
            if w <= FLOOR {
                return None;
            }
            widths[widest] = w - 1;
        }
    }
    let wrap_row = |row: &[Vec<String>]| -> Vec<Vec<String>> {
        row.iter()
            .zip(&widths)
            .map(|(lines, &w)| lines.iter().flat_map(|line| wrap(line, w)).collect())
            .collect()
    };
    let border = |left: char, mid: char, right: char| {
        let segments: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("{left}{}{right}", segments.join(&mid.to_string()))
    };
    let draw_row = |cells: &[Vec<String>], aligns: &[Align], out: &mut Vec<String>| {
        let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
        for line in 0..height {
            let mut text = String::from("│");
            for (c, w) in widths.iter().enumerate() {
                let cell = cells[c].get(line).map(String::as_str).unwrap_or("");
                text.push(' ');
                text.push_str(&pad(cell, *w, aligns[c]));
                text.push_str(" │");
            }
            out.push(text);
        }
    };
    let header = wrap_row(header);
    let body: Vec<_> = body.iter().map(|row| wrap_row(row)).collect();
    // Rules between rows only once a row takes more than one line; a table of
    // one-liners reads fine without them and stays half as tall.
    let ruled = body.iter().any(|row| row.iter().any(|cell| cell.len() > 1));
    let mut out = vec![border('┌', '┬', '┐')];
    draw_row(&header, aligns, &mut out);
    out.push(border('├', '┼', '┤'));
    for (index, row) in body.iter().enumerate() {
        if ruled && index > 0 {
            out.push(border('├', '┼', '┤'));
        }
        draw_row(row, aligns, &mut out);
    }
    out.push(border('└', '┴', '┘'));
    Some(out)
}

/// A table too wide to draw in the note becomes one short block per row, its
/// first cell as the title and every other cell under its column's name.
fn draw_records(header: &[Vec<String>], body: &[Vec<Vec<String>>]) -> Vec<String> {
    let mut out = Vec::new();
    for row in body {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("• {}", row[0].join(" ")));
        for (c, cell) in row.iter().enumerate().skip(1) {
            if cell.is_empty() {
                continue;
            }
            let name = header[c].join(" ");
            let value = join_sentences(cell);
            out.push(if name.is_empty() {
                format!("  {value}")
            } else {
                format!("  {name}: {value}")
            });
        }
    }
    out
}

/// A cell's lines on one line: "; " between fragments, a plain space after
/// a line that already ends a sentence.
fn join_sentences(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        if !out.is_empty() {
            out.push_str(if out.ends_with(['.', '!', '?', ';', ':']) { " " } else { "; " });
        }
        out.push_str(line);
    }
    out
}

fn pad(text: &str, width: usize, align: Align) -> String {
    let gap = width.saturating_sub(display_width(text));
    let (left, right) = match align {
        Align::Left => (0, gap),
        Align::Right => (gap, 0),
        Align::Center => (gap / 2, gap - gap / 2),
    };
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
}

/// Greedy word wrap to `width` cells; a word longer than a whole line is cut.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let mut word = word.to_owned();
        loop {
            let used = display_width(&current);
            let need = display_width(&word) + usize::from(!current.is_empty());
            if used + need <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(&word);
                break;
            }
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                continue;
            }
            // Alone on the line and still too long: cut it.
            let mut head = String::new();
            let mut rest = word.chars().peekable();
            while let Some(&c) = rest.peek() {
                if display_width(&head) + char_width(c) > width && !head.is_empty() {
                    break;
                }
                head.push(c);
                rest.next();
            }
            out.push(head);
            word = rest.collect();
            if word.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Cells a character takes in a fixed-width face: none for a combining mark
/// (a decomposed Vietnamese tone rides on its letter), two for East Asian wide
/// characters, one for everything else.
fn char_width(c: char) -> usize {
    let code = c as u32;
    let zero = matches!(code,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200B..=0x200F | 0x2060..=0x2064
        | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0xE0100..=0xE01EF);
    if zero {
        return 0;
    }
    let wide = matches!(code,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFD
        // The emoji a chatbot puts in a table cell, which terminals draw
        // two cells wide: ✅ ❌ ⭐ ⚠ and friends.
        | 0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE
        | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1
        | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE | 0x26D4
        | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705
        | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55);
    if wide {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------- inline

struct Held(Vec<String>);

impl Held {
    fn hold(&mut self, text: String) -> String {
        self.0.push(text);
        format!("{HOLD_OPEN}{}{HOLD_CLOSE}", self.0.len() - 1)
    }

    fn release(&self, text: &str) -> String {
        let token = regex!("\u{E000}(\\d+)\u{E001}");
        // Held text can hold tokens of its own (an escape inside a URL), so
        // keep going until none is left. Each pass only ever reaches for an
        // earlier hold, so this ends; the bound is there regardless.
        let mut text = text.to_owned();
        for _ in 0..8 {
            if !text.contains(HOLD_OPEN) {
                break;
            }
            text = token
                .replace_all(&text, |caps: &Captures| {
                    caps[1].parse::<usize>().ok().and_then(|i| self.0.get(i)).cloned().unwrap_or_default()
                })
                .into_owned();
        }
        text
    }
}

/// One stretch of Markdown text to what it reads as. A `<br>` becomes a
/// newline; the caller decides what a newline means there.
fn clean_inline(text: &str) -> String {
    let mut held = Held(Vec::new());
    let text = hold_code_spans(text, &mut held);
    let text = hold_maths(&text, &mut held);
    let escape = regex!(r"\\([!-/:-@\[-`{-~])");
    let text = escape.replace_all(&text, |caps: &Captures| held.hold(caps[1].to_owned())).into_owned();

    // One level of brackets inside a URL, for Wikipedia's Foo_(bar).
    let image = regex!(r#"!\[([^\]]*)\]\(\s*((?:[^()\s]|\([^()\s]*\))+)(?:\s+"[^"]*")?\s*\)"#);
    let text = image
        .replace_all(&text, |caps: &Captures| {
            if caps[1].trim().is_empty() {
                held.hold(caps[2].to_owned())
            } else {
                caps[1].to_owned()
            }
        })
        .into_owned();
    let link = regex!(r#"\[([^\]]+)\]\(\s*((?:[^()\s]|\([^()\s]*\))+)(?:\s+"[^"]*")?\s*\)"#);
    let text = link
        .replace_all(&text, |caps: &Captures| {
            let (label, url) = (&caps[1], &caps[2]);
            if url.starts_with('#') || label.trim() == url {
                label.to_owned()
            } else {
                format!("{label} ({})", held.hold(url.to_owned()))
            }
        })
        .into_owned();
    let autolink = regex!(r"<((?:https?|mailto):[^>\s]+)>");
    let text = autolink.replace_all(&text, |caps: &Captures| held.hold(caps[1].to_owned())).into_owned();

    let br = regex!(r"(?i)<br\s*/?>");
    let text = br.replace_all(&text, "\n").into_owned();
    let sup = regex!(r"(?i)<sup>([^<]*)</sup>");
    let text = sup.replace_all(&text, |caps: &Captures| scripted(&caps[1], true)).into_owned();
    let sub = regex!(r"(?i)<sub>([^<]*)</sub>");
    let text = sub.replace_all(&text, |caps: &Captures| scripted(&caps[1], false)).into_owned();
    let tag = regex!(
        r"(?i)</?(?:b|strong|i|em|u|s|del|ins|strike|mark|span|small|big|font|code|kbd|samp|var|abbr|cite|q|p|div|center|details|summary|tt)(?:\s[^<>]*)?/?>"
    );
    let text = tag.replace_all(&text, "").into_owned();
    let text = decode_entities(&text);

    let mut text = text;
    for _ in 0..2 {
        // Twice: a match eats the boundary character the next one needs.
        for (pattern, replacement) in [
            (
                regex!(r#"(^|[^\w*])\*\*\*([\p{L}\p{N}"'(“\[](?:[^*\n]*?[^\s*/])?)\*\*\*"#),
                "$1$2",
            ),
            (
                regex!(r#"(^|[^\w*])\*\*([\p{L}\p{N}"'(“\[](?:[^*\n]*?[^\s*/])?)\*\*"#),
                "$1$2",
            ),
            (regex!(BOLD_UNDERSCORE), "$1$2$3"),
            (regex!(r"(^|[^\w*])\*([^\s*](?:[^*\n]*[^\s*])?)\*($|[^\w*])"), "$1$2$3"),
            (regex!(r"(^|[^\w])_([^\s_](?:[^_\n]*[^\s_])?)_($|[^\w])"), "$1$2$3"),
            (regex!(r"~~([^\n~]+?)~~"), "$1"),
        ] {
            text = pattern.replace_all(&text, replacement).into_owned();
        }
    }
    held.release(&text)
}

/// Backtick spans keep their text exactly, Markdown and all.
fn hold_code_spans(text: &str, held: &mut Held) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let run = chars[i..].iter().take_while(|&&c| c == '`').count();
        let mut j = i + run;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let other = chars[j..].iter().take_while(|&&c| c == '`').count();
                if other == run {
                    close = Some(j);
                    break;
                }
                j += other;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                let inner: String = chars[i + run..end].iter().collect();
                let inner = if inner.len() > 2 && inner.starts_with(' ') && inner.ends_with(' ') {
                    inner[1..inner.len() - 1].to_owned()
                } else {
                    inner
                };
                out.push_str(&held.hold(inner));
                i = end + run;
            }
            None => {
                out.extend(&chars[i..i + run]);
                i += run;
            }
        }
    }
    out
}

fn hold_maths(text: &str, held: &mut Held) -> String {
    let display = regex!(r"\$\$(.+?)\$\$|\\\((.+?)\\\)|\\\[(.+?)\\\]");
    let text = display
        .replace_all(text, |caps: &Captures| {
            let body = caps.get(1).or(caps.get(2)).or(caps.get(3)).map_or("", |m| m.as_str());
            // A regex group, \(foo\), is left for the escape pass, and a span
            // around held code is not maths at all: converting it would tear
            // the hold apart.
            if is_tex(body) && !body.contains(HOLD_OPEN) {
                held.hold(latex_to_text(body))
            } else {
                caps[0].to_owned()
            }
        })
        .into_owned();
    // Single dollars by pandoc's rule: no space inside either dollar, and no
    // digit right after the closing one, so "$5 and $10" stays money. On top
    // of that the span must look like maths, so "$HOME/$PATH" stays a path.
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let opens = chars[i] == '$'
            && (i == 0 || chars[i - 1] != '\\')
            && chars.get(i + 1).is_some_and(|c| !c.is_whitespace() && *c != '$');
        if opens {
            let close = (i + 2..chars.len())
                .take_while(|&j| chars[j] != '\n')
                .find(|&j| {
                    chars[j] == '$'
                        && chars[j - 1] != '\\'
                        && !chars[j - 1].is_whitespace()
                        && !chars.get(j + 1).is_some_and(char::is_ascii_digit)
                });
            if let Some(end) = close {
                let body: String = chars[i + 1..end].iter().collect();
                let variable = body.chars().count() == 1 && body.chars().all(char::is_alphabetic);
                if (variable || body.contains(['\\', '^', '_', '{'])) && !body.contains(HOLD_OPEN) {
                    out.push_str(&held.hold(latex_to_text(&body)));
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn decode_entities(text: &str) -> String {
    let entity = regex!(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z]{2,8});");
    entity
        .replace_all(text, |caps: &Captures| {
            let name = &caps[1];
            let decoded = if let Some(hex) = name.strip_prefix("#x").or(name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).map(String::from)
            } else if let Some(dec) = name.strip_prefix('#') {
                dec.parse().ok().and_then(char::from_u32).map(String::from)
            } else {
                match name {
                    "nbsp" | "ensp" | "emsp" | "thinsp" => Some(" "),
                    "amp" => Some("&"),
                    "lt" => Some("<"),
                    "gt" => Some(">"),
                    "quot" => Some("\""),
                    "apos" => Some("'"),
                    "ndash" => Some("–"),
                    "mdash" => Some("—"),
                    "hellip" => Some("…"),
                    "rarr" => Some("→"),
                    "larr" => Some("←"),
                    "harr" => Some("↔"),
                    "rArr" => Some("⇒"),
                    "times" => Some("×"),
                    "divide" => Some("÷"),
                    "plusmn" => Some("±"),
                    "deg" => Some("°"),
                    "copy" => Some("©"),
                    "reg" => Some("®"),
                    "trade" => Some("™"),
                    "laquo" => Some("«"),
                    "raquo" => Some("»"),
                    "middot" => Some("·"),
                    "bull" => Some("•"),
                    _ => None,
                }
                .map(String::from)
            };
            decoded.unwrap_or_else(|| caps[0].to_owned())
        })
        .into_owned()
}

// ---------------------------------------------------------------- maths

/// Inline TeX to the Unicode it draws: `\rightarrow` to →, `x^2` to x²,
/// `\frac{a}{b}` to a/b. A command it does not know is kept as written, so
/// nothing silently disappears.
fn latex_to_text(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut at = 0;
    let text = latex_group(&chars, &mut at, false, 0);
    let spaces = regex!(r"[ \t\n]+");
    spaces.replace_all(text.trim(), " ").into_owned()
}

/// Deeper than any formula anyone writes; past it the rest is kept as typed
/// rather than recursing until the stack runs out.
const LATEX_DEPTH: usize = 64;

fn latex_rest(chars: &[char], at: &mut usize) -> String {
    let rest: String = chars[(*at).min(chars.len())..].iter().collect();
    *at = chars.len();
    rest
}

fn latex_group(chars: &[char], at: &mut usize, until_brace: bool, depth: usize) -> String {
    if depth > LATEX_DEPTH {
        return latex_rest(chars, at);
    }
    let mut out = String::new();
    while *at < chars.len() {
        let c = chars[*at];
        match c {
            '}' if until_brace => {
                *at += 1;
                return out;
            }
            '{' => {
                *at += 1;
                out.push_str(&latex_group(chars, at, true, depth + 1));
            }
            '\\' => {
                *at += 1;
                out.push_str(&latex_command(chars, at, depth + 1));
            }
            '^' | '_' => {
                *at += 1;
                let arg = latex_argument(chars, at, depth + 1);
                let superscript = c == '^';
                if superscript && (arg == "∘" || arg == "°") {
                    out.push('°');
                } else if superscript && arg == "′" {
                    out.push('′');
                } else {
                    out.push_str(&scripted(&arg, superscript));
                }
            }
            '~' => {
                *at += 1;
                out.push(' ');
            }
            '&' => {
                *at += 1;
                out.push(' ');
            }
            _ => {
                *at += 1;
                out.push(c);
            }
        }
    }
    out
}

/// The next argument: a braced group, a command, or one character.
fn latex_argument(chars: &[char], at: &mut usize, depth: usize) -> String {
    if depth > LATEX_DEPTH {
        return latex_rest(chars, at);
    }
    while *at < chars.len() && chars[*at] == ' ' {
        *at += 1;
    }
    match chars.get(*at) {
        Some('{') => {
            *at += 1;
            latex_group(chars, at, true, depth + 1)
        }
        Some('\\') => {
            *at += 1;
            latex_command(chars, at, depth + 1)
        }
        Some(&c) => {
            *at += 1;
            c.to_string()
        }
        None => String::new(),
    }
}

fn latex_command(chars: &[char], at: &mut usize, depth: usize) -> String {
    let start = *at;
    while *at < chars.len() && chars[*at].is_ascii_alphabetic() {
        *at += 1;
    }
    if *at == start {
        // A control symbol: \, \; \{ \% and friends.
        let Some(&c) = chars.get(*at) else {
            return String::new();
        };
        *at += 1;
        return match c {
            ',' | ';' | ':' | ' ' => " ".to_owned(),
            '!' => String::new(),
            '\\' => " ".to_owned(),
            other => other.to_string(),
        };
    }
    let name: String = chars[start..*at].iter().collect();
    match name.as_str() {
        "text" | "textbf" | "textit" | "textrm" | "textsf" | "texttt" | "mathrm" | "mathbf"
        | "mathit" | "mathsf" | "mathtt" | "mathcal" | "operatorname" | "mbox" | "boldsymbol"
        | "emph" | "underline" | "overline" | "hat" | "bar" | "vec" | "tilde" | "dot" => {
            latex_argument(chars, at, depth + 1)
        }
        "mathbb" => {
            let arg = latex_argument(chars, at, depth + 1);
            match arg.as_str() {
                "R" => "ℝ",
                "N" => "ℕ",
                "Z" => "ℤ",
                "Q" => "ℚ",
                "C" => "ℂ",
                _ => return arg,
            }
            .to_owned()
        }
        "frac" | "dfrac" | "tfrac" => {
            let top = latex_argument(chars, at, depth + 1);
            let bottom = latex_argument(chars, at, depth + 1);
            format!("{}/{}", grouped(&top), grouped(&bottom))
        }
        "sqrt" => {
            // An index, \sqrt[3]{x}, is dropped: √ has no room for it.
            if chars.get(*at) == Some(&'[') {
                while *at < chars.len() && chars[*at] != ']' {
                    *at += 1;
                }
                *at += 1;
            }
            let arg = latex_argument(chars, at, depth + 1);
            format!("√{}", grouped(&arg))
        }
        "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl" | "Bigr"
        | "displaystyle" | "textstyle" | "limits" | "nolimits" => {
            if chars.get(*at) == Some(&'.') {
                *at += 1;
            }
            String::new()
        }
        _ => latex_symbol(&name).map_or_else(|| format!("\\{name}"), str::to_owned),
    }
}

/// A fraction's side or a root's body, parenthesised once it is more than one
/// term, so a+b over c reads (a+b)/c and not a+b/c.
fn grouped(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() > 1 && text.contains([' ', '+', '-', '/', '−', '·', '×']) {
        format!("({text})")
    } else {
        text.to_owned()
    }
}

fn scripted(text: &str, superscript: bool) -> String {
    let map = |c: char| -> Option<char> {
        if superscript {
            Some(match c {
                '0' => '⁰',
                '1' => '¹',
                '2' => '²',
                '3' => '³',
                '4' => '⁴',
                '5' => '⁵',
                '6' => '⁶',
                '7' => '⁷',
                '8' => '⁸',
                '9' => '⁹',
                '+' => '⁺',
                '-' | '−' => '⁻',
                '=' => '⁼',
                '(' => '⁽',
                ')' => '⁾',
                'n' => 'ⁿ',
                'i' => 'ⁱ',
                _ => return None,
            })
        } else {
            Some(match c {
                '0' => '₀',
                '1' => '₁',
                '2' => '₂',
                '3' => '₃',
                '4' => '₄',
                '5' => '₅',
                '6' => '₆',
                '7' => '₇',
                '8' => '₈',
                '9' => '₉',
                '+' => '₊',
                '-' | '−' => '₋',
                '=' => '₌',
                '(' => '₍',
                ')' => '₎',
                'a' => 'ₐ',
                'e' => 'ₑ',
                'o' => 'ₒ',
                'x' => 'ₓ',
                'h' => 'ₕ',
                'k' => 'ₖ',
                'l' => 'ₗ',
                'm' => 'ₘ',
                'n' => 'ₙ',
                'p' => 'ₚ',
                's' => 'ₛ',
                't' => 'ₜ',
                'i' => 'ᵢ',
                'j' => 'ⱼ',
                _ => return None,
            })
        }
    };
    let text = text.trim();
    if let Some(mapped) = text.chars().map(map).collect::<Option<String>>() {
        return mapped;
    }
    let mark = if superscript { '^' } else { '_' };
    if text.chars().count() == 1 {
        format!("{mark}{text}")
    } else {
        format!("{mark}({text})")
    }
}

fn latex_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Arrows.
        "rightarrow" | "to" => "→",
        "leftarrow" | "gets" => "←",
        "Rightarrow" | "implies" => "⇒",
        "Leftarrow" | "impliedby" => "⇐",
        "leftrightarrow" => "↔",
        "Leftrightarrow" | "iff" => "⇔",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "Longrightarrow" => "⟹",
        "Longleftarrow" => "⟸",
        "longleftrightarrow" => "⟷",
        "Longleftrightarrow" => "⟺",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "Uparrow" => "⇑",
        "Downarrow" => "⇓",
        "updownarrow" => "↕",
        "mapsto" => "↦",
        "nearrow" => "↗",
        "searrow" => "↘",
        "hookrightarrow" => "↪",
        "rightleftharpoons" => "⇌",
        // Relations.
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "lt" => "<",
        "gt" => ">",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "perp" => "⊥",
        "parallel" => "∥",
        "mid" => "∣",
        // Operators.
        "times" => "×",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "cdot" | "cdotp" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" => "∖",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "neg" | "lnot" => "¬",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "iint" => "∬",
        "oint" => "∮",
        "partial" => "∂",
        "nabla" => "∇",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "emptyset" | "varnothing" => "∅",
        "infty" => "∞",
        "angle" => "∠",
        "triangle" => "△",
        "degree" | "textdegree" => "°",
        "prime" => "′",
        "ldots" | "dots" | "dotsc" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "therefore" => "∴",
        "because" => "∵",
        "checkmark" => "✓",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "aleph" => "ℵ",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "vert" | "lvert" | "rvert" => "|",
        "Vert" | "lVert" | "rVert" => "‖",
        "colon" => ":",
        "quad" => "  ",
        "qquad" => "    ",
        // Function names set upright in TeX read the same as plain words.
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "cot" => "cot",
        "log" => "log",
        "ln" => "ln",
        "lg" => "lg",
        "exp" => "exp",
        "lim" => "lim",
        "max" => "max",
        "min" => "min",
        "sup" => "sup",
        "inf" => "inf",
        "det" => "det",
        "gcd" => "gcd",
        "mod" | "bmod" => "mod",
        // Greek.
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "sigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer that prompted this, exactly as it came off the clipboard:
    /// `<br>` rows split over blank lines, bold, italics and `$\rightarrow$`.
    const TRIP_ANSWER: &str = "Để phân biệt, nhìn vào **độ dài, mục đích**:

| Từ vựng | Bản chất | Ví dụ thực tế |
| --- | --- | --- |
| **Trip** | Chuyến đi **ngắn hạn**. | *A road trip to Ta Xua*<br>

<br>*A business trip* |
| **Tour** | Tham quan **nhiều địa điểm**. | *A guided tour of Hanoi*<br>

<br>*A walking tour* |

---

### Mẹo so sánh:

1. **Trip vs. Journey:**
* **Trip** nhấn mạnh *mục đích*: đi 2 ngày rồi về $\\rightarrow$ gọi là **a trip** *(We went on a weekend trip)*.
";

    #[test]
    fn the_trip_answer_reads_as_text_with_a_drawn_table() {
        let cleaned = clean_pasted_markdown(TRIP_ANSWER, 0).unwrap();
        let expected = "Để phân biệt, nhìn vào độ dài, mục đích:

┌─────────┬───────────────────────────┬────────────────────────┐
│ Từ vựng │ Bản chất                  │ Ví dụ thực tế          │
├─────────┼───────────────────────────┼────────────────────────┤
│ Trip    │ Chuyến đi ngắn hạn.       │ A road trip to Ta Xua  │
│         │                           │ A business trip        │
├─────────┼───────────────────────────┼────────────────────────┤
│ Tour    │ Tham quan nhiều địa điểm. │ A guided tour of Hanoi │
│         │                           │ A walking tour         │
└─────────┴───────────────────────────┴────────────────────────┘

────────────────────────

Mẹo so sánh:

1. Trip vs. Journey:
• Trip nhấn mạnh mục đích: đi 2 ngày rồi về → gọi là a trip (We went on a weekend trip).
";
        assert_eq!(cleaned, expected);
        // Every table line is the same number of cells wide, so it lines up.
        let widths: Vec<usize> =
            cleaned.lines().filter(|l| l.starts_with(['┌', '│', '├', '└'])).map(display_width).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn a_table_wraps_its_widest_column_to_fit_the_note() {
        let table = "| Lệnh | Tác dụng |\n|---|---|\n| go vet | Bắt lỗi hay gặp, ví dụ sai format của Printf |\n";
        let cleaned = clean_pasted_markdown(table, 30).unwrap();
        for line in cleaned.lines() {
            assert_eq!(display_width(line), 30, "{line}");
        }
        assert!(cleaned.contains("│ go vet │ Bắt lỗi hay gặp,"), "{cleaned}");
    }

    #[test]
    fn a_table_too_wide_for_the_note_becomes_records() {
        let table = "| A | B | C | D | E |\n|---|---|---|---|---|\n| một | hai | ba | bốn<br>x | Năm.<br>Sáu |\n";
        let cleaned = clean_pasted_markdown(table, 20).unwrap();
        assert_eq!(cleaned, "• một\n  B: hai\n  C: ba\n  D: bốn; x\n  E: Năm. Sáu\n");
    }

    #[test]
    fn alignment_and_escaped_pipes_survive() {
        let table = "| x | y |\n|:-:|--:|\n| a \\| b | 1 |\n| c | 22 |";
        let cleaned = clean_pasted_markdown(table, 0).unwrap();
        assert_eq!(
            cleaned,
            "┌───────┬────┐\n│   x   │  y │\n├───────┼────┤\n│ a | b │  1 │\n│   c   │ 22 │\n└───────┴────┘"
        );
    }

    #[test]
    fn code_and_plain_text_paste_unchanged() {
        for text in [
            "# a comment\n- item\nfn main() {}",
            "def f(**kwargs):\n    return a**2 + b**2",
            "snake_case_name and 5 * 3 * 2",
            "costs $5 and $10, see $HOME/$PATH",
            "just a sentence.",
        ] {
            assert_eq!(clean_pasted_markdown(text, 40), None, "{text}");
        }
    }

    #[test]
    fn code_fences_keep_their_contents_verbatim() {
        let text = "Run **this**:\n\n```bash\nls **/*.rs | wc -l\n```\n";
        assert_eq!(clean_pasted_markdown(text, 40).unwrap(), "Run this:\n\nls **/*.rs | wc -l\n");
        let inline = "Use `**raw**` and **bold**";
        assert_eq!(clean_pasted_markdown(inline, 40).unwrap(), "Use **raw** and bold");
    }

    #[test]
    fn lists_quotes_links_and_entities() {
        let text = "**Notes**\n- [ ] todo\n- [x] done\n  - nested\n> quoted *text*\n[docs](https://go.dev/doc) &amp; <https://x.io>";
        assert_eq!(
            clean_pasted_markdown(text, 40).unwrap(),
            "Notes\n• ☐ todo\n• ☑ done\n  ◦ nested\n▏ quoted text\ndocs (https://go.dev/doc) & https://x.io"
        );
    }

    #[test]
    fn maths_becomes_unicode() {
        assert_eq!(latex_to_text(r"\rightarrow"), "→");
        assert_eq!(latex_to_text(r"x^2 + y_{1} \le \frac{a+b}{c}"), "x² + y₁ ≤ (a+b)/c");
        assert_eq!(latex_to_text(r"\sqrt{x^2+1} \times \pi"), "√(x²+1) × π");
        assert_eq!(latex_to_text(r"90^\circ, \alpha \to \beta"), "90°, α → β");
        assert_eq!(latex_to_text(r"\mathbb{R}^n, \text{ nếu } x \neq 0"), "ℝⁿ, nếu x ≠ 0");
        assert_eq!(latex_to_text(r"\unknown{x}"), r"\unknownx");
        let text = "Bold **a** so \\(E = mc^2\\) and $$\\sum_{i=1}^{n} i$$";
        assert_eq!(clean_pasted_markdown(text, 40).unwrap(), "Bold a so E = mc² and ∑ᵢ₌₁ⁿ i");
    }

    #[test]
    fn a_setext_underline_is_dropped_but_a_rule_is_drawn() {
        let text = "**Title**\n---\n\nbody\n\n---\n\nend";
        assert_eq!(
            clean_pasted_markdown(text, 10).unwrap(),
            "Title\n\nbody\n\n──────────\n\nend"
        );
    }

    #[test]
    fn the_edges_of_real_answers() {
        // No outer pipes, a row longer than its header, Windows line endings.
        let table = "a | b\r\n--- | ---\r\n1 | 2 | 3\r\n";
        assert_eq!(
            clean_pasted_markdown(table, 0).unwrap(),
            "┌───┬───┬───┐\n│ a │ b │   │\n├───┼───┼───┤\n│ 1 │ 2 │ 3 │\n└───┴───┴───┘\n"
        );
        let text = "Title\n===\n***both*** and **bold**, costs $5 and $10, 5 * 3, a**b, **open\nx&nbsp;y";
        assert_eq!(
            clean_pasted_markdown(text, 40).unwrap(),
            "Title\nboth and bold, costs $5 and $10, 5 * 3, a**b, **open\nx y"
        );
    }

    #[test]
    fn code_that_only_looks_like_markdown_is_left_alone() {
        for text in [
            "class A:\n    def __init__(self):\n        pass\n\nif __name__ == \"__main__\":\n    A()",
            "# build output\n**/node_modules/**\n**/target/**\n",
            "sed 's/\\(foo\\)/\\1/' file",
            "grep -E '\\[error\\]' log",
            "echo $$ > pid\n# done",
            "<div class=\"x\">a<br/>b</div>\n<span>c</span>",
        ] {
            assert_eq!(clean_pasted_markdown(text, 40), None, "{text}");
        }
    }

    #[test]
    fn fenced_code_keeps_its_blank_lines_and_trailing_spaces() {
        let text = "**Code**:\n\n```python\nimport os\n\n\ndef f():  \n    pass\n```\n\n\n\nafter";
        assert_eq!(
            clean_pasted_markdown(text, 40).unwrap(),
            "Code:\n\nimport os\n\n\ndef f():  \n    pass\n\nafter"
        );
    }

    #[test]
    fn links_keep_escaped_and_bracketed_urls_whole() {
        let text = "**See** [the page](https://en.wikipedia.org/wiki/Foo_(bar)) and [x](https://a.io/a\\_b)";
        assert_eq!(
            clean_pasted_markdown(text, 40).unwrap(),
            "See the page (https://en.wikipedia.org/wiki/Foo_(bar)) and x (https://a.io/a_b)"
        );
    }

    #[test]
    fn deep_braces_do_not_overflow_the_stack() {
        let deep = format!("$${}x{}$$", "{".repeat(100_000), "}".repeat(100_000));
        let cleaned = latex_to_text(&deep[2..deep.len() - 2]);
        assert!(cleaned.contains('x'));
    }

    #[test]
    fn emoji_take_two_cells_like_in_a_terminal() {
        assert_eq!(display_width("✅"), 2);
        assert_eq!(display_width("❌"), 2);
        assert_eq!(display_width("🚀"), 2);
        assert_eq!(display_width("✓"), 1);
        assert_eq!(display_width("→"), 1);
    }

    #[test]
    fn a_chatbot_answer_still_triggers_on_bold_maths_and_underscored_phrases() {
        assert_eq!(clean_pasted_markdown("**Note:** x", 40).unwrap(), "Note: x");
        assert_eq!(clean_pasted_markdown("Try \\(x^2\\) now **ok**", 40).unwrap(), "Try x² now ok");
        assert_eq!(clean_pasted_markdown("**A** and __bold words__ and __init__", 40).unwrap(), "A and bold words and __init__");
    }

    /// The diagram a chatbot answer carried, as it reached a note: the copy
    /// began at the first `┌`, so the top of the first box lost its indent.
    fn copied_diagram() -> Vec<&'static str> {
        vec![
            "┌──────────────────────────────┐",
            "               │         Kubernetes           │",
            "               └──────────────┬───────────────┘",
            "                              │",
            "         ┌────────────────────┴────────────────────┐",
            "         ▼                                         ▼",
            "┌────────────────────────┐               ┌────────────────────────┐",
            "│   Datashim Operator    │               │     AWS EBS CSI        │",
            "│ (csi-s3.example.com)   │               │   (ebs.csi.aws.com)    │",
            "└──────────┬─────────────┘               └──────────┬─────────────┘",
            "           │                                        │",
            "           ▼ (Mount S3 Dataset)                     ▼ (Mount Block Storage)",
            "    /mnt/datasets/...                         /mnt/checkpoints/...",
            "           │                                        │",
            "           └──────────────────┬─────────────────────┘",
            "                              ▼",
            "                     ┌──────────────────┐",
            "                     │   Training Pod   │",
            "                     └──────────────────",
        ]
    }

    #[test]
    fn a_copied_diagram_gets_its_first_line_back_in_place() {
        let lines = copied_diagram();
        let pasted = lines.join("\n");
        let restored = restore_drawing_indent(&pasted).expect("the first line moved");
        let first = restored.split('\n').next().unwrap();
        // The top of the box now sits over its sides, fifteen cells in.
        assert_eq!(first, format!("{}{}", " ".repeat(15), lines[0]));
        assert_eq!(restored.split('\n').skip(1).collect::<Vec<_>>(), lines[1..]);
        // Already in place, or not a drawing: left alone.
        assert_eq!(restore_drawing_indent(&restored), None);
        assert_eq!(restore_drawing_indent("┌──┐\n│ok│\n└──┘"), None);
        assert_eq!(restore_drawing_indent("hello\n  │ x │"), None);
        // One stroke is not enough to tell where it belonged.
        assert_eq!(restore_drawing_indent("─── a │\n      │"), None);
        // The paste path does the same, Markdown or not.
        assert_eq!(clean_paste(&pasted, 80), Some(restored));
    }

    #[test]
    fn every_line_of_a_diagram_is_set_in_the_fixed_width_face() {
        let lines = copied_diagram();
        let note = format!("Sơ đồ kiến trúc:\n{}\n\nKết luận: xong.", lines.join("\n"));
        // The arrow rows and the /mnt labels between the boxes too, but not
        // the sentence above it or the one after the blank line.
        assert_eq!(diagram_lines(&note), (1..=lines.len()).collect::<Vec<_>>());
        // A table's rows and rules, but not a pipe table or plain notes.
        let table = "Lệnh:\n│ go vet ./... │ Bắt lỗi │\n├──────┼──────┤\n| markdown | pipes |\nghi chú";
        assert_eq!(diagram_lines(table), vec![1, 2]);
        assert!(diagram_lines("").is_empty());
        // An ASCII box counts as a drawing as well.
        assert_eq!(diagram_lines("+-----+\n| app |\n+-----+"), vec![0, 2]);
    }

    #[test]
    fn a_diagram_in_a_markdown_answer_keeps_its_spacing_and_underscores() {
        let answer = "**Luồng:**\n┌──────────┐\n│ my_pod_a │    │ *x* │\n└──────────┘";
        assert_eq!(
            clean_pasted_markdown(answer, 80).unwrap(),
            "Luồng:\n┌──────────┐\n│ my_pod_a │    │ *x* │\n└──────────┘"
        );
    }

    #[test]
    fn vietnamese_tones_take_no_cell_of_their_own() {
        // Decomposed: e + combining circumflex + combining acute.
        assert_eq!(display_width("e\u{302}\u{301}"), 1);
        assert_eq!(display_width("ế"), 1);
        assert_eq!(display_width("中文"), 4);
    }
}
