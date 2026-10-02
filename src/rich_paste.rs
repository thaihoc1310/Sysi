//! What a copy carries besides its text. Ghostty, Heminus, VS Code and every
//! browser put an HTML flavour on the clipboard next to the plain one, holding
//! the colours, bold, italics and faint text the words were shown in. A note
//! keeps that look (see `NoteInk`); a source that copies plain text only, as
//! most terminals do, has nothing to keep.
//!
//! The plain flavour stays the text that is pasted. The HTML is only read for
//! its look, which is laid over the plain text by matching the characters both
//! hold, so nothing here has to render HTML the way the source did: spaces,
//! line breaks and blocks are skipped on both sides.

#[cfg(test)]
use crate::state::parse_hex_color;
use crate::state::NoteInk;

/// The look of one character, once the source's own text colour is gone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ink {
    color: Option<[u8; 3]>,
    bold: bool,
    italic: bool,
    dim: bool,
}

impl Ink {
    fn is_plain(self) -> bool {
        self == Self::default()
    }
}

/// How a run of HTML is shown, inherited down the elements.
#[derive(Clone, Copy, Debug)]
struct Look {
    color: Option<[u8; 3]>,
    /// The alpha of `color`; a faint terminal cell is often only that.
    color_alpha: f64,
    opacity: f64,
    bold: bool,
    italic: bool,
    hidden: bool,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            color: None,
            color_alpha: 1.0,
            opacity: 1.0,
            bold: false,
            italic: false,
            hidden: false,
        }
    }
}

/// Text shown below this much of its colour reads as faint: Ghostty and xterm
/// write a faint cell as `opacity: 0.5`.
const DIM_BELOW: f64 = 0.75;
/// A colour whose channels sit closer than this (out of 1) is a grey: a text
/// colour, not a highlight colour.
const GREY_CHROMA: f64 = 0.1;
/// A grey in this band of lightness is the secondary text of a page or a
/// terminal (Claude Code's 153,153,153), which is faint text; darker or
/// lighter, it is the body text of a light or a dark theme.
const DIM_GREY: (f64, f64) = (0.30, 0.72);
/// An element whose colour covers this share of the text is the source's own
/// text colour (VS Code wraps a copy in a `<div>` of it), as is a colour that
/// covers that much of the text on its own.
const BASE_SHARE: f64 = 0.85;
/// No more stretches than this are kept from one paste: they are saved with
/// the note on every edit.
const MAX_RUNS: usize = 12_000;
/// Text this long is pasted without its look rather than make the paste slow.
const MAX_CHARS: usize = 400_000;

/// The text of a `text/html` clipboard flavour. Chromium and Electron write
/// UTF-8; Firefox has written UTF-16, with and without a byte-order mark.
pub fn decode_html(bytes: &[u8]) -> String {
    let utf16 = |little: bool, bytes: &[u8]| {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    let text = match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(true, rest),
        [0xFE, 0xFF, rest @ ..] => utf16(false, rest),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        // "<\0" — UTF-16LE with no mark.
        [b'<', 0, ..] => utf16(true, bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    };
    text.trim_end_matches('\0').to_owned()
}

/// The look `html` gives the characters of `text`, as stretches in characters
/// from the start of `text`. Empty when the HTML has no look to keep, or holds
/// text too unlike `text` to be laid over it.
pub fn html_ink(html: &str, text: &str) -> Vec<NoteInk> {
    if html.len() > MAX_CHARS * 8 || text.chars().count() > MAX_CHARS {
        return Vec::new();
    }
    let source = styled_chars(html);
    if source.iter().all(|(_, ink)| ink.is_plain()) {
        return Vec::new();
    }
    let target: Vec<char> = text.chars().collect();
    let Some(mut inks) = align(&source, &target) else {
        return Vec::new();
    };
    bridge_spaces(&target, &mut inks);
    let runs = runs(&inks);
    if runs.len() > MAX_RUNS {
        return Vec::new();
    }
    runs
}

// ------------------------------------------------------------------ HTML

/// Elements that never hold text and are never closed.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
/// Elements whose content is not shown.
const UNSHOWN: &[&str] = &[
    "head", "script", "style", "title", "template", "noscript", "svg",
];

struct Frame {
    name: String,
    look: Look,
    /// The colour this element set itself, and where its text began.
    declared: Option<[u8; 3]>,
    start: usize,
}

/// Every character the HTML shows, with how it is shown. The colours that are
/// only the source's text colour are already taken off.
fn styled_chars(html: &str) -> Vec<(char, Ink)> {
    let mut out: Vec<(char, Look)> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    // Elements that set a colour: (colour, first char, end char).
    let mut painted: Vec<([u8; 3], usize, usize)> = Vec::new();
    let mut rest = html;
    let current = |stack: &[Frame]| stack.last().map(|frame| frame.look).unwrap_or_default();
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            push_text(&mut out, rest, current(&stack));
            break;
        };
        push_text(&mut out, &rest[..open], current(&stack));
        rest = &rest[open..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let starts_tag = rest[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
        let Some(close) = tag_end(rest).filter(|_| starts_tag) else {
            // A lone '<' is text.
            push_text(&mut out, "<", current(&stack));
            rest = &rest[1..];
            continue;
        };
        let tag = &rest[1..close];
        rest = &rest[close + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            let name = tag_name(name);
            if let Some(at) = stack.iter().rposition(|frame| frame.name == name) {
                for frame in stack.drain(at..).rev() {
                    if let Some(color) = frame.declared {
                        painted.push((color, frame.start, out.len()));
                    }
                }
            }
            continue;
        }
        let name = tag_name(tag);
        if name.is_empty() {
            continue;
        }
        if VOID.contains(&name.as_str()) {
            if name == "br" {
                out.push(('\n', current(&stack)));
            }
            continue;
        }
        if UNSHOWN.contains(&name.as_str()) && name != "svg" && name != "head" {
            // Raw text: skip straight to its end tag.
            rest = match end_tag(rest, &name) {
                Some(at) => rest[at..].find('>').map_or("", |gt| &rest[at + gt + 1..]),
                None => "",
            };
            continue;
        }
        let parent = current(&stack);
        let (look, declared) = element_look(&name, tag, parent);
        if tag.trim_end().ends_with('/') {
            continue;
        }
        stack.push(Frame {
            name,
            look,
            declared,
            start: out.len(),
        });
    }
    for frame in stack.drain(..).rev() {
        if let Some(color) = frame.declared {
            painted.push((color, frame.start, out.len()));
        }
    }
    resolve(&out, &painted)
}

/// Where the tag opening `text` closes: the first `>` outside a quoted value.
fn tag_end(text: &str) -> Option<usize> {
    // No tag a source writes is this long; a stray quote must not send every
    // later '<' scanning to the end of the document.
    const LONGEST_TAG: usize = 1 << 16;
    let mut quote = None;
    for (at, c) in text
        .char_indices()
        .skip(1)
        .take_while(|(at, _)| *at < LONGEST_TAG)
    {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

/// Where `</name` next starts in `text`, in any case.
fn end_tag(text: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find("</") {
        let at = from + at;
        let after = &text.as_bytes()[at + 2..];
        if after.len() >= name.len() && after[..name.len()].eq_ignore_ascii_case(name.as_bytes()) {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

fn tag_name(tag: &str) -> String {
    tag.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn push_text(out: &mut Vec<(char, Look)>, text: &str, look: Look) {
    if text.is_empty() || look.hidden {
        return;
    }
    out.extend(decode_entities(text).chars().map(|c| (c, look)));
}

/// The value of attribute `name` in the inside of a start tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag.trim_start();
    // Past the element name.
    rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-');
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            return None;
        }
        let key_end = rest
            .find(|c: char| c == '=' || c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(rest.len());
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let body = &after[1..];
                    let end = body.find(q).unwrap_or(body.len());
                    rest = body.get(end + 1..).unwrap_or("");
                    body[..end].to_owned()
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    rest = &after[end..];
                    after[..end].to_owned()
                }
            }
        } else {
            if key.is_empty() {
                // Nothing we can read; step over one character.
                let mut chars = rest.chars();
                chars.next();
                rest = chars.as_str();
                continue;
            }
            String::new()
        };
        if key == name {
            return Some(decode_entities(&value));
        }
    }
}

fn element_look(name: &str, tag: &str, parent: Look) -> (Look, Option<[u8; 3]>) {
    let mut look = parent;
    let mut declared = None;
    match name {
        "b" | "strong" | "th" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => look.bold = true,
        "i" | "em" | "cite" | "var" | "dfn" => look.italic = true,
        "head" | "svg" => look.hidden = true,
        _ => {}
    }
    if attribute(tag, "hidden").is_some() {
        look.hidden = true;
    }
    if name == "font" {
        if let Some((rgb, alpha)) = attribute(tag, "color").as_deref().and_then(parse_color) {
            look.color = Some(rgb);
            look.color_alpha = alpha;
            declared = Some(rgb);
        }
    }
    let Some(style) = attribute(tag, "style") else {
        return (look, declared);
    };
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim().to_ascii_lowercase();
        let value = value
            .trim()
            .trim_end_matches("!important")
            .trim()
            .to_ascii_lowercase();
        match property.as_str() {
            "color" => {
                if let Some((rgb, alpha)) = parse_color(&value) {
                    look.color = Some(rgb);
                    look.color_alpha = alpha;
                    declared = Some(rgb);
                }
            }
            "opacity" => {
                if let Some(opacity) = parse_fraction(&value) {
                    look.opacity *= opacity;
                }
            }
            "font-weight" => {
                look.bold = match value.as_str() {
                    "bold" | "bolder" => true,
                    "normal" | "lighter" => false,
                    number => number.parse::<f64>().map_or(look.bold, |w| w >= 600.0),
                }
            }
            "font-style" => {
                look.italic = value.starts_with("italic") || value.starts_with("oblique")
            }
            "display" if value == "none" => look.hidden = true,
            "visibility" if value == "hidden" || value == "collapse" => look.hidden = true,
            _ => {}
        }
    }
    (look, declared)
}

/// `0.5` or `50%`, clamped to 0..=1.
fn parse_fraction(value: &str) -> Option<f64> {
    let value = value.trim();
    let number = match value.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f64>().ok()? / 100.0,
        None => value.parse::<f64>().ok()?,
    };
    number.is_finite().then(|| number.clamp(0.0, 1.0))
}

/// A CSS colour as RGB and alpha: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`,
/// `rgb()` / `rgba()` with commas or spaces, or a basic named colour.
/// Anything else (`currentcolor`, `var(--x)`, `hsl()`) is no colour.
fn parse_color(value: &str) -> Option<([u8; 3], f64)> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()?;
        let wide = |hi: u8, lo: u8| hi * 16 + lo;
        return match digits.as_slice() {
            [r, g, b] => Some(([r * 17, g * 17, b * 17], 1.0)),
            [r, g, b, a] => Some(([r * 17, g * 17, b * 17], f64::from(a * 17) / 255.0)),
            [r1, r2, g1, g2, b1, b2] => {
                Some(([wide(*r1, *r2), wide(*g1, *g2), wide(*b1, *b2)], 1.0))
            }
            [r1, r2, g1, g2, b1, b2, a1, a2] => Some((
                [wide(*r1, *r2), wide(*g1, *g2), wide(*b1, *b2)],
                f64::from(wide(*a1, *a2)) / 255.0,
            )),
            _ => None,
        };
    }
    if let Some(inner) = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner
            .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let channel = |part: &str| -> Option<u8> {
            let number = match part.strip_suffix('%') {
                Some(percent) => percent.parse::<f64>().ok()? * 2.55,
                None => part.parse::<f64>().ok()?,
            };
            number
                .is_finite()
                .then(|| number.round().clamp(0.0, 255.0) as u8)
        };
        let rgb = [channel(parts[0])?, channel(parts[1])?, channel(parts[2])?];
        let alpha = parts.get(3).map_or(Some(1.0), |a| parse_fraction(a))?;
        return Some((rgb, alpha));
    }
    let named = match value {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "gray" | "grey" => [128, 128, 128],
        "silver" => [192, 192, 192],
        "red" => [255, 0, 0],
        "maroon" => [128, 0, 0],
        "green" => [0, 128, 0],
        "lime" => [0, 255, 0],
        "blue" => [0, 0, 255],
        "navy" => [0, 0, 128],
        "yellow" => [255, 255, 0],
        "olive" => [128, 128, 0],
        "orange" => [255, 165, 0],
        "purple" => [128, 0, 128],
        "fuchsia" | "magenta" => [255, 0, 255],
        "teal" => [0, 128, 128],
        "aqua" | "cyan" => [0, 255, 255],
        _ => return None,
    };
    Some((named, 1.0))
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map(|end| end + 1)
            .unwrap_or(rest.len());
        let name = &rest[1..end];
        let decoded = if let Some(number) = name.strip_prefix('#') {
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => number.parse::<u32>().ok(),
            };
            code.and_then(char::from_u32)
        } else {
            match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "ensp" => Some('\u{2002}'),
                "emsp" => Some('\u{2003}'),
                "thinsp" => Some('\u{2009}'),
                "ndash" => Some('–'),
                "mdash" => Some('—'),
                "hellip" => Some('…'),
                "lsquo" => Some('‘'),
                "rsquo" => Some('’'),
                "ldquo" => Some('“'),
                "rdquo" => Some('”'),
                "bull" => Some('•'),
                "middot" => Some('·'),
                "copy" => Some('©'),
                "reg" => Some('®'),
                "trade" => Some('™'),
                "times" => Some('×'),
                "rarr" => Some('→'),
                "larr" => Some('←'),
                _ => None,
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end..];
                rest = rest.strip_prefix(';').unwrap_or(rest);
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A grey: a colour text is set in rather than one that marks it out.
fn is_grey(rgb: [u8; 3]) -> bool {
    let max = *rgb.iter().max().unwrap_or(&0);
    let min = *rgb.iter().min().unwrap_or(&0);
    f64::from(max - min) / 255.0 < GREY_CHROMA
}

fn lightness(rgb: [u8; 3]) -> f64 {
    let max = f64::from(*rgb.iter().max().unwrap_or(&0));
    let min = f64::from(*rgb.iter().min().unwrap_or(&0));
    (max + min) / 2.0 / 255.0
}

/// Takes the source's own text colour off, so the note's shows instead: that
/// colour was picked for the source's background, not the note's.
fn resolve(out: &[(char, Look)], painted: &[([u8; 3], usize, usize)]) -> Vec<(char, Ink)> {
    let mut shown = vec![0usize; out.len() + 1];
    for (at, (c, _)) in out.iter().enumerate() {
        shown[at + 1] = shown[at] + usize::from(!c.is_whitespace());
    }
    let total = shown[out.len()];
    let covers = |count: usize| total > 0 && count as f64 >= total as f64 * BASE_SHARE;
    let mut base: Vec<[u8; 3]> = painted
        .iter()
        .filter(|(_, start, end)| covers(shown[*end] - shown[*start]))
        .map(|(color, _, _)| *color)
        .collect();
    let mut counts: Vec<([u8; 3], usize)> = Vec::new();
    for (c, look) in out {
        if let (Some(color), false) = (look.color, c.is_whitespace()) {
            match counts.iter_mut().find(|(seen, _)| *seen == color) {
                Some((_, count)) => *count += 1,
                None => counts.push((color, 1)),
            }
        }
    }
    base.extend(
        counts
            .iter()
            .filter(|(_, count)| covers(*count))
            .map(|(color, _)| *color),
    );
    out.iter()
        .map(|&(c, look)| {
            let mut dim = look.color_alpha * look.opacity < DIM_BELOW;
            let color = look.color.filter(|color| !base.contains(color));
            let color = match color {
                Some(color) if is_grey(color) => {
                    let light = lightness(color);
                    dim |= light >= DIM_GREY.0 && light <= DIM_GREY.1;
                    None
                }
                other => other,
            };
            (
                c,
                Ink {
                    color,
                    bold: look.bold,
                    italic: look.italic,
                    dim,
                },
            )
        })
        .collect()
}

// ------------------------------------------------------------- alignment

/// How far ahead either side may run past text the other does not have: a
/// bullet, a hidden label, the borders of a redrawn table.
const RESYNC_WINDOW: usize = 96;
/// How many characters in a row must agree before the two sides are taken to
/// be back in step.
const RESYNC_RUN: usize = 4;

/// The look of every character of `target`, read off the matching character
/// of `source`. Only characters that are not spaces are matched. `None` when
/// too few match for the look to belong to this text.
fn align(source: &[(char, Ink)], target: &[char]) -> Option<Vec<Ink>> {
    let from: Vec<usize> = (0..source.len())
        .filter(|&at| !source[at].0.is_whitespace())
        .collect();
    let to: Vec<usize> = (0..target.len())
        .filter(|&at| !target[at].is_whitespace())
        .collect();
    let same = |t: usize, s: usize| target[to[t]] == source[from[s]].0;
    // Both sides agree for a run from here, or to the end of the shorter.
    let agree = |t: usize, s: usize| {
        let run = RESYNC_RUN.min(to.len() - t).min(from.len() - s);
        run > 0 && (0..run).all(|k| same(t + k, s + k))
    };
    let mut inks = vec![Ink::default(); target.len()];
    let (mut t, mut s, mut matched) = (0, 0, 0);
    while t < to.len() && s < from.len() {
        if same(t, s) {
            inks[to[t]] = source[from[s]].1;
            matched += 1;
            t += 1;
            s += 1;
            continue;
        }
        let skip = (1..=RESYNC_WINDOW).find_map(|k| {
            if s + k < from.len() && agree(t, s + k) {
                Some((0, k))
            } else if t + k < to.len() && agree(t + k, s) {
                Some((k, 0))
            } else {
                None
            }
        });
        match skip {
            Some((dt, ds)) => {
                t += dt;
                s += ds;
            }
            // One character each side that differs: a bullet for a dash.
            None => {
                t += 1;
                s += 1;
            }
        }
    }
    let shorter = to.len().min(from.len());
    (shorter > 0 && matched * 2 >= shorter).then_some(inks)
}

/// A space between two characters of one look takes it too, so a bold phrase
/// or a faint line is one stretch and not one per word.
fn bridge_spaces(target: &[char], inks: &mut [Ink]) {
    let mut last: Option<usize> = None;
    for at in 0..target.len() {
        if target[at].is_whitespace() {
            continue;
        }
        if let Some(previous) = last {
            if at > previous + 1 && inks[previous] == inks[at] && !inks[at].is_plain() {
                for gap in previous + 1..at {
                    inks[gap] = inks[at];
                }
            }
        }
        last = Some(at);
    }
}

fn runs(inks: &[Ink]) -> Vec<NoteInk> {
    let mut runs: Vec<NoteInk> = Vec::new();
    let mut at = 0;
    while at < inks.len() {
        let ink = inks[at];
        let start = at;
        while at < inks.len() && inks[at] == ink {
            at += 1;
        }
        if !ink.is_plain() {
            runs.push(NoteInk {
                start: start as i32,
                end: at as i32,
                color: ink.color,
                bold: ink.bold,
                italic: ink.italic,
                dim: ink.dim,
            });
        }
    }
    runs
}

// ------------------------------------------------------------- legibility

fn relative_luminance(rgb: [f64; 3]) -> f64 {
    let linear = |c: f64| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// What a note in this text colour is assumed to sit on: light text is on a
/// dark plate or dimmed glass, dark text on a light plate.
fn backdrop(text: [f64; 3]) -> [f64; 3] {
    if relative_luminance(text) > 0.4 {
        [0.11, 0.11, 0.12]
    } else {
        [0.98, 0.98, 0.99]
    }
}

/// The copied colour, made as light or as dark as it has to be to read on
/// the note: a terminal's yellow is no use on a white LIGHT note. The hue is
/// kept; only the lightness moves, toward the note's own text colour.
pub fn legible(rgb: [u8; 3], text: [f64; 3]) -> [f64; 3] {
    const MIN_CONTRAST: f64 = 4.5;
    let color = rgb.map(|c| f64::from(c) / 255.0);
    let ground = backdrop(text);
    if contrast(color, ground) >= MIN_CONTRAST {
        return color;
    }
    let toward = if relative_luminance(text) > relative_luminance(ground) {
        [1.0; 3]
    } else {
        [0.0; 3]
    };
    // Mixing toward white or black keeps the hue; the first mix that reads is
    // the one closest to the copied colour.
    (1..=20)
        .map(|step| f64::from(step) / 20.0)
        .map(|mix| {
            [0, 1, 2].map(|channel| color[channel] + (toward[channel] - color[channel]) * mix)
        })
        .find(|mixed| contrast(*mixed, ground) >= MIN_CONTRAST)
        .unwrap_or(toward)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(start: i32, end: i32) -> NoteInk {
        NoteInk {
            start,
            end,
            ..NoteInk::default()
        }
    }

    #[test]
    fn a_ghostty_copy_keeps_its_colours_bold_italic_and_faint_text() {
        let html = "<div style=\"font-family: monospace; white-space: pre;\">\
            <div style=\"display: inline;\"><span style=\"color: rgb(152, 195, 121);\">kind</span>: App</div><br>\
            <span style=\"font-weight: bold;\">bold</span> <span style=\"font-style: italic;\">it</span> \
            <span style=\"opacity: 0.5;\">faint words</span></div>";
        let text = "kind: App\nbold it faint words";
        assert_eq!(
            html_ink(html, text),
            vec![
                NoteInk {
                    color: parse_hex_color("#98c379"),
                    ..ink(0, 4)
                },
                NoteInk {
                    bold: true,
                    ..ink(10, 14)
                },
                NoteInk {
                    italic: true,
                    ..ink(15, 17)
                },
                NoteInk {
                    dim: true,
                    ..ink(18, 29)
                },
            ]
        );
    }

    #[test]
    fn the_sources_own_text_colour_is_not_kept() {
        // VS Code wraps the copy in a div of the theme's text colour.
        let html = "<meta charset='utf-8'><div style=\"color: #cccccc;background-color: #1f1f1f;\
            font-family: 'Droid Sans Mono', monospace;white-space: pre;\"><div>\
            <span style=\"color: #569cd6;\">const</span><span style=\"color: #cccccc;\"> x = </span>\
            <span style=\"color: #b5cea8;\">1</span><span style=\"color: #cccccc;\">;</span></div></div>";
        assert_eq!(
            html_ink(html, "const x = 1;"),
            vec![
                NoteInk {
                    color: parse_hex_color("#569cd6"),
                    ..ink(0, 5)
                },
                NoteInk {
                    color: parse_hex_color("#b5cea8"),
                    ..ink(10, 11)
                },
            ]
        );
        // A page's body text is a grey, set on every block rather than once.
        let html = "<p style=\"color: rgb(31, 35, 40);\">Read the <a style=\"color: rgb(9, 105, 218);\">docs</a></p>\
            <p style=\"color: rgb(31, 35, 40);\">first.</p>";
        assert_eq!(
            html_ink(html, "Read the docs\n\nfirst."),
            vec![NoteInk {
                color: parse_hex_color("#0969da"),
                ..ink(9, 13)
            }]
        );
        // Grey secondary text is faint text.
        let html = "<span style=\"color: rgb(153, 153, 153);\">⎿ done</span> ok";
        assert_eq!(
            html_ink(html, "⎿ done ok"),
            vec![NoteInk {
                dim: true,
                ..ink(0, 6)
            }]
        );
    }

    #[test]
    fn a_heminus_copy_from_xterm_keeps_its_look() {
        // xterm's serialize addon, as Heminus puts it on the clipboard.
        let html = "<html><head><meta charset=\"utf-8\"></head><body><!--StartFragment--><pre>\
            <div style='color: #c9d1d9; background-color: #1e2228; font-family: \"JetBrains Mono\", monospace; font-size: 14px;'>\
            <div><span>$ </span><span style='color: #7ee787; font-weight: bold;'>ls</span><span> -la</span></div>\
            <div><span style='opacity: 0.5;'>total 8</span></div>\
            <div><span style='color: #000000; background-color: #BFBFBF;'>sel</span></div>\
            </div></pre><!--EndFragment--></body></html>";
        assert_eq!(
            html_ink(html, "$ ls -la\ntotal 8\nsel"),
            vec![
                NoteInk {
                    color: parse_hex_color("#7ee787"),
                    bold: true,
                    ..ink(2, 4)
                },
                NoteInk {
                    dim: true,
                    ..ink(9, 16)
                },
            ]
        );
        // Script and style bodies are never text, whatever their case.
        let html = "<STYLE>p{}</STYLE><Script>x<y</SCRIPT><i>shown</i>";
        assert_eq!(
            html_ink(html, "shown"),
            vec![NoteInk {
                italic: true,
                ..ink(0, 5)
            }]
        );
    }

    #[test]
    fn plain_html_gives_nothing() {
        assert!(html_ink("<p>just <span>words</span></p>", "just words").is_empty());
        assert!(html_ink("", "words").is_empty());
        // The HTML of some other text.
        let html = "<b>completely different</b>";
        assert!(html_ink(html, "nothing alike here at all").is_empty());
    }

    #[test]
    fn a_browser_copy_lines_up_past_what_the_plain_text_lacks_or_adds() {
        let html = "<html><head><style>b{color:red}</style><title>t</title></head><body>\
            <!--StartFragment--><ul><li><strong>First</strong> point</li>\
            <li><em>Second</em> &amp; last&nbsp;one</li></ul><!--EndFragment--></body></html>";
        let text = "• First point\n• Second & last one";
        assert_eq!(
            html_ink(html, text),
            vec![
                NoteInk {
                    bold: true,
                    ..ink(2, 7)
                },
                NoteInk {
                    italic: true,
                    ..ink(16, 22)
                },
            ]
        );
    }

    #[test]
    fn a_redrawn_table_keeps_the_look_of_its_words() {
        // The plain text was a terminal table the paste redrew in boxes.
        let html = "<span style=\"color: #61afef;\">PreSync</span> | Before";
        let text = "┌─────────┬────────┐\n│ PreSync │ Before │\n└─────────┴────────┘";
        let runs = html_ink(html, text);
        assert_eq!(runs.len(), 1);
        let start = text.chars().position(|c| c == 'P').unwrap() as i32;
        assert_eq!((runs[0].start, runs[0].end), (start, start + 7));
    }

    #[test]
    fn colours_parse_in_every_form_the_sources_write() {
        assert_eq!(parse_color("#abc"), Some(([0xaa, 0xbb, 0xcc], 1.0)));
        assert_eq!(
            parse_color("#11223380").map(|c| c.0),
            Some([0x11, 0x22, 0x33])
        );
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(([1, 2, 3], 1.0)));
        assert_eq!(parse_color("rgba(1,2,3,0.5)"), Some(([1, 2, 3], 0.5)));
        assert_eq!(parse_color("rgb(1 2 3 / 50%)"), Some(([1, 2, 3], 0.5)));
        assert_eq!(parse_color("teal"), Some(([0, 128, 128], 1.0)));
        assert_eq!(parse_color("var(--fg)"), None);
        assert_eq!(parse_color("#12"), None);
        assert_eq!(
            decode_entities("a &lt;b&gt; &#39;c&#x27; &bogus; &"),
            "a <b> 'c' &bogus; &"
        );
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("<b>ạ</b>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_html(&utf16), "<b>ạ</b>");
        assert_eq!(decode_html("<b>ạ</b>\0".as_bytes()), "<b>ạ</b>");
        assert_eq!(parse_hex_color("#0969da"), Some([9, 105, 218]));
        assert_eq!(parse_hex_color("0969da"), None);
        assert_eq!(parse_hex_color("#ééé"), None);
    }

    #[test]
    fn a_quoted_font_list_or_a_stray_bracket_does_not_lose_the_rest() {
        let html = "<div style='color: #eee; font-family: \"JetBrains Mono\", monospace'>\
            a < b <span style='color:#e06c75'>red</span></div>";
        assert_eq!(
            html_ink(html, "a < b red"),
            vec![NoteInk {
                color: parse_hex_color("#e06c75"),
                ..ink(6, 9)
            }]
        );
    }

    #[test]
    fn a_copied_colour_is_made_to_read_on_the_note() {
        let dark_text = [0.07, 0.07, 0.07];
        let light_text = [0.96, 0.96, 0.96];
        // Ghostty's yellow on a LIGHT note goes darker, keeping its hue.
        let yellow = legible([229, 192, 123], dark_text);
        assert!(contrast(yellow, backdrop(dark_text)) >= 4.5);
        assert!(yellow[0] > yellow[2] && yellow[1] > yellow[2]);
        // A dark blue on a DARK note goes lighter.
        let blue = legible([0, 0, 160], light_text);
        assert!(contrast(blue, backdrop(light_text)) >= 4.5);
        assert!(blue[2] > blue[0]);
        // A colour that already reads is left as it is.
        let green = legible([152, 195, 121], light_text);
        assert_eq!(green, [152.0 / 255.0, 195.0 / 255.0, 121.0 / 255.0]);
    }
}
