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
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
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
    pub meanings: Vec<ViMeaning>,
}

/// A single sense, with whatever sentences the entry illustrates it by. Idioms
/// lean on these: no English dictionary here carries "in spite of", so the
/// examples tracau ships are the only thing showing how it is used.
pub struct ViMeaning {
    pub text: String,
    pub examples: Vec<ViExample>,
}

pub struct ViExample {
    pub en: String,
    /// tracau translates most, but not all, of its examples.
    pub vi: Option<String>,
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
    let failed = tx.clone();
    let started = spawn_named("sysi-lookup", {
        let query = query.clone();
        move || {
            let kind = if is_sentence(&query) {
                translate_sentence(&query)
            } else {
                lookup_word(&query)
            };
            let _ = tx.send_blocking(TranslateEvent::Lookup {
                generation,
                result: LookupResult { query, kind },
            });
        }
    });
    if !started {
        // The window is showing "Looking it up…" and only a Lookup event takes
        // it down, so a worker that never started still owes one back.
        let _ = failed.send_blocking(TranslateEvent::Lookup {
            generation,
            result: LookupResult {
                query,
                kind: ResultKind::Error("Could not start the lookup".into()),
            },
        });
    }
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

/// Start a worker, reporting whether it actually got off the ground. Every
/// caller has to cope with `false`: the process can be out of thread slots, and
/// silently dropping the work would leave the window waiting for a reply that
/// is never coming.
fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, work: F) -> bool {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(work)
        .is_ok()
}

/// One in-flight GET. Described by its URL rather than by a closure, so that a
/// request which could not get a thread can still be run in series at join
/// time instead of being dropped on the floor.
enum Fetch {
    Running(thread::JoinHandle<Result<String, String>>),
    Deferred(String),
}

fn spawn_fetch(name: &str, url: String) -> Fetch {
    let work = {
        let url = url.clone();
        move || http_get(&url)
    };
    match thread::Builder::new().name(name.to_owned()).spawn(work) {
        Ok(handle) => Fetch::Running(handle),
        Err(_) => Fetch::Deferred(url),
    }
}

impl Fetch {
    fn join(self) -> Fetched {
        let body = match self {
            Fetch::Running(handle) => match handle.join() {
                Ok(body) => body,
                // The worker died mid-request; treat it as unreachable rather
                // than as "the word does not exist".
                Err(_) => return Fetched::Unreachable,
            },
            Fetch::Deferred(url) => http_get(&url),
        };
        match body {
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(json) => Fetched::Json(json),
                // http_get turns a non-2xx status into an empty body, so an
                // unparseable reply here really is "reached, nothing to say".
                Err(_) => Fetched::Empty,
            },
            Err(_) => Fetched::Unreachable,
        }
    }
}

// ------------------------------------------------------------------- lookups

fn translate_sentence(query: &str) -> ResultKind {
    match google_translate(query, "vi") {
        Ok((translation, detected)) => {
            // Google echoes Vietnamese input straight back when asked for
            // Vietnamese; flip the direction so a pasted Vietnamese paragraph
            // still gets translated.
            if detected.as_deref() == Some("vi") {
                return match google_translate(query, "en") {
                    Ok((english, _)) => ResultKind::Sentence(SentenceResult {
                        source: query.to_owned(),
                        translation: english,
                        detected: Some("vi".into()),
                    }),
                    // Falling through would present Google's echo of the input
                    // as if it were the translation.
                    Err(error) => ResultKind::Error(error),
                };
            }
            ResultKind::Sentence(SentenceResult {
                source: query.to_owned(),
                translation,
                // Only a non-English source is worth naming in the heading.
                detected: detected.filter(|language| language != "en"),
            })
        }
        Err(error) => ResultKind::Error(error),
    }
}

fn lookup_word(query: &str) -> ResultKind {
    // The two dictionaries are independent, so they run at once and the slower
    // one sets the wait. The gloss runs here on the calling thread, which would
    // otherwise just block waiting for them.
    let cambridge = spawn_fetch(
        "sysi-cambridge",
        format!(
            "https://dictionary-api.eliaschen.dev/api/dictionary/en/{}",
            percent_encode(query)
        ),
    );
    let tracau = spawn_fetch(
        "sysi-tracau",
        format!(
            "https://api.tracau.vn/WBBcwnwQpV89/s/{}/en",
            percent_encode(query)
        ),
    );
    let gloss = google_translate(query, "vi").ok();

    let cambridge = cambridge.join();
    let tracau = tracau.join();

    // A 404 from Cambridge is "no such word", not a failure to reach it, so it
    // must not count towards the offline check below.
    let reached = !matches!(cambridge, Fetched::Unreachable)
        || !matches!(tracau, Fetched::Unreachable)
        || gloss.is_some();
    if !reached {
        return ResultKind::Error("Could not reach the dictionary services".into());
    }

    let (pronunciations, en_definitions) = match &cambridge {
        Fetched::Json(json) => parse_cambridge(json, query),
        _ => (Vec::new(), Vec::new()),
    };
    let (vi_entries, examples) = match &tracau {
        Fetched::Json(json) => parse_tracau(json, query),
        _ => (Vec::new(), Vec::new()),
    };

    if en_definitions.is_empty() && vi_entries.is_empty() && examples.is_empty() {
        // A single accented word that no English dictionary knows is far more
        // likely to be Vietnamese than a typo, and "did you mean" has nothing
        // useful to offer for it — translate it instead.
        if query.chars().any(|c| c.is_alphabetic() && !c.is_ascii()) {
            return translate_sentence(query);
        }
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
    // Punctuation that merely bookends the query says nothing about whether it
    // is prose: "hello." typed in a hurry is still a word to look up.
    let core = input.trim_matches(|c: char| !c.is_alphanumeric());
    if core.is_empty() {
        return false;
    }
    let words = core.split_whitespace().count();
    if words > 4 {
        return true;
    }
    // Punctuation *inside* the text is what separates clauses.
    if core.contains(['.', '?', '!', ';', ':', ',', '\n']) {
        return true;
    }
    // Several accented words together are a foreign phrase, not a headword. A
    // single one goes to the dictionary anyway, because English has borrowed
    // plenty of them ("café", "naïve", "résumé"); `lookup_word` falls back to
    // translating when no dictionary recognises it.
    words > 1 && core.chars().any(|c| c.is_alphabetic() && !c.is_ascii())
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
        // One odd element must not discard the segments already collected: the
        // endpoint is unversioned and free, and grows trailing fields.
        let Some(pair) = item.as_array() else {
            continue;
        };
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

/// Whether a Cambridge reply is actually about the word that was asked for.
///
/// The endpoint resolves by search rather than by slug, and answers a phrase it
/// does not carry with whatever it found first instead of a 404: "look after"
/// comes back as "look", "give up" as "give someone a heads-up", "of course" as
/// "course of action". Rendering those means quietly showing the definitions of
/// a different word.
pub fn cambridge_headword_matches(query: &str, headword: &str) -> bool {
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let headword = headword.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.eq_ignore_ascii_case(&headword) {
        return true;
    }
    // A lemma is still the right entry — "cats" answers with "cat", "happier"
    // with "happy" — but only one word can be lemmatised into one other word,
    // and it stays recognisably the same word.
    if query.contains(' ') || headword.contains(' ') || headword.is_empty() {
        return false;
    }
    let query = query.to_lowercase();
    let headword = headword.to_lowercase();
    let stem: String = query.chars().take(3).collect();
    headword.starts_with(&stem) || query.starts_with(&headword.chars().take(3).collect::<String>())
}

/// Read the Cambridge scraper's reply into a pronunciation row and a list of
/// definition blocks.
pub fn parse_cambridge(json: &Value, query: &str) -> (Vec<Pronunciation>, Vec<EnDefinition>) {
    // A present-but-null "error" is a success in JSON terms; only a real value
    // means the word was not found.
    if json.get("error").is_some_and(|error| !error.is_null()) {
        return (Vec::new(), Vec::new());
    }
    let headword = json.get("word").and_then(Value::as_str).unwrap_or_default();
    if !cambridge_headword_matches(query, headword) {
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
    // The same sentences as plain lowercase text, kept for the check below.
    let mut plain: Vec<String> = Vec::new();
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
            plain.push(strip_tags(en).to_lowercase());
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
    // evidence the word exists. Matched against the plain text rather than the
    // markup, which has had its apostrophes and ampersands escaped — "don't"
    // would never match `don&apos;t`.
    if entries.is_empty() && !examples.is_empty() {
        let needle = query.trim().to_lowercase();
        if !plain.iter().any(|example| example.contains(&needle)) {
            examples.clear();
        }
    }

    (entries, examples)
}

/// What one row of a tracau definition table carries.
///
/// The ids vary by the kind of entry, and a single headword can mix them: a
/// plain word uses `tl`/`mn`, an idiom block uses `tn`/`tn_n`, and a saying or
/// slang block suffixes everything with `_ss`. Missing the variants is why
/// multi-word lookups came back blank — "in spite of" carries no `mn` row at
/// all, so the whole entry was skipped.
enum TracauRow {
    /// Opens a block: a part of speech, or a label like "thành ngữ".
    Heading,
    Meaning,
    Example,
    /// The Vietnamese for the example just above it.
    ExampleTranslation,
    Ignored,
}

fn classify_row(id: &str) -> TracauRow {
    // `_ss` marks the saying/slang block and appears in the middle of the id
    // (`mh_ss_n`), so it is removed wherever it sits rather than trimmed.
    match id.replace("_ss", "").as_str() {
        "tl" => TracauRow::Heading,
        "mn" | "tn_n" => TracauRow::Meaning,
        "mh" | "tn_mh" => TracauRow::Example,
        "mh_n" | "tn_mh_n" => TracauRow::ExampleTranslation,
        // `pa` is the phonetic spelling, which Cambridge already supplies in
        // IPA, and `tn` merely repeats the headword being looked up.
        _ => TracauRow::Ignored,
    }
}

/// Walk the `<table id="definition">` rows of a tracau entry, folding them into
/// blocks of meanings and the sentences that illustrate them.
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
            match classify_row(id) {
                TracauRow::Heading => entries.push(ViEntry {
                    pos: text,
                    meanings: Vec::new(),
                }),
                TracauRow::Meaning => {
                    last_entry(&mut entries).meanings.push(ViMeaning {
                        text,
                        examples: Vec::new(),
                    });
                }
                TracauRow::Example => {
                    let meaning = last_meaning(&mut entries);
                    meaning.examples.push(ViExample { en: text, vi: None });
                }
                TracauRow::ExampleTranslation => {
                    // Belongs to the example directly above; with none to
                    // attach to it would read as an untranslated sentence.
                    if let Some(example) = last_meaning(&mut entries).examples.last_mut() {
                        example.vi = Some(text);
                    }
                }
                TracauRow::Ignored => {}
            }
        }
        rest = &rest[body_start + close + "</tr>".len()..];
    }
    entries.retain(|entry| !entry.meanings.is_empty());
    entries
}

/// The block rows are being folded into, opening an unlabelled one if the entry
/// started with a meaning rather than a heading.
fn last_entry(entries: &mut Vec<ViEntry>) -> &mut ViEntry {
    if entries.is_empty() {
        entries.push(ViEntry {
            pos: String::new(),
            meanings: Vec::new(),
        });
    }
    entries.last_mut().expect("just pushed when empty")
}

/// The sense examples attach to, opening an unlabelled one if an example
/// arrives before any meaning has.
fn last_meaning(entries: &mut Vec<ViEntry>) -> &mut ViMeaning {
    let entry = last_entry(entries);
    if entry.meanings.is_empty() {
        entry.meanings.push(ViMeaning {
            text: String::new(),
            examples: Vec::new(),
        });
    }
    entry.meanings.last_mut().expect("just pushed when empty")
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

fn attribute<'a>(open_tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = [name, "=\""].concat();
    let mut from = 0usize;
    while let Some(hit) = open_tag[from..].find(&needle) {
        let at = from + hit;
        // `id="` also appears inside `data-id="`, which would hand back the
        // wrong attribute's value; a real one starts at a word boundary.
        let boundary = at == 0
            || open_tag[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let start = at + needle.len();
        if boundary {
            let rest = &open_tag[start..];
            let end = rest.find('"')?;
            return Some(&rest[..end]);
        }
        from = start;
    }
    None
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
    let mut rest = html;
    loop {
        // Only a '<' that actually starts a tag opens one. A bare comparison
        // ("a < b") would otherwise swallow the rest of the sentence, and the
        // matching '>' would vanish from the output.
        let Some(open) = rest.find('<') else {
            push_flattened(&mut text, rest);
            break;
        };
        let after = &rest[open + 1..];
        let is_tag = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        if !is_tag {
            push_flattened(&mut text, &rest[..open + 1]);
            rest = after;
            continue;
        }
        push_flattened(&mut text, &rest[..open]);
        match after.find('>') {
            Some(close) => rest = &after[close + 1..],
            // An unterminated tag runs to the end of the input.
            None => break,
        }
    }
    decode_entities(&text)
}

/// Append text with its whitespace runs collapsed to single spaces.
fn push_flattened(text: &mut String, chunk: &str) {
    for character in chunk.chars() {
        if character.is_whitespace() {
            if !text.ends_with(' ') {
                text.push(' ');
            }
        } else {
            text.push(character);
        }
    }
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

/// One agent for the whole process. It owns the connection pool, so the three
/// requests a lookup makes — and the two `clients5` calls a Vietnamese
/// paragraph makes back to back — reuse connections instead of paying for a
/// fresh TCP and TLS handshake every time. Cloning is cheap; it is an `Arc`.
fn agent() -> ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::builder()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
        })
        .clone()
}

fn http_get(url: &str) -> Result<String, String> {
    match agent().get(url).call() {
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
    let response = agent()
        .get(url)
        .call()
        .map_err(|error| format!("Could not fetch the audio: {error}"))?;
    let mut bytes = Vec::new();
    // One byte past the cap, so an oversized reply is detected rather than
    // silently truncated into a file that would be cached and replayed forever.
    response
        .into_reader()
        .take(MAX_AUDIO_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read the audio: {error}"))?;
    if bytes.is_empty() {
        return Err("The audio was empty".into());
    }
    if bytes.len() as u64 > MAX_AUDIO_BYTES {
        return Err("The audio was too large".into());
    }
    let Some(parent) = path.parent() else {
        return Err("The cache path has no parent".into());
    };
    fs::create_dir_all(parent).map_err(|error| format!("Could not create the cache: {error}"))?;
    // Write beside the target and rename, so a clip interrupted mid-download
    // never leaves a truncated file that later plays as silence. The temporary
    // name is unique per attempt: two threads fetching the same clip would
    // otherwise truncate each other's file and rename half of one into place.
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("part{}-{attempt}", std::process::id()));
    fs::write(&temporary, &bytes).map_err(|error| format!("Could not save the audio: {error}"))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not store the audio: {error}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        cached_audio_path, cambridge_headword_matches, em_to_pango_bold, escape_markup, fnv1a,
        is_sentence, parse_cambridge,
        parse_google, parse_tracau, parse_tracau_fulltext, percent_encode, strip_tags,
    };

    #[test]
    fn a_single_word_is_not_a_sentence() {
        assert!(!is_sentence("inefficient"));
        assert!(!is_sentence("  kick the bucket "));
        assert!(!is_sentence(""));
        assert!(!is_sentence("   "));
        assert!(!is_sentence("!?"));
    }

    #[test]
    fn punctuation_length_and_accents_all_mark_prose() {
        assert!(is_sentence("Hello there, how are you?"));
        assert!(is_sentence("one two three four five"));
        assert!(is_sentence("xin chào bạn"));
    }

    #[test]
    fn punctuation_around_a_word_does_not_make_it_prose() {
        // Typed in a hurry, or copied with the sentence's full stop attached.
        assert!(!is_sentence("hello."));
        assert!(!is_sentence("\"serendipity\""));
        assert!(!is_sentence("(inefficient)"));
        // An apostrophe is part of the word, not a clause break.
        assert!(!is_sentence("don't"));
    }

    #[test]
    fn a_borrowed_accented_headword_still_goes_to_the_dictionary() {
        // English has these as headwords; only a run of accented words reads as
        // a foreign phrase.
        assert!(!is_sentence("café"));
        assert!(!is_sentence("naïve"));
        assert!(is_sentence("cà phê sữa"));
    }

    #[test]
    fn the_word_count_boundary_is_four() {
        assert!(!is_sentence("one two three four"));
        assert!(is_sentence("one two three four five"));
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
        let wording: Vec<_> = entries[0].meanings.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(wording, ["thiếu khả năng, bất tài", "không có hiệu quả"]);
        assert_eq!(entries[1].pos, "danh từ");
        assert_eq!(entries[1].meanings[0].text, "sự kém cỏi");
    }

    #[test]
    fn an_idiom_block_is_read_from_its_tn_rows() {
        // "in spite of" carries no `mn` row at all: its wording is in `tn_n`
        // and its examples in `tn_mh` / `tn_mh_n`. Matching only `tl` and `mn`
        // dropped the entry whole, which is why multi-word lookups came back
        // blank even though tracau had them.
        let html = r##"<table id="definition">
            <tr id="tl"><td id="C_C"><b>thành ngữ spite</b></td></tr>
            <tr id="tn"><td id="C_C">in spite of</td></tr>
            <tr id="tn_n"><td id="C_C">mặc dù; bất chấp</td></tr>
            <tr id="tn_mh"><td id="C_C">they went out in spite of the rain</td></tr>
            <tr id="tn_mh_n"><td id="C_C">họ ra đi bất chấp trời mưa</td></tr>
            <tr id="tn_mh"><td id="C_C">in spite of all his efforts, he failed</td></tr>
            <tr id="tn_mh_n"><td id="C_C">dù hết sức cố gắng, nó vẫn thi trượt</td></tr>
        </table>"##;
        let entries = parse_tracau_fulltext(html);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pos, "thành ngữ spite");
        let meaning = &entries[0].meanings[0];
        assert_eq!(meaning.text, "mặc dù; bất chấp");
        // Both examples hang off that one sense, each with its translation.
        assert_eq!(meaning.examples.len(), 2);
        assert_eq!(meaning.examples[0].en, "they went out in spite of the rain");
        assert_eq!(meaning.examples[0].vi.as_deref(), Some("họ ra đi bất chấp trời mưa"));
        assert_eq!(meaning.examples[1].en, "in spite of all his efforts, he failed");
    }

    #[test]
    fn a_saying_block_suffixed_with_ss_is_read_like_any_other() {
        // "kick the bucket" comes back entirely in the _ss variants.
        let html = r##"<table id="definition">
            <tr id="pa_ss"><td id="C_C">[kick the bucket]</td></tr>
            <tr id="tl_ss"><td id="C_C"><b>saying &amp;&amp; slang</b></td></tr>
            <tr id="mn_ss"><td id="C_C">die, buy the farm, pass away</td></tr>
            <tr id="mh_ss"><td id="C_C">Charlie finally kicked the bucket.</td></tr>
        </table>"##;
        let entries = parse_tracau_fulltext(html);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pos, "saying && slang");
        let meaning = &entries[0].meanings[0];
        assert_eq!(meaning.text, "die, buy the farm, pass away");
        assert_eq!(meaning.examples[0].en, "Charlie finally kicked the bucket.");
        // No `_n` row followed it, so there is nothing to translate it by.
        assert!(meaning.examples[0].vi.is_none());
    }

    #[test]
    fn an_example_translation_with_no_example_above_it_is_dropped() {
        let html = r##"<table id="definition">
            <tr id="tl"><td id="C_C">danh từ</td></tr>
            <tr id="mn"><td id="C_C">nghĩa</td></tr>
            <tr id="mh_n"><td id="C_C">mồ côi</td></tr>
        </table>"##;
        let entries = parse_tracau_fulltext(html);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].meanings[0].examples.is_empty());
    }

    #[test]
    fn a_part_of_speech_with_no_meanings_is_dropped() {
        assert!(parse_tracau_fulltext(r#"<tr id="tl"><td>tính từ</td></tr>"#).is_empty());
        // A phonetic row alone is not a definition either.
        assert!(parse_tracau_fulltext(r#"<tr id="pa_ss"><td id="C_C">[x]</td></tr>"#).is_empty());
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
            "word": "inefficient",
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
        let (pronunciations, definitions) = parse_cambridge(&json, "inefficient");
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
    fn a_reply_about_a_different_word_is_discarded() {
        // The endpoint resolves by search, so a phrase it does not carry comes
        // back as whatever it found first rather than as a 404. Rendering it
        // would show the definitions of another word entirely.
        let json = serde_json::json!({
            "word": "look",
            "definition": [{"pos": "verb", "text": "to direct your eyes", "example": []}]
        });
        let (_, definitions) = parse_cambridge(&json, "look after");
        assert!(definitions.is_empty());
    }

    #[test]
    fn a_headword_is_matched_past_inflection_but_not_past_truncation() {
        // Lemma resolution is the right entry and is kept.
        assert!(cambridge_headword_matches("cats", "cat"));
        assert!(cambridge_headword_matches("happier", "happy"));
        assert!(cambridge_headword_matches("Inefficient", "inefficient"));
        assert!(cambridge_headword_matches("hard  work", "hard work"));
        // Every one of these was observed answering the wrong entry.
        assert!(!cambridge_headword_matches("look after", "look"));
        assert!(!cambridge_headword_matches("give up", "give someone a heads-up"));
        assert!(!cambridge_headword_matches("of course", "course of action"));
        assert!(!cambridge_headword_matches("ice cream", "ice cream cone"));
        // A single word must not resolve to something unrelated either.
        assert!(!cambridge_headword_matches("quantum", "zebra"));
        assert!(!cambridge_headword_matches("run", ""));
    }

    #[test]
    fn an_unknown_word_parses_to_nothing() {
        let json = serde_json::json!({"error": "word not found"});
        let (pronunciations, definitions) = parse_cambridge(&json, "whatever");
        assert!(pronunciations.is_empty());
        assert!(definitions.is_empty());
    }

    #[test]
    fn comparison_operators_survive_tag_stripping() {
        // A bare "<" is not a tag, and used to swallow the rest of the line.
        assert_eq!(strip_tags("a < b và c"), "a < b và c");
        assert_eq!(strip_tags("x > y"), "x > y");
        assert_eq!(strip_tags("5 < 6 <b>times</b>"), "5 < 6 times");
        // An unterminated tag simply ends the text.
        assert_eq!(strip_tags("keep<td id="), "keep");
    }

    #[test]
    fn a_row_id_is_not_matched_inside_another_attribute() {
        let html = r#"<tr data-id="xx" id="mn"><td id="C_C">nghĩa</td></tr>"#;
        let entries = parse_tracau_fulltext(html);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meanings[0].text, "nghĩa");
    }

    #[test]
    fn an_unclosed_em_still_yields_its_text() {
        assert_eq!(em_to_pango_bold("a <em>b"), "a b");
    }

    #[test]
    fn a_null_error_field_is_not_a_missing_word() {
        // `{"error": null}` is a success; only a real value means not found.
        let json = serde_json::json!({
            "word": "cat",
            "error": null,
            "definition": [{"pos": "noun", "text": "a small animal", "example": []}]
        });
        let (_, definitions) = parse_cambridge(&json, "cat");
        assert_eq!(definitions.len(), 1);
    }

    #[test]
    fn one_odd_element_does_not_discard_the_whole_translation() {
        // The endpoint is unversioned; a trailing field must not lose the text.
        let json = serde_json::json!([["Xin chào.", "en"], null, ["Tạm biệt.", "en"]]);
        let (text, _) = parse_google(&json).expect("a translation");
        assert_eq!(text, "Xin chào. Tạm biệt.");
    }

    #[test]
    fn tracau_examples_survive_an_apostrophe_in_the_query() {
        // The markup has apostrophes escaped, so matching against it directly
        // would throw away examples that genuinely contain the query.
        let json = serde_json::json!({
            "tratu": [],
            "sentences": [{"fields": {"en": "I <em>don't</em> know.", "vi": "Tôi không biết."}}]
        });
        let (entries, examples) = parse_tracau(&json, "don't");
        assert!(entries.is_empty());
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].en_markup, "I <b>don&apos;t</b> know.");
    }

    #[test]
    fn examples_for_a_word_tracau_does_not_know_are_dropped() {
        let json = serde_json::json!({
            "tratu": [],
            "sentences": [{"fields": {"en": "Something else.", "vi": "Chuyện khác."}}]
        });
        let (_, examples) = parse_tracau(&json, "zzzz");
        assert!(examples.is_empty());
    }

    #[test]
    fn the_hash_behind_the_audio_cache_matches_the_reference_vectors() {
        // Pinned against the published FNV-1a 64-bit vectors: a typo in the
        // offset basis or the prime would silently invalidate every cached clip
        // and re-download the lot.
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn an_audio_url_always_maps_to_the_same_cache_file() {
        let first = cached_audio_path("https://example.test/uk/word.mp3");
        assert_eq!(first, cached_audio_path("https://example.test/uk/word.mp3"));
        assert_ne!(first, cached_audio_path("https://example.test/us/word.mp3"));
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("mp3"));
    }
}
