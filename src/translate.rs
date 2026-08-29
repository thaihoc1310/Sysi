//! Dictionary lookups and machine translation for the translate window.
//!
//! Every request here runs on a worker thread and reports back over an
//! async_channel that the GLib main loop drains, because the overlay has one UI
//! thread and a blocking HTTP call on it would freeze every other widget. The
//! release profile builds with `panic = "abort"`, so a panic on one of these
//! threads takes the whole overlay down with it — each parser below is written
//! to degrade to an empty value instead of unwrapping.

use async_channel::Sender;
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

// Cambridge serves its pronunciation audio only to something that looks like a
// browser, and the other three endpoints do not mind, so every request carries
// the same header.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(9);
const MAX_AUDIO_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DEFINITIONS: usize = 8;
const MAX_EXAMPLES: usize = 4;
const MAX_SUGGESTIONS: usize = 8;

/// What a worker sends back to the UI. Lookups and suggestions carry the
/// generation they were requested at so the receiver can drop replies that a
/// newer keystroke has already made stale.
pub enum TranslateEvent {
    Suggestions {
        generation: u64,
        items: Vec<String>,
    },
    Lookup {
        generation: u64,
        result: LookupResult,
    },
    AudioReady {
        url: String,
        path: PathBuf,
    },
    AudioFailed {
        url: String,
    },
}

pub struct LookupResult {
    pub query: String,
    pub kind: ResultKind,
}

pub enum ResultKind {
    /// A pasted sentence or paragraph: machine translation only.
    Sentence(SentenceResult),
    /// A single word or short phrase, assembled from every source that answered.
    Word(Box<WordResult>),
    /// Neither dictionary knew the word — offer close spellings instead.
    NotFound { suggestions: Vec<String> },
    /// Nothing could be reached at all.
    Error(String),
}

pub struct SentenceResult {
    pub source: String,
    pub translation: String,
    /// Set when the input was not English, so the header can say which way the
    /// translation ran.
    pub detected: Option<String>,
}

pub struct WordResult {
    pub headword: String,
    /// The one-line Vietnamese gloss, dropped when it just echoes the input.
    pub gloss: Option<String>,
    pub pronunciations: Vec<Pronunciation>,
    pub vi_entries: Vec<ViEntry>,
    pub en_definitions: Vec<EnDefinition>,
    pub examples: Vec<BilingualExample>,
}

pub struct Pronunciation {
    /// "uk" or "us".
    pub lang: String,
    pub ipa: String,
    pub audio_url: String,
}

/// One part-of-speech block of the Vietnamese dictionary, e.g. "tính từ" and
/// the meanings listed under it.
pub struct ViEntry {
    pub pos: String,
    pub meanings: Vec<String>,
}

pub struct EnDefinition {
    pub pos: String,
    pub text: String,
    pub examples: Vec<String>,
}

pub struct BilingualExample {
    /// Already converted to Pango markup: the searched word comes back wrapped
    /// in `<em>`, which becomes `<b>`.
    pub en_markup: String,
    pub vi: String,
}

// ---------------------------------------------------------------- entry points

/// Look a query up and send exactly one `Lookup` event.
///
/// A thread per request rather than one long-lived worker: a lookup that sits
/// on the 9s timeout would otherwise block the suggestions for everything the
/// user typed after it, and the generation counter already makes late replies
/// free to discard.
pub fn spawn_lookup(query: String, generation: u64, tx: Sender<TranslateEvent>) {
    spawn_named("sysi-lookup", move || {
        let kind = if is_sentence(&query) {
            translate_sentence(&query)
        } else {
            lookup_word(&query)
        };
        let _ = tx.send_blocking(TranslateEvent::Lookup {
            generation,
            result: LookupResult { query, kind },
        });
    });
}

/// Fetch the type-ahead completions for a prefix.
pub fn spawn_suggest(prefix: String, generation: u64, tx: Sender<TranslateEvent>) {
    spawn_named("sysi-suggest", move || {
        let url = format!(
            "https://api.datamuse.com/sug?s={}&max={MAX_SUGGESTIONS}",
            percent_encode(&prefix)
        );
        let items = http_get(&url)
            .ok()
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .map(|json| datamuse_words(&json))
            .unwrap_or_default();
        if items.is_empty() {
            return;
        }
        let _ = tx.send_blocking(TranslateEvent::Suggestions { generation, items });
    });
}

/// Put a pronunciation clip in the cache and report where it landed. Audio
/// events are keyed by URL rather than by generation: the receiver matches them
/// against the buttons currently on screen, so a clip that arrives after its
/// button is gone simply finds no one waiting.
pub fn spawn_audio(url: String, tx: Sender<TranslateEvent>) {
    spawn_named("sysi-audio", move || {
        let path = cached_audio_path(&url);
        if path.exists() {
            let _ = tx.send_blocking(TranslateEvent::AudioReady { url, path });
            return;
        }
        match download_audio(&url, &path) {
            Ok(()) => {
                let _ = tx.send_blocking(TranslateEvent::AudioReady { url, path });
            }
            Err(_) => {
                let _ = tx.send_blocking(TranslateEvent::AudioFailed { url });
            }
        }
    });
}

/// Play a cached clip through whichever system player is installed. There is no
/// audio crate in the tree, and spawning a detached process keeps decoding off
/// the UI thread for free.
pub fn play_audio(path: &Path) {
    let players: [&[&str]; 4] = [
        &["gst-play-1.0", "--quiet"],
        &["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet"],
        &["cvlc", "--intf", "dummy", "--play-and-exit"],
        &["mpv", "--really-quiet", "--no-video"],
    ];
    for player in players {
        let Some((program, args)) = player.split_first() else {
            continue;
        };
        let spawned = Command::new(program)
            .args(args)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            // Nothing reaps the player otherwise, and every clip played would
            // leave a zombie behind for as long as the overlay runs.
            spawn_named("sysi-audio-reap", move || {
                let _ = child.wait();
            });
            return;
        }
    }
}

fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, work: F) {
    let _ = thread::Builder::new().name(name.to_owned()).spawn(work);
}

// ------------------------------------------------------------------- lookups

fn translate_sentence(query: &str) -> ResultKind {
    match google_translate(query, "vi") {
        Ok((translation, detected)) => {
            // Google echoes Vietnamese input straight back when asked for
            // Vietnamese; flip the direction so a pasted Vietnamese paragraph
            // still gets translated.
            if detected.as_deref() == Some("vi") {
                if let Ok((english, _)) = google_translate(query, "en") {
                    return ResultKind::Sentence(SentenceResult {
                        source: query.to_owned(),
                        translation: english,
                        detected: Some("vi".into()),
                    });
                }
            }
            ResultKind::Sentence(SentenceResult {
                source: query.to_owned(),
                translation,
                detected: None,
            })
        }
        Err(error) => ResultKind::Error(error),
    }
}

fn lookup_word(query: &str) -> ResultKind {
    // The three sources are independent, so they run at once and the slowest
    // one sets the wait rather than the sum of all three.
    let cambridge = {
        let url = format!(
            "https://dictionary-api.eliaschen.dev/api/dictionary/en/{}",
            percent_encode(query)
        );
        spawn_fetch(url)
    };
    let tracau = {
        let url = format!(
            "https://api.tracau.vn/WBBcwnwQpV89/s/{}/en",
            percent_encode(query)
        );
        spawn_fetch(url)
    };
    let gloss = {
        let query = query.to_owned();
        thread::spawn(move || google_translate(&query, "vi"))
    };

    let cambridge = join_json(cambridge);
    let tracau = join_json(tracau);
    let gloss = gloss.join().ok().and_then(|result| result.ok());

    // A 404 from Cambridge is "no such word", not a failure to reach it, so it
    // must not count towards the offline check below.
    let reached = !matches!(cambridge, Fetched::Unreachable)
        || !matches!(tracau, Fetched::Unreachable)
        || gloss.is_some();
    if !reached {
        return ResultKind::Error("Could not reach the dictionary services".into());
    }

    let (pronunciations, en_definitions) = match &cambridge {
        Fetched::Json(json) => parse_cambridge(json),
        _ => (Vec::new(), Vec::new()),
    };
    let (vi_entries, examples) = match &tracau {
        Fetched::Json(json) => parse_tracau(json, query),
        _ => (Vec::new(), Vec::new()),
    };

    if en_definitions.is_empty() && vi_entries.is_empty() && examples.is_empty() {
        return ResultKind::NotFound {
            suggestions: fetch_did_you_mean(query),
        };
    }

    let gloss = gloss
        .map(|(text, _)| text)
        .filter(|text| !text.trim().is_empty())
        .filter(|text| !text.eq_ignore_ascii_case(query.trim()));

    ResultKind::Word(Box::new(WordResult {
        headword: query.trim().to_owned(),
        gloss,
        pronunciations,
        vi_entries,
        en_definitions,
        examples,
    }))
}

/// The outcome of one JSON request, keeping "the server said no" apart from
/// "the server was not there".
enum Fetched {
    Json(Value),
    /// Reached, but the answer was an error or unparseable (e.g. a 404).
    Empty,
    Unreachable,
}

fn spawn_fetch(url: String) -> thread::JoinHandle<Result<String, String>> {
    thread::spawn(move || http_get(&url))
}

fn join_json(handle: thread::JoinHandle<Result<String, String>>) -> Fetched {
    match handle.join() {
        Ok(Ok(body)) => match serde_json::from_str::<Value>(&body) {
            Ok(json) => Fetched::Json(json),
            Err(_) => Fetched::Empty,
        },
        // http_get maps a non-2xx status to Empty by returning Ok(String::new()),
        // so anything left here really is a transport failure.
        Ok(Err(_)) => Fetched::Unreachable,
        Err(_) => Fetched::Unreachable,
    }
}

fn fetch_did_you_mean(word: &str) -> Vec<String> {
    let url = format!(
        "https://api.datamuse.com/words?sp={}&max={MAX_SUGGESTIONS}",
        percent_encode(word)
    );
    http_get(&url)
        .ok()
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .map(|json| datamuse_words(&json))
        .unwrap_or_default()
        .into_iter()
        .filter(|item| !item.eq_ignore_ascii_case(word.trim()))
        .collect()
}

/// Ask Google for a translation, returning the text and the language it
/// detected. The free `dict-chrome-ex` client answers `[["text","en"]]`; the
/// older `translate_a/single` endpoint is refused outright from many networks,
/// which is why it is not used here.
fn google_translate(query: &str, target: &str) -> Result<(String, Option<String>), String> {
    let url = format!(
        "https://clients5.google.com/translate_a/t?client=dict-chrome-ex&sl=auto&tl={target}&q={}",
        percent_encode(query)
    );
    let body = http_get(&url)?;
    let json: Value =
        serde_json::from_str(&body).map_err(|_| "Translation service replied oddly".to_owned())?;
    parse_google(&json).ok_or_else(|| "Translation service replied oddly".to_owned())
}

// -------------------------------------------------------------------- parsers

/// Decide whether the input is prose to translate or a word to look up.
pub fn is_sentence(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Anything outside the Latin alphabet is never a Cambridge headword, so a
    // pasted Vietnamese line goes straight to the translator.
    if trimmed.chars().any(|c| c.is_alphabetic() && !c.is_ascii()) {
        return true;
    }
    if trimmed.contains(['.', '?', '!', ';', ':', '\n', ',']) {
        return true;
    }
    trimmed.split_whitespace().count() > 4
}

/// Percent-encode a query for use in a URL path or parameter. Hand-rolled
/// because the tree carries no URL crate and the rule is four lines long.
pub fn percent_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());
    for byte in input.trim().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Both datamuse endpoints answer with `[{"word": "..."}, ...]`.
fn datamuse_words(json: &Value) -> Vec<String> {
    json.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("word")?.as_str())
                .map(str::to_owned)
                .take(MAX_SUGGESTIONS)
                .collect()
        })
        .unwrap_or_default()
}

/// Pull the translated text and the detected source language out of a
/// `dict-chrome-ex` reply, which is `[["text","lang"]]` but degrades to a bare
/// `["text"]` for some inputs.
pub fn parse_google(json: &Value) -> Option<(String, Option<String>)> {
    let items = json.as_array()?;
    if let Some(text) = items.first().and_then(Value::as_str) {
        return Some((text.to_owned(), None));
    }
    // Multi-segment input comes back as one nested pair per sentence; join them
    // so a pasted paragraph reads as a paragraph.
    let mut text = String::new();
    let mut detected = None;
    for item in items {
        let pair = item.as_array()?;
        if let Some(segment) = pair.first().and_then(Value::as_str) {
            if !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text.push_str(segment);
        }
        if detected.is_none() {
            detected = pair.get(1).and_then(Value::as_str).map(str::to_owned);
        }
    }
    if text.is_empty() {
        return None;
    }
    Some((text, detected))
}

/// Read the Cambridge scraper's reply into a pronunciation row and a list of
/// definition blocks.
pub fn parse_cambridge(json: &Value) -> (Vec<Pronunciation>, Vec<EnDefinition>) {
    if json.get("error").is_some() {
        return (Vec::new(), Vec::new());
    }

    // The same UK/US pair repeats once per dictionary edition; the first of
    // each is the one the page shows.
    let mut pronunciations: Vec<Pronunciation> = Vec::new();
    if let Some(items) = json.get("pronunciation").and_then(Value::as_array) {
        for item in items {
            let lang = item.get("lang").and_then(Value::as_str).unwrap_or("");
            let ipa = item.get("pron").and_then(Value::as_str).unwrap_or("");
            let url = item.get("url").and_then(Value::as_str).unwrap_or("");
            if ipa.is_empty() || url.is_empty() {
                continue;
            }
            if pronunciations.iter().any(|kept| kept.lang == lang) {
                continue;
            }
            pronunciations.push(Pronunciation {
                lang: lang.to_owned(),
                ipa: ipa.to_owned(),
                audio_url: url.to_owned(),
            });
        }
    }
    // UK before US, the order the dictionary page prints them in.
    pronunciations.sort_by_key(|item| item.lang != "uk");

    let mut definitions = Vec::new();
    if let Some(items) = json.get("definition").and_then(Value::as_array) {
        for item in items {
            let text = item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .trim_end_matches(':')
                .trim()
                .to_owned();
            // Cambridge pads its list with example-only rows that have no
            // definition of their own; they read as orphans out of context.
            if text.is_empty() {
                continue;
            }
            let examples = item
                .get("example")
                .and_then(Value::as_array)
                .map(|examples| {
                    examples
                        .iter()
                        .filter_map(|example| example.get("text")?.as_str())
                        .map(|text| text.trim().to_owned())
                        .filter(|text| !text.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            definitions.push(EnDefinition {
                pos: item
                    .get("pos")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                text,
                examples,
            });
            if definitions.len() == MAX_DEFINITIONS {
                break;
            }
        }
    }
    (pronunciations, definitions)
}

/// Read tracau's reply: the Vietnamese dictionary arrives as a blob of HTML
/// under `tratu`, the bilingual examples as a plain list under `sentences`.
pub fn parse_tracau(json: &Value, query: &str) -> (Vec<ViEntry>, Vec<BilingualExample>) {
    let entries = json
        .get("tratu")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("fields")?.get("fulltext")?.as_str())
        .map(parse_tracau_fulltext)
        .unwrap_or_default();

    let mut examples = Vec::new();
    if let Some(items) = json.get("sentences").and_then(Value::as_array) {
        for item in items {
            let Some(fields) = item.get("fields") else {
                continue;
            };
            let en = fields.get("en").and_then(Value::as_str).unwrap_or("");
            let vi = fields.get("vi").and_then(Value::as_str).unwrap_or("");
            if en.is_empty() || vi.is_empty() {
                continue;
            }
            examples.push(BilingualExample {
                en_markup: em_to_pango_bold(en),
                vi: strip_tags(vi),
            });
            if examples.len() == MAX_EXAMPLES {
                break;
            }
        }
    }

    // tracau answers a nonsense query with example sentences that merely
    // contain the letters typed; without a dictionary entry they are not
    // evidence the word exists.
    if entries.is_empty() && !examples.is_empty() {
        let query = query.trim().to_lowercase();
        let matched = examples
            .iter()
            .any(|example| example.en_markup.to_lowercase().contains(&query));
        if !matched {
            examples.clear();
        }
    }

    (entries, examples)
}

/// Walk the `<table id="definition">` rows of a tracau entry. `tl` rows open a
/// part-of-speech block ("tính từ"), `mn` rows are the meanings under it.
///
/// Hand-rolled rather than pulled in as an HTML crate: the markup is a fixed,
/// machine-generated shape and this only needs the row ids.
pub fn parse_tracau_fulltext(html: &str) -> Vec<ViEntry> {
    let mut entries: Vec<ViEntry> = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<tr") {
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else { break };
        let open_tag = &rest[..open_end];
        let body_start = open_end + 1;
        let Some(close) = rest[body_start..].find("</tr>") else {
            break;
        };
        let body = &rest[body_start..body_start + close];
        let id = attribute(open_tag, "id").unwrap_or_default();
        let text = strip_tags(content_cell(body));
        if !text.is_empty() {
            match id.as_str() {
                "tl" => entries.push(ViEntry {
                    pos: text,
                    meanings: Vec::new(),
                }),
                "mn" => {
                    if entries.is_empty() {
                        entries.push(ViEntry {
                            pos: String::new(),
                            meanings: Vec::new(),
                        });
                    }
                    if let Some(entry) = entries.last_mut() {
                        entry.meanings.push(text);
                    }
                }
                _ => {}
            }
        }
        rest = &rest[body_start + close + "</tr>".len()..];
    }
    entries.retain(|entry| !entry.meanings.is_empty());
    entries
}

/// The text of a row lives in its `C_C` cell; the cells before it hold the
/// bullet glyphs the page draws in the margin ("*", "■"), which would otherwise
/// end up welded to the front of every meaning.
fn content_cell(row: &str) -> &str {
    let Some(start) = row.find("id=\"C_C\"") else {
        return row;
    };
    let cell = &row[start..];
    let Some(open_end) = cell.find('>') else {
        return row;
    };
    let body = &cell[open_end + 1..];
    match body.find("</td>") {
        Some(end) => &body[..end],
        None => body,
    }
}

fn attribute(open_tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = open_tag.find(&needle)? + needle.len();
    let rest = &open_tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Strip tags and decode the handful of entities tracau emits, collapsing the
/// whitespace the markup leaves behind.
pub fn strip_tags(html: &str) -> String {
    flatten_markup(html).trim().to_owned()
}

/// The same flattening without the trim, so a sentence can be rebuilt from
/// several pieces without losing the spaces that separated them.
fn flatten_markup(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut depth = 0usize;
    for character in html.chars() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if character.is_whitespace() => {
                if !text.ends_with(' ') {
                    text.push(' ');
                }
            }
            _ => text.push(character),
        }
    }
    decode_entities(&text)
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        // Ampersand last, so a literal "&amp;lt;" does not turn into a tag.
        .replace("&amp;", "&")
}

/// Convert a tracau example sentence to Pango markup, keeping the `<em>`
/// highlight around the searched word as bold. Each plain segment is escaped on
/// its own so the result is markup-safe.
pub fn em_to_pango_bold(html: &str) -> String {
    let mut markup = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<em>") {
        // Flattened rather than stripped: trimming each piece would weld the
        // highlighted word to the words on either side of it.
        markup.push_str(&escape_markup(&flatten_markup(&rest[..start])));
        rest = &rest[start + "<em>".len()..];
        let Some(end) = rest.find("</em>") else {
            markup.push_str(&escape_markup(&flatten_markup(rest)));
            return markup.trim().to_owned();
        };
        markup.push_str("<b>");
        markup.push_str(&escape_markup(flatten_markup(&rest[..end]).trim()));
        markup.push_str("</b>");
        rest = &rest[end + "</em>".len()..];
    }
    markup.push_str(&escape_markup(&flatten_markup(rest)));
    markup.trim().to_owned()
}

/// Escape the five characters Pango treats as markup.
pub fn escape_markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\'' => escaped.push_str("&apos;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

// ----------------------------------------------------------------------- http

fn http_get(url: &str) -> Result<String, String> {
    let agent = ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();
    match agent.get(url).call() {
        Ok(response) => response
            .into_string()
            .map_err(|error| format!("Could not read the reply: {error}")),
        // A 404 means the word is unknown, not that the service is down; hand
        // back an empty body so the caller treats it as "reached, no answer".
        Err(ureq::Error::Status(_, _)) => Ok(String::new()),
        Err(error) => Err(format!("Request failed: {error}")),
    }
}

/// Where a pronunciation clip is cached. Keyed by a hash of the URL so the same
/// clip is fetched once and every later click plays instantly.
pub fn cached_audio_path(url: &str) -> PathBuf {
    crate::state::cache_dir()
        .join("audio")
        .join(format!("{:016x}.mp3", fnv1a(url)))
}

fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn download_audio(url: &str, path: &Path) -> Result<(), String> {
    let agent = ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("Could not fetch the audio: {error}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_AUDIO_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read the audio: {error}"))?;
    if bytes.is_empty() {
        return Err("The audio was empty".into());
    }
    let Some(parent) = path.parent() else {
        return Err("The cache path has no parent".into());
    };
    fs::create_dir_all(parent).map_err(|error| format!("Could not create the cache: {error}"))?;
    // Write beside the target and rename, so a clip interrupted mid-download
    // never leaves a truncated file that later plays as silence.
    let temporary = path.with_extension("part");
    fs::write(&temporary, &bytes).map_err(|error| format!("Could not save the audio: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("Could not store the audio: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        cached_audio_path, em_to_pango_bold, escape_markup, is_sentence, parse_cambridge,
        parse_google, parse_tracau_fulltext, percent_encode, strip_tags,
    };

    #[test]
    fn a_single_word_is_not_a_sentence() {
        assert!(!is_sentence("inefficient"));
        assert!(!is_sentence("  kick the bucket "));
        assert!(!is_sentence(""));
    }

    #[test]
    fn punctuation_length_and_accents_all_mark_prose() {
        assert!(is_sentence("Hello there, how are you?"));
        assert!(is_sentence("one two three four five"));
        assert!(is_sentence("xin chào bạn"));
    }

    #[test]
    fn queries_are_percent_encoded_per_utf8_byte() {
        assert_eq!(percent_encode("kick the bucket"), "kick%20the%20bucket");
        assert_eq!(percent_encode(" hello "), "hello");
        assert_eq!(percent_encode("chào"), "ch%C3%A0o");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn tags_are_stripped_and_entities_decoded() {
        assert_eq!(
            strip_tags("<td id=\"C_C\">thiếu khả năng,\n  bất tài</td>"),
            "thiếu khả năng, bất tài"
        );
        assert_eq!(strip_tags("a &amp; b &#39;c&#39;"), "a & b 'c'");
    }

    #[test]
    fn the_vietnamese_dictionary_rows_group_under_their_part_of_speech() {
        // Shaped like the real page: every row leads with a margin cell holding
        // a bullet glyph, and the text sits in the C_C cell after it.
        let html = r##"<table id="definition">
            <tr id="pa"><td id="I_C"><font>◘</font></td><td id="C_C"><font>[,ini'fi∫ənt]</font></td></tr>
            <tr id="tl"><td id="I_C"><font color="#1a76bf">*</font></td><td id="C_C" colspan="2"><b><font>tính từ</font></b></td></tr>
            <tr id="mn"><td> </td><td id="I_C"><font>■</font></td><td id="C_C">thiếu khả năng, bất tài</td></tr>
            <tr id="mn"><td> </td><td id="I_C"><font>■</font></td><td id="C_C">không có hiệu quả</td></tr>
            <tr id="tl"><td id="I_C"><font>*</font></td><td id="C_C"><b>danh từ</b></td></tr>
            <tr id="mn"><td id="I_C"><font>■</font></td><td id="C_C">sự kém cỏi</td></tr>
        </table>"##;
        let entries = parse_tracau_fulltext(html);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pos, "tính từ");
        assert_eq!(entries[0].meanings, ["thiếu khả năng, bất tài", "không có hiệu quả"]);
        assert_eq!(entries[1].pos, "danh từ");
        assert_eq!(entries[1].meanings, ["sự kém cỏi"]);
    }

    #[test]
    fn a_part_of_speech_with_no_meanings_is_dropped() {
        assert!(parse_tracau_fulltext(r#"<tr id="tl"><td>tính từ</td></tr>"#).is_empty());
        assert!(parse_tracau_fulltext("no markup at all").is_empty());
    }

    #[test]
    fn the_searched_word_survives_as_bold_markup() {
        assert_eq!(
            em_to_pango_bold("An <em>inefficient</em> stove & a fan"),
            "An <b>inefficient</b> stove &amp; a fan"
        );
        assert_eq!(em_to_pango_bold("<em>Hello</em>?"), "<b>Hello</b>?");
        assert_eq!(
            em_to_pango_bold("a <em>b</em> <em>c</em> d"),
            "a <b>b</b> <b>c</b> d"
        );
        assert_eq!(em_to_pango_bold("plain <text>"), "plain");
    }

    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(escape_markup("a<b>&'\""), "a&lt;b&gt;&amp;&apos;&quot;");
    }

    #[test]
    fn google_replies_are_read_flat_or_nested() {
        let nested = serde_json::json!([["không hiệu quả", "en"]]);
        assert_eq!(
            parse_google(&nested),
            Some(("không hiệu quả".to_owned(), Some("en".to_owned())))
        );
        let flat = serde_json::json!(["xin chào"]);
        assert_eq!(parse_google(&flat), Some(("xin chào".to_owned(), None)));
        assert_eq!(parse_google(&serde_json::json!([])), None);
    }

    #[test]
    fn multi_sentence_replies_are_joined() {
        let json = serde_json::json!([["Xin chào.", "en"], ["Bạn khỏe không?", "en"]]);
        let (text, detected) = parse_google(&json).expect("a translation");
        assert_eq!(text, "Xin chào. Bạn khỏe không?");
        assert_eq!(detected.as_deref(), Some("en"));
    }

    #[test]
    fn cambridge_keeps_one_pronunciation_per_accent_uk_first() {
        let json = serde_json::json!({
            "pronunciation": [
                {"lang": "us", "pron": "/us-one/", "url": "us1.mp3"},
                {"lang": "uk", "pron": "/uk-one/", "url": "uk1.mp3"},
                {"lang": "us", "pron": "/us-two/", "url": "us2.mp3"},
            ],
            "definition": [
                {"pos": "adjective", "text": "not organized: ", "example": [{"text": "An example."}]},
                {"pos": "adjective", "text": "", "example": [{"text": "An orphan."}]},
            ]
        });
        let (pronunciations, definitions) = parse_cambridge(&json);
        assert_eq!(pronunciations.len(), 2);
        assert_eq!(pronunciations[0].lang, "uk");
        assert_eq!(pronunciations[0].ipa, "/uk-one/");
        assert_eq!(pronunciations[1].audio_url, "us1.mp3");
        // The trailing colon is chrome from the page, and the example-only row
        // has no definition to show.
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].text, "not organized");
        assert_eq!(definitions[0].examples, ["An example."]);
    }

    #[test]
    fn an_unknown_word_parses_to_nothing() {
        let json = serde_json::json!({"error": "word not found"});
        let (pronunciations, definitions) = parse_cambridge(&json);
        assert!(pronunciations.is_empty());
        assert!(definitions.is_empty());
    }

    #[test]
    fn an_audio_url_always_maps_to_the_same_cache_file() {
        let first = cached_audio_path("https://example.test/uk/word.mp3");
        assert_eq!(first, cached_audio_path("https://example.test/uk/word.mp3"));
        assert_ne!(first, cached_audio_path("https://example.test/us/word.mp3"));
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("mp3"));
    }
}
