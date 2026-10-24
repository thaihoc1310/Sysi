//! The Oxford Learner's Dictionary, read from its web pages.
//!
//! Oxford's own API is paid, and every free one is a scraper of these same
//! pages, so this reads them itself. The search page does the hard matching:
//! it sends an inflection to its root ("children" to "child"), a phrasal verb
//! to its own page and an idiom to the entry that holds it ("kick the bucket"
//! to "bucket"). What comes back is checked against the query all the same,
//! since a search for anything lands somewhere.
//!
//! Like the rest of the lookups this runs on a worker thread under
//! `panic = "abort"`, so nothing here unwraps.

use crate::translate::{self, EnDefinition};

const SITE: &str = "https://www.oxfordlearnersdictionaries.com/";
/// The same word's other parts of speech fetched alongside the first: "court"
/// is a noun on one page and a verb on the next.
const MAX_OTHER_ENTRIES: usize = 3;
/// Oxford gives some senses a dozen examples; the rest are a click away there.
const MAX_EXAMPLES: usize = 4;
/// Idioms shown for one phrase that more than one of them says.
const MAX_PHRASES: usize = 3;

/// Oxford's senses for `query`, the same word's other parts of speech
/// included, or nothing when Oxford has no entry that is this word.
pub fn lookup(query: &str) -> Vec<EnDefinition> {
    let url = format!(
        "{SITE}search/english/?q={}",
        translate::percent_encode(query.trim())
    );
    let Ok(page) = translate::http_get(&url) else {
        return Vec::new();
    };
    let document = Document::parse(&page);
    let Some(found) = read_entry(&document, query) else {
        return Vec::new();
    };
    let mut definitions = found.definitions;
    if found.whole_entry {
        // At once rather than one after another; a page that cannot get a
        // thread is fetched here instead.
        let others: Vec<_> = found
            .other_entries
            .into_iter()
            .take(MAX_OTHER_ENTRIES)
            .map(|url| {
                std::thread::Builder::new()
                    .name("sysi-oxford".to_owned())
                    .spawn({
                        let url = url.clone();
                        move || translate::http_get(&url)
                    })
                    .map_err(|_| url)
            })
            .collect();
        for other in others {
            let page = match other {
                Ok(thread) => thread.join().ok().and_then(Result::ok),
                Err(url) => translate::http_get(&url).ok(),
            };
            let Some(page) = page else {
                continue;
            };
            // The other page is the same headword in another part of speech,
            // so it has to answer the same query on its own.
            if let Some(entry) = read_entry(&Document::parse(&page), query) {
                definitions.extend(entry.definitions);
            }
        }
    }
    definitions
}

struct Entry {
    definitions: Vec<EnDefinition>,
    /// The whole entry was the word, rather than one idiom or phrasal verb
    /// inside it; only then are the headword's other entries the word too.
    whole_entry: bool,
    /// The same headword's other entries, by URL.
    other_entries: Vec<String>,
}

fn read_entry(document: &Document, query: &str) -> Option<Entry> {
    let entry = document.find(ROOT, &|node| {
        document.nodes[node].tag == "div" && document.has_class(node, "entry")
    })?;
    let headword_node = document.find(entry, &|node| document.has_class(node, "headword"))?;
    let headword = document.text(headword_node, &|_| false);
    let pos = document
        .find(entry, &|node| document.has_class(node, "pos"))
        .map(|node| document.text(node, &|_| false))
        .unwrap_or_default();
    let wanted = normalize(query);
    if wanted.is_empty() {
        return None;
    }
    if normalize(&headword) == wanted || is_listed_form(document, entry, &wanted, &headword) {
        let mut definitions = Vec::new();
        collect_senses(document, entry, &headword, &pos, &mut definitions);
        let this = &document.nodes[entry].id;
        let mut others = other_entries(document, &headword);
        others.retain(|url| !url.ends_with(&format!("/{this}")));
        return (!definitions.is_empty()).then_some(Entry {
            definitions,
            whole_entry: true,
            other_entries: others,
        });
    }
    // Not the headword: the idioms or phrasal verbs inside the entry that say
    // the query word for word, under any of their titles. Shortest first, and
    // each of them: "in spite of" is both "in spite of something" and "in
    // spite of yourself", and which was meant is the reader's to see.
    if !wanted.contains(' ') {
        return None;
    }
    let mut matches: Vec<(usize, usize, String, &str)> = Vec::new();
    for (group_class, title_class, label) in
        [("idm-g", "idm", "idiom"), ("pv-g", "pv", "phrasal verb")]
    {
        for group in document.all(entry, &|node| document.has_class(node, group_class)) {
            let Some(title) = document.find(group, &|node| document.has_class(node, title_class))
            else {
                continue;
            };
            let title = document.text(title, &|_| false);
            let Some(length) = std::iter::once(title.clone())
                .chain(variants(document, group))
                .flat_map(|name| phrase_readings(&name))
                .filter(|reading| format!(" {reading} ").contains(&format!(" {wanted} ")))
                .map(|reading| reading.len())
                .min()
            else {
                continue;
            };
            matches.push((group, length, title, label));
        }
    }
    matches.sort_by_key(|&(_, length, _, _)| length);
    let mut definitions = Vec::new();
    for (group, _, title, label) in matches.into_iter().take(MAX_PHRASES) {
        for sense in document.all(group, &|node| document.has_class(node, "sense")) {
            if let Some(definition) = read_sense(document, sense, &format!("{label} · {title}")) {
                definitions.push(definition);
            }
        }
    }
    (!definitions.is_empty()).then_some(Entry {
        definitions,
        whole_entry: false,
        other_entries: Vec::new(),
    })
}

/// The other spellings and wordings given at the head of an entry or idiom,
/// "(also café)" under "cafe": not those of its senses.
fn variants(document: &Document, of: usize) -> Vec<String> {
    let Some(top) = document.find(of, &|node| document.has_class(node, "top-container")) else {
        return Vec::new();
    };
    document
        .all(top, &|node| document.has_class(node, "v"))
        .into_iter()
        .map(|variant| document.text(variant, &|_| false))
        .collect()
}

/// The ways an idiom's title can be said, normalized: each of its
/// alternatives ("your/somebody’s court" is "your court" and "somebody's
/// court") with and without its optional part ("hold court (with
/// somebody)").
fn phrase_readings(title: &str) -> Vec<String> {
    let mut bare = String::new();
    let mut depth = 0usize;
    for character in title.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => bare.push(character),
            _ => {}
        }
    }
    let unbracketed = title.replace(['(', ')'], " ");
    let mut readings = Vec::new();
    for text in [bare, unbracketed] {
        let mut variants = vec![String::new()];
        for word in text.split_whitespace() {
            let choices: Vec<&str> = word
                .split('/')
                .filter(|choice| !choice.is_empty())
                .collect();
            // A title with many slashes would multiply out; past a handful,
            // keep the first choice of the rest.
            let choices = if variants.len() * choices.len() > 32 {
                &choices[..choices.len().min(1)]
            } else {
                &choices[..]
            };
            variants = variants
                .iter()
                .flat_map(|variant| {
                    choices
                        .iter()
                        .map(move |choice| format!("{variant} {choice}"))
                })
                .collect();
        }
        readings.extend(variants.iter().map(|variant| normalize(variant)));
    }
    readings.sort();
    readings.dedup();
    readings
}

/// Whether the entry lists `wanted` among its forms ("children" under
/// "child", "ran" in the verb table), or it is a regular inflection of it.
fn is_listed_form(document: &Document, entry: usize, wanted: &str, headword: &str) -> bool {
    if translate::cambridge_headword_matches(wanted, headword)
        || variants(document, entry)
            .iter()
            .any(|variant| normalize(variant) == wanted)
    {
        return true;
    }
    document
        .all(entry, &|node| {
            document.has_class(node, "inflections") || document.has_class(node, "verb_forms_table")
        })
        .into_iter()
        .any(|forms| {
            let forms = normalize(&document.text(forms, &|_| false));
            format!(" {forms} ").contains(&format!(" {wanted} "))
        })
}

/// Every sense of the entry proper, under its part of speech and the heading
/// of its group of senses ("noun · law"). Idioms are their own words and stay
/// out; a phrasal verb page is a set of groups, each under its own title.
fn collect_senses(
    document: &Document,
    entry: usize,
    headword: &str,
    pos: &str,
    out: &mut Vec<EnDefinition>,
) {
    let skip = |node: usize| {
        document.has_class(node, "idioms")
            || document.has_class(node, "collapse")
            || document.nodes[node].tag == "aside"
    };
    for sense in document.all_outside(entry, &|node| document.has_class(node, "sense"), &skip) {
        let mut label = pos.to_owned();
        if let Some(group) = document.ancestor(sense, &|node| document.has_class(node, "pv-g")) {
            if let Some(title) = document.find(group, &|node| document.has_class(node, "pv")) {
                let title = document.text(title, &|_| false);
                if normalize(&title) != normalize(headword) {
                    label = join(&label, &title);
                }
            }
        }
        if let Some(group) = document.ancestor(sense, &|node| document.has_class(node, "shcut-g")) {
            if let Some(heading) = document.find(group, &|node| document.has_class(node, "shcut")) {
                label = join(&label, &document.text(heading, &|_| false));
            }
        }
        if let Some(definition) = read_sense(document, sense, &label) {
            out.push(definition);
        }
    }
}

fn join(label: &str, more: &str) -> String {
    match (label.is_empty(), more.is_empty()) {
        (true, _) => more.to_owned(),
        (_, true) => label.to_owned(),
        _ => format!("{label} · {more}"),
    }
}

/// One sense: its grammar, register and context ahead of the definition, as
/// Oxford prints them, and its first few examples. A sense that only points
/// elsewhere ("past tense of go") is that pointer.
fn read_sense(document: &Document, sense: usize, label: &str) -> Option<EnDefinition> {
    // What belongs to the sense itself, not to an example, a box or a link.
    let own = |node: usize| {
        document.has_class(node, "examples")
            || document.has_class(node, "collapse")
            || document.has_class(node, "xrefs")
            || document.has_class(node, "symbols")
            || document.has_class(node, "topic-g")
            || document.has_class(node, "un")
    };
    let first = |class: &str| {
        document
            .all_outside(sense, &|node| document.has_class(node, class), &own)
            .first()
            .map(|&node| document.text(node, &|_| false))
            .filter(|text| !text.is_empty())
    };
    let definition = first("def").or_else(|| {
        let xrefs = document.find(sense, &|node| document.has_class(node, "xrefs"))?;
        Some(document.text(xrefs, &|_| false)).filter(|text| !text.is_empty())
    })?;
    let text = [
        first("grammar"),
        first("labels"),
        first("dis-g"),
        Some(definition),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let examples = document
        .all_outside(
            sense,
            &|node| document.has_class(node, "examples"),
            &|node| document.has_class(node, "collapse"),
        )
        .into_iter()
        .flat_map(|list| document.all(list, &|node| document.has_class(node, "x")))
        .map(|example| document.text(example, &|_| false))
        .filter(|example| !example.is_empty())
        .take(MAX_EXAMPLES)
        .collect();
    Some(EnDefinition {
        pos: label.to_owned(),
        text,
        examples,
    })
}

/// The links under "Other results" that are this headword again: Oxford marks
/// them `arl1`, apart from compounds (`arl2`) and lookalikes (`arl3`).
fn other_entries(document: &Document, headword: &str) -> Vec<String> {
    let Some(related) = document.find(ROOT, &|node| document.nodes[node].id == "relatedentries")
    else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    for link in document.all(related, &|node| document.nodes[node].tag == "a") {
        let same = document
            .find(link, &|node| document.has_class(node, "arl1"))
            .is_some();
        let href = &document.nodes[link].href;
        // Only Oxford's own definition pages: the page is not ours to follow
        // anywhere else.
        if same && href.starts_with(&format!("{SITE}definition/english/")) && !urls.contains(href) {
            urls.push(href.clone());
        }
    }
    // A headword with no other entries still lists its compounds as arl1 at
    // times; they are a different word.
    urls.retain(|url| {
        let slug = url.rsplit('/').next().unwrap_or_default();
        let slug = slug.split(['_', '?', '#']).next().unwrap_or_default();
        normalize(slug) == normalize(headword)
    });
    urls
}

fn normalize(text: &str) -> String {
    translate::normalize_match_text(text).trim().to_owned()
}

// ------------------------------------------------------------- a little HTML

/// Just enough of an HTML tree for Oxford's pages: elements with their class,
/// id and href, and text. Oxford's markup is generated and regular; this
/// copes with the rest (void and self-closed elements, stray end tags,
/// comments, scripts) by never failing, only reading less.
struct Document {
    nodes: Vec<Node>,
}

const ROOT: usize = 0;

#[derive(Default)]
struct Node {
    /// Lowercase tag name; empty for text.
    tag: String,
    class: String,
    id: String,
    href: String,
    text: String,
    parent: usize,
    children: Vec<usize>,
}

const BLOCK: &[&str] = &[
    "br", "dd", "div", "dt", "h1", "h2", "h3", "h4", "li", "ol", "p", "table", "td", "th", "tr",
    "ul",
];

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

impl Document {
    fn parse(html: &str) -> Self {
        let mut nodes = vec![Node::default()];
        let mut open = vec![ROOT];
        let mut rest = html;
        while !rest.is_empty() {
            let Some(at) = rest.find('<') else {
                push_text(&mut nodes, &open, rest);
                break;
            };
            push_text(&mut nodes, &open, &rest[..at]);
            rest = &rest[at..];
            if let Some(after) = rest.strip_prefix("<!--") {
                rest = after.find("-->").map_or("", |end| &after[end + 3..]);
                continue;
            }
            if rest.starts_with("<!") || rest.starts_with("<?") {
                rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
                continue;
            }
            if let Some(after) = rest.strip_prefix("</") {
                let end = after.find('>').unwrap_or(after.len());
                let name = after[..end].trim().to_ascii_lowercase();
                // Close up to the matching element; an end tag for nothing
                // open is dropped.
                if let Some(depth) = open.iter().rposition(|&node| nodes[node].tag == name) {
                    if depth > 0 {
                        open.truncate(depth);
                    }
                }
                rest = after.get(end + 1..).unwrap_or("");
                continue;
            }
            let Some((tag, consumed)) = read_tag(rest) else {
                // A lone "<" in text.
                push_text(&mut nodes, &open, "<");
                rest = &rest[1..];
                continue;
            };
            rest = &rest[consumed..];
            let parent = *open.last().unwrap_or(&ROOT);
            let index = nodes.len();
            let raw = tag.name == "script" || tag.name == "style";
            let leaf = tag.closed || VOID.contains(&tag.name.as_str()) || raw;
            nodes.push(Node {
                tag: tag.name.clone(),
                class: tag.class,
                id: tag.id,
                href: tag.href,
                parent,
                ..Node::default()
            });
            nodes[parent].children.push(index);
            if raw {
                // Their text is code, not words; skip to the end tag.
                let close = format!("</{}", tag.name);
                rest = find_ascii_case_insensitive(rest, &close).map_or("", |end| {
                    rest[end..].find('>').map_or("", |gt| &rest[end + gt + 1..])
                });
            } else if !leaf {
                open.push(index);
            }
        }
        Self { nodes }
    }

    fn has_class(&self, node: usize, class: &str) -> bool {
        self.nodes[node]
            .class
            .split_ascii_whitespace()
            .any(|name| name == class)
    }

    /// The first node under `from`, in document order, that is `wanted`.
    fn find(&self, from: usize, wanted: &dyn Fn(usize) -> bool) -> Option<usize> {
        self.all(from, wanted).into_iter().next()
    }

    fn all(&self, from: usize, wanted: &dyn Fn(usize) -> bool) -> Vec<usize> {
        self.all_outside(from, wanted, &|_| false)
    }

    /// Every node under `from` that is `wanted`, not looking inside any that
    /// is `skip` (nor inside a node that was wanted: senses do not nest).
    fn all_outside(
        &self,
        from: usize,
        wanted: &dyn Fn(usize) -> bool,
        skip: &dyn Fn(usize) -> bool,
    ) -> Vec<usize> {
        let mut found = Vec::new();
        let mut stack: Vec<usize> = self.nodes[from].children.iter().rev().copied().collect();
        while let Some(node) = stack.pop() {
            if wanted(node) {
                found.push(node);
                continue;
            }
            if skip(node) {
                continue;
            }
            stack.extend(self.nodes[node].children.iter().rev());
        }
        found
    }

    fn ancestor(&self, from: usize, wanted: &dyn Fn(usize) -> bool) -> Option<usize> {
        let mut node = from;
        while node != ROOT {
            node = self.nodes[node].parent;
            if wanted(node) {
                return Some(node);
            }
        }
        None
    }

    /// The words under `from`, spaced as the page shows them, leaving out
    /// anything `skip`.
    fn text(&self, from: usize, skip: &dyn Fn(usize) -> bool) -> String {
        let mut text = String::new();
        let mut stack = vec![from];
        while let Some(node) = stack.pop() {
            if node != from && skip(node) {
                continue;
            }
            // Cells and rows are apart on the page even with no space
            // between them in the markup.
            if BLOCK.contains(&self.nodes[node].tag.as_str()) {
                text.push(' ');
            }
            text.push_str(&self.nodes[node].text);
            stack.extend(self.nodes[node].children.iter().rev());
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        // Spans end before punctuation that the page sets flush: "court ."
        text.replace(" .", ".")
            .replace(" ,", ",")
            .replace("( ", "(")
            .replace(" )", ")")
    }
}

fn push_text(nodes: &mut Vec<Node>, open: &[usize], raw: &str) {
    if raw.is_empty() {
        return;
    }
    let parent = *open.last().unwrap_or(&ROOT);
    let index = nodes.len();
    nodes.push(Node {
        text: decode_entities(raw),
        parent,
        ..Node::default()
    });
    nodes[parent].children.push(index);
}

struct Tag {
    name: String,
    class: String,
    id: String,
    href: String,
    /// Written `<span/>`.
    closed: bool,
}

/// A start tag at the head of `input`, and how many bytes it took.
fn read_tag(input: &str) -> Option<(Tag, usize)> {
    let bytes = input.as_bytes();
    let mut at = 1;
    while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'-') {
        at += 1;
    }
    if at == 1 {
        return None;
    }
    let mut tag = Tag {
        name: input[1..at].to_ascii_lowercase(),
        class: String::new(),
        id: String::new(),
        href: String::new(),
        closed: false,
    };
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        match bytes.get(at) {
            None => return Some((tag, input.len())),
            Some(b'>') => return Some((tag, at + 1)),
            Some(b'/') => {
                at += 1;
                if bytes.get(at) == Some(&b'>') {
                    tag.closed = true;
                    return Some((tag, at + 1));
                }
                continue;
            }
            _ => {}
        }
        let start = at;
        while at < bytes.len()
            && !matches!(bytes[at], b'=' | b'>' | b'/')
            && !bytes[at].is_ascii_whitespace()
        {
            at += 1;
        }
        let name = input[start..at].to_ascii_lowercase();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = "";
        if bytes.get(at) == Some(&b'=') {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let end = input[at + 1..]
                        .find(quote as char)
                        .map_or(input.len(), |end| at + 1 + end);
                    value = &input[at + 1..end];
                    at = (end + 1).min(input.len());
                }
                _ => {
                    let start = at;
                    while at < bytes.len() && bytes[at] != b'>' && !bytes[at].is_ascii_whitespace()
                    {
                        at += 1;
                    }
                    value = &input[start..at];
                }
            }
        }
        match name.as_str() {
            "class" => tag.class = decode_entities(value),
            "id" => tag.id = decode_entities(value),
            "href" => tag.href = decode_entities(value),
            _ => {}
        }
    }
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.find(';').filter(|&end| end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let character = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => name
                            .strip_prefix('#')
                            .and_then(|decimal| decimal.parse().ok()),
                    };
                    code.and_then(char::from_u32)
                }
            };
            character.map(|character| (character, end))
        });
        match decoded {
            Some((character, end)) => {
                out.push(character);
                rest = &rest[end + 1..];
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

#[cfg(test)]
mod tests {
    use super::{decode_entities, read_entry, Document};

    fn entry(body: &str) -> String {
        format!(r##"<html><body><div class="entry" id="x_1">{body}</div></body></html>"##)
    }

    const COURT: &str = r##"<div class="top-container"><div class="webtop">
        <h1 class="headword" id="court_h_1">court</h1> <span class="pos">noun</span>
        <div class="symbols"><a href="#"><span class="ox3ksym_b1"> </span></a></div></div></div>
        <ol class="senses_multiple"><span class="shcut-g"><h2 class="shcut">law</h2>
        <li class="sense"><span class="sensetop"><div class="symbols"><a><span> </span></a></div></span>
          <span class="grammar">[countable, uncountable]</span>
          <span class="def">the place where legal trials take place</span>
          <ul class="examples"><li><span class="cf">in court</span> <span class="x">Relatives were <span class="cl">in court</span>.</span></li>
          <li><span class="x">The case took five years to <span class="cl">come to court</span> <span class="gloss">(= to be heard)</span>.</span></li></ul>
          <span class="xrefs"><span class="prefix">see also</span> <a class="Ref" href="#"><span class="xh">courthouse</span></a></span>
          <div class="collapse"><span class="unbox"><ul class="examples"><li><span class="x">a boxed example</span></li></ul></span></div>
        </li></span>
        <li class="sense"><span class="labels">(British English)</span> <span class="def">a tennis court</span></li></ol>
        <div class="idioms"><span class="idm-g"><div class="top-container"><div class="webtop">
          <span class="idm">the ball is in your/somebody’s court</span></div></div>
          <ol class="sense_single"><li class="sense"><span class="def">it is your turn to act</span></li></ol></span></div>
        <div id="relatedentries"><ul>
          <li><a href="https://www.oxfordlearnersdictionaries.com/definition/english/court_2"><span class='arl1'>court <pos-g><pos>verb</pos></pos-g></span></a></li>
          <li><a href="https://www.oxfordlearnersdictionaries.com/definition/english/law-court"><span class='arl2'>law court</span></a></li>
          <li><a href="https://example.test/definition/english/court_3"><span class='arl1'>court</span></a></li>
        </ul></div>"##;

    #[test]
    fn a_word_reads_its_senses_without_idioms_boxes_or_links() {
        let document = Document::parse(&entry(COURT));
        let found = read_entry(&document, "Court").unwrap();
        assert!(found.whole_entry);
        assert_eq!(found.definitions.len(), 2);
        let first = &found.definitions[0];
        assert_eq!(first.pos, "noun · law");
        assert_eq!(
            first.text,
            "[countable, uncountable] the place where legal trials take place"
        );
        assert_eq!(
            first.examples,
            [
                "Relatives were in court.",
                "The case took five years to come to court (= to be heard).",
            ]
        );
        assert_eq!(found.definitions[1].pos, "noun");
        assert_eq!(
            found.definitions[1].text,
            "(British English) a tennis court"
        );
        // The verb is another page of the same word; compounds and other
        // sites are not.
        assert_eq!(
            found.other_entries,
            ["https://www.oxfordlearnersdictionaries.com/definition/english/court_2"]
        );
    }

    #[test]
    fn an_idiom_reads_only_its_own_senses() {
        let document = Document::parse(&entry(COURT));
        let found = read_entry(&document, "the ball is in your court").unwrap();
        assert!(!found.whole_entry);
        assert_eq!(found.definitions.len(), 1);
        assert_eq!(
            found.definitions[0].pos,
            "idiom · the ball is in your/somebody’s court"
        );
        assert_eq!(found.definitions[0].text, "it is your turn to act");
        // Words of the idiom in another order are not it.
        assert!(read_entry(&document, "court the ball").is_none());
    }

    #[test]
    fn a_landing_on_another_word_is_refused_unless_it_lists_the_form() {
        let child = entry(
            r##"<h1 class="headword">child</h1> <span class="pos">noun</span>
            <div class="inflections">(plural children <div class="sound"></div>)</div>
            <ol><li class="sense"><span class="def">a young human</span></li></ol>"##,
        );
        let document = Document::parse(&child);
        assert_eq!(
            read_entry(&document, "children").unwrap().definitions.len(),
            1
        );
        assert!(read_entry(&document, "childish").is_none());
        let cat = entry(
            r##"<h1 class="headword">cat</h1><ol><li class="sense"><span class="def">a pet</span></li></ol>"##,
        );
        assert!(read_entry(&Document::parse(&cat), "cats").is_some());
        assert!(read_entry(&Document::parse(&cat), "catalogue").is_none());
        let cafe = entry(
            r##"<div class="top-container"><h1 class="headword">cafe</h1>
            <div class="variants">(also <span class="v-g"><span class="v">café</span></span>)</div></div>
            <ol><li class="sense"><span class="def">a place to eat</span>
            <div class="variants">(also <span class="v">caff</span>)</div></li></ol>"##,
        );
        assert!(read_entry(&Document::parse(&cafe), "café").is_some());
        // A sense's own variants are not the entry's.
        assert!(read_entry(&Document::parse(&cafe), "caff").is_none());
        // A spellcheck page has no entry at all.
        assert!(read_entry(&Document::parse("<h1>No exact match found</h1>"), "asdfgh").is_none());
    }

    #[test]
    fn a_pointer_sense_and_phrasal_verb_groups() {
        let went = entry(
            r##"<h1 class="headword">went</h1> <span class="pos">verb</span><ol><li class="sense"><span class="sensetop">
            <span class="xrefs"><span class="prefix">past tense of</span> <a><span class="xh">go</span></a></span></span></li></ol>"##,
        );
        let found = read_entry(&Document::parse(&went), "went").unwrap();
        assert_eq!(found.definitions[0].text, "past tense of go");
        let take_off = entry(
            r##"<h1 class="headword">take off</h1> <span class="pos">phrasal verb</span>
            <span class="pv-g"><div class="webtop"><span class="pv">take off</span></div>
              <ol><li class="sense"><span class="dis-g"><span class="wrap">(</span><span class="dtxt">of an aircraft</span><span class="wrap">)</span></span>
              <span class="def">to leave the ground</span></li></ol></span>
            <span class="pv-g"><div class="webtop"><span class="pv">take somebody <span class="pvarr"/> off</span></div>
              <ol><li class="sense"><span class="def">to copy somebody’s voice</span></li></ol></span>"##,
        );
        let found = read_entry(&Document::parse(&take_off), "take off").unwrap();
        assert_eq!(found.definitions[0].pos, "phrasal verb");
        assert_eq!(
            found.definitions[0].text,
            "(of an aircraft) to leave the ground"
        );
        assert_eq!(found.definitions[1].pos, "phrasal verb · take somebody off");
    }

    #[test]
    fn rough_markup_parses_without_losing_the_rest() {
        let document = Document::parse(
            r##"<p>a &amp; b &#8217; &#x41; &bogus; & c<br><img src=x><span/>
            <!-- <span class="def">no</span> --><script>if (a < b) { "</span>" }</script>
            <i class=def>unquoted</i></b></p> tail <"##,
        );
        let text = document.text(super::ROOT, &|_| false);
        assert_eq!(text, "a & b \u{2019} A &bogus; & c unquoted tail <");
        let def = document.find(super::ROOT, &|node| document.has_class(node, "def"));
        assert_eq!(document.nodes[def.unwrap()].tag, "i");
        assert_eq!(decode_entities("&#10;"), "\n");
    }
}
