//! YOUTUBE: a link or a search played by mpv inside a card. yt-dlp finds the
//! streams; mpv decodes them and draws into the card's own X window (`--wid`),
//! so Sysi holds no decoder, only the process and its IPC socket.
//!
//! googlevideo stalls a request for more than a couple of MB, or for the rest
//! of a file, and the FFmpeg mpv is built on (8.0) only asks that way. So mpv
//! reads the streams from a small proxy on 127.0.0.1 here, which fetches them
//! 2 MB at a time and asks again for a piece that stalls.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

/// At most `height` lines, the card's quality setting. Plain https streams
/// first, the ones the proxy can fetch in pieces; failing those HLS, which is
/// all a live stream has, and whose two-second pieces googlevideo serves to
/// mpv as they are.
fn format(height: u32) -> String {
    format!(
        "bv*[height<=?{height}][protocol=https]+ba[protocol=https]/b[protocol=https]\
         /bv*[height<=?{height}][protocol^=m3u8]+ba[protocol^=m3u8]/b[protocol^=m3u8]"
    )
}
const CHUNK: u64 = 2 << 20;
/// Tries per piece. A stalled one usually comes through on the next.
const ATTEMPTS: usize = 6;

/// What yt-dlp found for a link: separate video and audio, or one stream
/// holding both.
#[derive(Clone)]
pub struct Streams {
    pub title: String,
    video: String,
    audio: Option<String>,
    /// HLS: mpv reads it straight from googlevideo, not through the proxy.
    hls: bool,
    /// Live: there is no place in it to come back to.
    pub live: bool,
    /// YouTube's thumbnails along the video, for the seek bar to preview.
    pub storyboard: Option<Storyboard>,
    pub subtitles: Vec<Subtitle>,
}

/// A subtitle track YouTube has for the video, as WebVTT mpv can fetch.
#[derive(Clone, Debug, PartialEq)]
pub struct Subtitle {
    pub label: String,
    pub url: String,
}

/// The tracks worth offering: every one written for the video, the one
/// YouTube made from its own speech, and that one machine-translated to
/// English when the speech is not English already. The other hundred machine
/// translations are left out.
/// A language's WebVTT track: its link and the name YouTube gives it.
fn vtt(list: &serde_json::Value) -> Option<(&str, Option<&str>)> {
    let track = list
        .as_array()?
        .iter()
        .find(|track| track["ext"] == "vtt")?;
    Some((track["url"].as_str()?, track["name"].as_str()))
}

fn parse_subtitles(info: &serde_json::Value) -> Vec<Subtitle> {
    let mut written: Vec<Subtitle> = info["subtitles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(language, _)| *language != "live_chat")
        .filter_map(|(language, list)| {
            let (url, name) = vtt(list)?;
            Some(Subtitle {
                label: name.unwrap_or(language).to_string(),
                url: url.to_string(),
            })
        })
        .collect();
    written.sort_by(|a, b| a.label.cmp(&b.label));
    let made = info["automatic_captions"].as_object();
    let english_already = made.is_some_and(|made| made.contains_key("en-orig"));
    let mut automatic: Vec<Subtitle> = made
        .into_iter()
        .flatten()
        .filter(|(language, _)| {
            language.ends_with("-orig") || (*language == "en" && !english_already)
        })
        .filter_map(|(language, list)| {
            let (url, name) = vtt(list)?;
            let name = name.unwrap_or(language);
            Some(Subtitle {
                label: format!("{name} (auto)"),
                url: url.to_string(),
            })
        })
        .collect();
    automatic.sort_by(|a, b| a.label.cmp(&b.label));
    written.extend(automatic);
    written
}

/// YouTube's storyboard: sheets of thumbnails, `columns` x `rows` to a sheet,
/// one every `seconds_per_tile`, each sheet covering the stretch of the video
/// its duration says.
#[derive(Clone, Debug)]
pub struct Storyboard {
    pub width: i32,
    pub height: i32,
    columns: i32,
    rows: i32,
    seconds_per_tile: f64,
    sheets: Vec<(String, f64)>,
}

impl Storyboard {
    /// The sheet holding the thumbnail for `position`, and where in that
    /// sheet it sits.
    pub fn locate(&self, position: f64) -> Option<(usize, i32, i32)> {
        let mut start = 0.0;
        for (index, (_, duration)) in self.sheets.iter().enumerate() {
            if position < start + duration || index + 1 == self.sheets.len() {
                let tile = ((position - start).max(0.0) / self.seconds_per_tile) as i32;
                let tile = tile.min(self.columns * self.rows - 1);
                return Some((
                    index,
                    tile % self.columns * self.width,
                    tile / self.columns * self.height,
                ));
            }
            start += duration;
        }
        None
    }

    pub fn sheet_url(&self, index: usize) -> Option<&str> {
        self.sheets.get(index).map(|(url, _)| url.as_str())
    }
}

/// The largest storyboard no wider than `max_width`: 320 is what a 160-wide
/// preview needs at 200%.
fn parse_storyboard(info: &serde_json::Value, max_width: i64) -> Option<Storyboard> {
    let format = info["formats"]
        .as_array()?
        .iter()
        .filter(|format| {
            format["format_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("sb"))
                && format["width"]
                    .as_i64()
                    .is_some_and(|width| width <= max_width)
        })
        .max_by_key(|format| format["width"].as_i64())?;
    let sheets: Vec<(String, f64)> = format["fragments"]
        .as_array()?
        .iter()
        .filter_map(|sheet| {
            Some((
                sheet["url"].as_str()?.to_string(),
                sheet["duration"].as_f64()?,
            ))
        })
        .collect();
    let fps = format["fps"].as_f64().filter(|fps| *fps > 0.0)?;
    Some(Storyboard {
        width: format["width"].as_i64()? as i32,
        height: format["height"].as_i64()? as i32,
        columns: format["columns"].as_i64()? as i32,
        rows: format["rows"].as_i64()? as i32,
        seconds_per_tile: 1.0 / fps,
        sheets: (!sheets.is_empty()).then_some(sheets)?,
    })
}

/// One storyboard sheet's image. Blocks.
pub fn fetch_sheet(url: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    AGENT
        .get(url)
        .call()
        .ok()?
        .into_reader()
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// What yt-dlp should look up for what was typed: a link as it is, anything
/// else the first YouTube result for it.
fn target(input: &str) -> String {
    let input = input.trim();
    if input.contains("://") {
        input.to_string()
    } else {
        format!("ytsearch1:{input}")
    }
}

/// The last lookup and the quality it was for, so reopening the card skips
/// yt-dlp while its links last.
static LAST: Mutex<Option<(String, u32, Streams)>> = Mutex::new(None);

/// When a googlevideo link stops working, from its `expire` parameter.
fn expires(url: &str) -> u64 {
    url.split(['?', '&', '/'])
        .skip_while(|part| !part.starts_with("expire"))
        .find_map(|part| part.trim_start_matches("expire=").parse().ok())
        .unwrap_or(0)
}

/// Ask yt-dlp for the streams `input` plays. Blocks for a few seconds, unless
/// the same input was looked up recently.
pub fn resolve(input: &str, height: u32) -> Result<Streams, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_secs());
    if let Some((_, _, streams)) = LAST.lock().unwrap().as_ref().filter(|(last, at, streams)| {
        last == input && *at == height && expires(&streams.video) > now + 600
    }) {
        return Ok(streams.clone());
    }
    let streams = lookup(input, height)?;
    *LAST.lock().unwrap() = Some((input.to_string(), height, streams.clone()));
    Ok(streams)
}

/// Browsers yt-dlp can sign in with, in the order tried. Brave first: it holds
/// the user's Premium account, where Chrome's many profiles leave yt-dlp to
/// take whichever was used last.
const BROWSERS: [&str; 5] = ["brave", "chrome", "firefox", "chromium", "edge"];
/// What got past YouTube last, asked first from then on: a browser, or
/// `Some(None)` for signed out. `None` until a lookup has worked.
static SIGNED_IN: Mutex<Option<Option<&str>>> = Mutex::new(None);

/// Whether a failed lookup could go better signed in some other way. A video
/// that is gone, or a search with nothing in it, fails the same for everyone
/// and is not worth five more runs of yt-dlp.
fn about_signing_in(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    [
        "not a bot",
        "sign in",
        "cookie",
        "keyring",
        "could not find",
        "refused",
    ]
    .iter()
    .any(|hint| reason.contains(hint))
}

/// yt-dlp, signed in through a browser's cookies first: YouTube hands an
/// address it has seen ask for many videos either "Sign in to confirm you're
/// not a bot" or, quietly, links that answer 403, and signed in (Premium, for
/// this user) it does neither. The browser that worked is asked first after;
/// with none that works, the lookup goes out signed out. A lookup counts only
/// once its picture answers.
fn lookup(input: &str, height: u32) -> Result<Streams, String> {
    let known = *SIGNED_IN.lock().unwrap();
    let ways = BROWSERS.into_iter().map(Some).chain([None]);
    let ways = known
        .into_iter()
        .chain(ways.filter(|way| Some(*way) != known));
    let mut reason = String::from("Nothing found");
    for way in ways {
        match run_yt_dlp(input, height, way) {
            Ok(streams) if answers(&streams) => {
                *SIGNED_IN.lock().unwrap() = Some(way);
                return Ok(streams);
            }
            Ok(_) => reason = "YouTube refused the stream".to_string(),
            Err(why) if about_signing_in(&why) => reason = why,
            Err(why) => return Err(why),
        }
    }
    Err(reason)
}

/// Drop the remembered lookup: its links stopped working, so the next play
/// asks yt-dlp afresh.
pub fn forget() {
    *LAST.lock().unwrap() = None;
}

/// Whether googlevideo serves the picture's first byte. HLS is played by mpv
/// straight from YouTube and has no single file to ask.
fn answers(streams: &Streams) -> bool {
    streams.hls
        || AGENT
            .get(&streams.video)
            .set("Range", "bytes=0-0")
            .call()
            .is_ok()
}

fn run_yt_dlp(input: &str, height: u32, browser: Option<&str>) -> Result<Streams, String> {
    let output = Command::new("yt-dlp")
        .args(["-j", "--no-playlist", "--playlist-items", "1"])
        .args(["--no-warnings", "-f", &format(height)])
        .args(browser.map(|browser| format!("--cookies-from-browser={browser}")))
        // What was typed is a link or words, never an option: a box holding
        // "--exec=…://" must not hand yt-dlp a command to run.
        .arg("--")
        .arg(target(input))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => "yt-dlp is not installed".to_string(),
            _ => format!("Could not run yt-dlp: {error}"),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(info) = stdout
        .lines()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().rev().find(|line| !line.trim().is_empty());
        return Err(reason.map_or("Nothing found".to_string(), short_reason));
    };
    parse_streams(&info).ok_or_else(|| "No stream to play".to_string())
}

/// yt-dlp's complaint without its prefixes and advice: `ERROR: [youtube]
/// id: Requested format is not available. Use --list-formats …` reads as
/// `Requested format is not available`.
fn short_reason(line: &str) -> String {
    let line = line.trim().trim_start_matches("ERROR: ");
    let line = match line.split_once("] ") {
        Some((_, rest)) => rest.split_once(": ").map_or(rest, |(_, reason)| reason),
        None => line,
    };
    line.split(". ")
        .next()
        .unwrap_or(line)
        .trim_end_matches('.')
        .to_string()
}

fn parse_streams(info: &serde_json::Value) -> Option<Streams> {
    let formats = match info["requested_formats"].as_array() {
        Some(formats) => formats.iter().collect(),
        None => vec![info],
    };
    let hls = formats.iter().any(|format| {
        format["protocol"]
            .as_str()
            .is_some_and(|protocol| protocol.starts_with("m3u8"))
    });
    let mut urls: Vec<String> = match info["requested_formats"].as_array() {
        Some(formats) => formats
            .iter()
            .filter_map(|format| format["url"].as_str().map(str::to_string))
            .collect(),
        None => info["url"]
            .as_str()
            .map(str::to_string)
            .into_iter()
            .collect(),
    };
    let audio = (urls.len() > 1).then(|| urls.remove(1));
    Some(Streams {
        title: info["title"].as_str().unwrap_or_default().to_string(),
        video: urls.into_iter().next()?,
        audio,
        hls,
        live: info["is_live"].as_bool().unwrap_or(false),
        storyboard: parse_storyboard(info, 320),
        subtitles: parse_subtitles(info),
    })
}

/// In the runtime dir: a socket path must fit in 108 bytes. One per mpv, so
/// nothing reaches one that is on its way out.
fn socket(generation: u64) -> PathBuf {
    crate::ui::sysi_runtime_dir().join(format!("youtube-mpv-{generation}.sock"))
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::builder()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(4))
        .build()
});

/// Where mpv reads the streams: `/<token>/video` and `/<token>/audio` on a
/// port of its own. Any program on the machine can reach 127.0.0.1; the token,
/// new each time Sysi starts, keeps the streams to the mpv told it.
struct Proxy {
    port: u16,
    token: u64,
    routes: Arc<Mutex<HashMap<String, Route>>>,
}

/// A stream the proxy serves: where it really is, and its length once known,
/// so a seek does not ask googlevideo for it again.
struct Route {
    url: String,
    total: Option<u64>,
}

impl Proxy {
    fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let routes = Arc::new(Mutex::new(HashMap::new()));
        thread::spawn({
            let routes = routes.clone();
            move || {
                for client in listener.incoming().map_while(Result::ok) {
                    let routes = routes.clone();
                    // An error here is mpv hanging up to seek, or a piece that
                    // never came: mpv sees the stream end either way.
                    thread::spawn(move || serve(client, &routes));
                }
            }
        });
        // Random per process: std's hasher keys are seeded from the OS.
        let token = {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        Ok(Self {
            port,
            token,
            routes,
        })
    }

    /// The route for `name`, as mpv asks for it and as it is looked up.
    fn path(&self, name: &str) -> String {
        format!("/{:016x}/{name}", self.token)
    }

    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path(name))
    }
}

fn serve(mut client: TcpStream, routes: &Mutex<HashMap<String, Route>>) -> io::Result<()> {
    // A client that connects and says nothing gives up its thread.
    client.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let path = request.split_whitespace().nth(1).unwrap_or_default();
    let mut range = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        let header = header.to_ascii_lowercase();
        if let Some(bytes) = header.strip_prefix("range: bytes=") {
            range = bytes
                .trim()
                .split_once('-')
                .map(|(from, to)| (from.parse::<u64>().ok(), to.parse::<u64>().ok()));
        }
    }
    let known = routes
        .lock()
        .unwrap()
        .get(path)
        .map(|route| (route.url.clone(), route.total));
    let Some((upstream, total)) = known else {
        return client.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    };
    let total = match total {
        Some(total) => total,
        None => {
            let total = size(&upstream)?;
            if let Some(route) = routes.lock().unwrap().get_mut(path) {
                route.total = Some(total);
            }
            total
        }
    };
    let (from, to) = match range {
        // "bytes=-N": the last N.
        Some((None, Some(last))) => (total.saturating_sub(last), total.saturating_sub(1)),
        Some((from, to)) => (from.unwrap_or(0), to.unwrap_or(u64::MAX)),
        None => (0, u64::MAX),
    };
    let to = to.min(total.saturating_sub(1));
    if total == 0 || from > to {
        let head = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\n\r\n");
        return client.write_all(head.as_bytes());
    }
    let status = match range {
        Some(_) => format!("206 Partial Content\r\nContent-Range: bytes {from}-{to}/{total}"),
        None => "200 OK".to_string(),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        to - from + 1
    );
    // The first piece before the header: a stream that cannot be had is then
    // a 502 mpv reports, not a body cut short that it takes for the end.
    let mut first = Vec::new();
    let Ok(mut at) = fetch(&upstream, from, (from + CHUNK - 1).min(to), &mut first) else {
        return client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
    };
    client.write_all(head.as_bytes())?;
    client.write_all(&first)?;
    while at <= to {
        at = fetch(&upstream, at, (at + CHUNK - 1).min(to), &mut client)?;
    }
    Ok(())
}

/// A refusal no retry changes: the link expired or is not this address's.
/// The card looks the video up again instead.
fn refused(error: &ureq::Error) -> bool {
    matches!(error, ureq::Error::Status(403 | 404 | 410, _))
}

/// The stream's length, from the `Content-Range` of its first byte.
fn size(url: &str) -> io::Result<u64> {
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(125 << attempt.min(4)));
        }
        let response = match AGENT.get(url).set("Range", "bytes=0-0").call() {
            Ok(response) => response,
            Err(error) if refused(&error) => return Err(io::Error::other("googlevideo refused")),
            Err(_) => continue,
        };
        if let Some(total) = response
            .header("Content-Range")
            .and_then(|range| range.rsplit('/').next())
            .and_then(|total| total.parse().ok())
        {
            return Ok(total);
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "no stream length"))
}

/// Copy bytes `from..=to` to `out`, asking again from where a stalled request
/// stopped. Returns the offset after the last byte written.
fn fetch(url: &str, from: u64, to: u64, out: &mut impl Write) -> io::Result<u64> {
    let mut at = from;
    let mut buffer = vec![0; 64 << 10];
    for attempt in 0..ATTEMPTS {
        // A breath between tries, longer each time: a blip in the network
        // otherwise burns all of them in a few milliseconds.
        if attempt > 0 {
            thread::sleep(Duration::from_millis(125 << attempt.min(4)));
        }
        let response = match AGENT
            .get(url)
            .set("Range", &format!("bytes={at}-{to}"))
            .call()
        {
            Ok(response) => response,
            Err(error) if refused(&error) => return Err(io::Error::other("googlevideo refused")),
            Err(_) => continue,
        };
        let mut body = response.into_reader();
        while let Ok(read) = body.read(&mut buffer) {
            if read == 0 {
                break;
            }
            out.write_all(&buffer[..read])?;
            at += read as u64;
        }
        if at > to {
            return Ok(at);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "googlevideo stalled",
    ))
}

/// What the card's controls show, as mpv reports it.
pub enum Update {
    Position(f64),
    Duration(f64),
    Paused(bool),
    Muted(bool),
    /// 0 to 100.
    Volume(f64),
    /// How far ahead is downloaded, as a position in seconds.
    Buffered(f64),
    /// The picture's width over its height.
    Aspect(f64),
    /// A right-click on the picture, which mpv's window took: the card's menu
    /// is wanted.
    Menu,
    /// Whether the video starts over at its end.
    Looping(bool),
    /// Played to its end, and left on its last frame.
    Ended,
    /// mpv quit, or never came up.
    Gone,
}

/// Follow mpv's playback until it quits, handing each change to the card.
/// Updates carry the generation of the mpv they are about, so one from an mpv
/// already replaced is dropped.
fn watch(socket: &Path, generation: u64, updates: &async_channel::Sender<(u64, Update)>) {
    let stream = (0..150).find_map(|_| {
        UnixStream::connect(socket).ok().or_else(|| {
            thread::sleep(Duration::from_millis(100));
            None
        })
    });
    let Some(stream) = stream else {
        let _ = updates.send_blocking((generation, Update::Gone));
        return;
    };
    let Ok(mut ask) = stream.try_clone() else {
        let _ = updates.send_blocking((generation, Update::Gone));
        return;
    };
    let mut setup: Vec<serde_json::Value> = [
        "duration",
        "pause",
        "mute",
        "volume",
        "video-params/aspect",
        "loop-file",
        "eof-reached",
    ]
    .iter()
    .enumerate()
    .map(|(id, name)| serde_json::json!(["observe_property", id + 1, name]))
    .collect();
    // The video takes clicks and the wheel itself, mpv's window being on top.
    for (key, command) in [
        ("MBTN_LEFT", "cycle pause"),
        ("WHEEL_UP", "seek 5"),
        ("WHEEL_DOWN", "seek -5"),
        ("MBTN_RIGHT", "script-message sysi-menu"),
    ] {
        setup.push(serde_json::json!(["keybind", key, command]));
    }
    for command in setup {
        let _ = writeln!(ask, "{}", serde_json::json!({ "command": command }));
    }
    // Where it is and how far is downloaded are asked four times a second
    // while it plays, not observed: mpv reports both on every frame and every
    // packet, which woke this thread some fifty times a second for a bar that
    // shows a quarter second. Paused, they are asked only after a seek.
    const POSITION: u64 = 1;
    const BUFFERED: u64 = 2;
    let poll = |ask: &mut UnixStream| {
        for (id, name) in [(POSITION, "time-pos"), (BUFFERED, "demuxer-cache-time")] {
            let request =
                serde_json::json!({ "command": ["get_property", name], "request_id": id });
            let _ = writeln!(ask, "{request}");
        }
    };
    let mut reader = BufReader::new(stream);
    // Bytes, not a String: read_line drops what it read when a timeout lands
    // inside a multi-byte character.
    let mut line = Vec::new();
    let mut playing = false;
    let mut polled = std::time::Instant::now();
    let mut buffered_second = i64::MIN;
    loop {
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            // Not a whole line yet: what came stays in `line` for the next read.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
            Ok(_) => {}
        }
        if playing && polled.elapsed() >= Duration::from_millis(250) {
            polled = std::time::Instant::now();
            poll(&mut ask);
        }
        if line.last() != Some(&b'\n') {
            continue;
        }
        let event = serde_json::from_slice::<serde_json::Value>(&line);
        line.clear();
        let Ok(event) = event else {
            continue;
        };
        if event["event"] == "client-message" && event["args"][0] == "sysi-menu" {
            if updates.send_blocking((generation, Update::Menu)).is_err() {
                return;
            }
            continue;
        }
        if event["event"] == "playback-restart" {
            poll(&mut ask);
            continue;
        }
        let data = &event["data"];
        let update = match event["request_id"].as_u64() {
            Some(POSITION) => match data.as_f64() {
                Some(position) => Update::Position(position),
                None => continue,
            },
            Some(BUFFERED) => {
                let Some(ahead) = data.as_f64() else {
                    continue;
                };
                if ahead as i64 == buffered_second {
                    continue;
                }
                buffered_second = ahead as i64;
                Update::Buffered(ahead)
            }
            _ if event["event"] != "property-change" => continue,
            _ => match event["name"].as_str() {
                Some("duration") => Update::Duration(data.as_f64().unwrap_or(0.0)),
                Some("pause") => {
                    playing = data == false;
                    // Woken to poll only while it plays; paused, the thread
                    // sleeps until mpv has something to say.
                    let timeout = playing.then_some(Duration::from_millis(250));
                    let _ = reader.get_ref().set_read_timeout(timeout);
                    poll(&mut ask);
                    Update::Paused(!playing)
                }
                Some("mute") => Update::Muted(data.as_bool().unwrap_or(false)),
                Some("volume") => Update::Volume(data.as_f64().unwrap_or(100.0)),
                Some("eof-reached") if data == true => Update::Ended,
                // "inf", a count, or false.
                Some("loop-file") => Update::Looping(data != false && data != "no" && data != 0),
                Some("video-params/aspect") => {
                    let Some(aspect) = data.as_f64() else {
                        continue;
                    };
                    Update::Aspect(aspect)
                }
                _ => continue,
            },
        };
        if updates.send_blocking((generation, update)).is_err() {
            return;
        }
    }
    let _ = updates.send_blocking((generation, Update::Gone));
}

pub struct Player {
    child: Option<Child>,
    proxy: Option<Proxy>,
    generation: u64,
    updates: async_channel::Sender<(u64, Update)>,
    /// One connection to the current mpv for the card's commands, rather
    /// than a new one for each (a volume drag sends dozens).
    ipc: std::cell::RefCell<Option<UnixStream>>,
}

impl Player {
    pub fn new(updates: async_channel::Sender<(u64, Update)>) -> Self {
        Self {
            child: None,
            proxy: None,
            generation: 0,
            updates,
            ipc: std::cell::RefCell::new(None),
        }
    }

    /// Which mpv is current; updates from any other are stale.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Send mpv a command. False only when there is no mpv: one still
    /// starting, its socket not up yet, drops the command rather than have
    /// the card start another.
    pub fn command(&self, command: serde_json::Value) -> bool {
        if self.child.is_none() {
            return false;
        }
        let line = format!("{}\n", serde_json::json!({ "command": command }));
        let mut ipc = self.ipc.borrow_mut();
        // Once on the open connection, once more on a fresh one.
        for _ in 0..2 {
            if ipc.is_none() {
                *ipc = UnixStream::connect(socket(self.generation))
                    .and_then(|stream| {
                        stream.set_write_timeout(Some(Duration::from_millis(200)))?;
                        Ok(stream)
                    })
                    .ok();
            }
            let Some(stream) = ipc.as_mut() else {
                return true;
            };
            if stream.write_all(line.as_bytes()).is_ok() {
                return true;
            }
            *ipc = None;
        }
        true
    }

    /// Play `streams` in the X window `xid` from `start` seconds, at the
    /// volume left last time, in place of whatever played there before.
    pub fn play(
        &mut self,
        xid: u64,
        streams: &Streams,
        start: f64,
        paused: bool,
        (volume, muted): (f64, bool),
        looping: bool,
    ) -> io::Result<()> {
        self.stop();
        if self.proxy.is_none() {
            self.proxy = Some(Proxy::start()?);
        }
        let proxy = self.proxy.as_ref().unwrap();
        let (video, audio) = if streams.hls {
            (streams.video.clone(), streams.audio.clone())
        } else {
            let mut routes = proxy.routes.lock().unwrap();
            routes.clear();
            let route = |url: &String| Route {
                url: url.clone(),
                total: None,
            };
            routes.insert(proxy.path("video"), route(&streams.video));
            if let Some(audio) = &streams.audio {
                routes.insert(proxy.path("audio"), route(audio));
            }
            (
                proxy.url("video"),
                streams.audio.as_ref().map(|_| proxy.url("audio")),
            )
        };
        // A live stream joins at the live edge.
        let start = if streams.live { None } else { Some(start) };
        let _ = std::fs::create_dir_all(crate::ui::sysi_runtime_dir());
        self.generation += 1;
        let socket = socket(self.generation);
        let mut command = Command::new("mpv");
        command
            .arg(format!("--wid={xid}"))
            .arg(format!("--input-ipc-server={}", socket.display()))
            .arg(format!("--force-media-title={}", streams.title))
            .args(start.map(|start| format!("--start={start}")))
            .arg(format!("--volume={volume}"))
            // The card's level slider ends at 100, and so does the Up key.
            .arg("--volume-max=100")
            .arg(format!(
                "--loop-file={}",
                if looping { "inf" } else { "no" }
            ))
            .arg(format!("--mute={}", if muted { "yes" } else { "no" }))
            .args(audio.map(|audio| format!("--audio-file={audio}")))
            .args([
                "--ytdl=no",
                "--hwdec=auto-safe",
                "--keep-open=yes",
                "--no-terminal",
                // The card draws the controls; mpv only the picture.
                "--osc=no",
                "--osd-level=0",
                "--input-default-bindings=no",
                "--input-vo-keyboard=no",
            ])
            // A minute or so ahead is plenty to ride out a stalled piece;
            // mpv's default would hold the whole video in memory.
            .args(["--demuxer-max-bytes=32MiB", "--demuxer-max-back-bytes=8MiB"])
            .args(paused.then_some("--pause"))
            .arg("--")
            .arg(video)
            // On a Wayland desk mpv would pick Wayland, ignore --wid and open
            // a window of its own; through Xwayland it draws into the card.
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // A Sysi that dies without closing the card takes mpv with it, rather
        // than leaving it playing into a window that no longer exists.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        self.child = Some(command.spawn()?);
        let updates = self.updates.clone();
        let generation = self.generation;
        thread::spawn(move || watch(&socket, generation, &updates));
        Ok(())
    }

    /// Quit mpv. What it was still reporting is stale from here on.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let _ = std::fs::remove_file(socket(self.generation));
        self.generation += 1;
        *self.ipc.borrow_mut() = None;
        // Reaped off the main loop.
        thread::spawn(move || child.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_play_as_given_and_words_search() {
        assert_eq!(
            target(" https://youtu.be/aqz-KE-bpKQ "),
            "https://youtu.be/aqz-KE-bpKQ"
        );
        assert_eq!(target("lofi hip hop"), "ytsearch1:lofi hip hop");
    }

    #[test]
    fn storyboard_finds_the_sheet_and_tile_for_a_time() {
        let info = serde_json::json!({"formats": [
            {"format_id": "sb1", "width": 160, "height": 90, "rows": 5, "columns": 5, "fps": 0.1,
             "fragments": [{"url": "s1a", "duration": 250.0}, {"url": "s1b", "duration": 250.0}]},
            {"format_id": "sb0", "width": 320, "height": 180, "rows": 3, "columns": 3, "fps": 0.1,
             "fragments": [{"url": "a", "duration": 90.0}, {"url": "b", "duration": 90.0}]},
            {"format_id": "137", "width": 1920},
        ]});
        let board = parse_storyboard(&info, 320).unwrap();
        assert_eq!((board.width, board.height), (320, 180));
        // 10 s a tile, 3 to a row: 45 s is the fifth, row 1 column 1.
        assert_eq!(board.locate(45.0), Some((0, 320, 180)));
        // Into the second sheet, its first tile.
        assert_eq!(board.locate(91.0), Some((1, 0, 0)));
        // Past the end stays on the last sheet's last tile.
        assert_eq!(board.locate(10_000.0), Some((1, 640, 360)));
        assert_eq!(board.sheet_url(1), Some("b"));
        assert!(parse_storyboard(&info, 100).is_none());
    }

    #[test]
    fn subtitles_offer_written_tracks_then_the_original_and_english() {
        let info = serde_json::json!({
            "subtitles": {
                "en": [{"ext": "json3", "url": "j"}, {"ext": "vtt", "url": "en.vtt", "name": "English"}],
                "live_chat": [{"ext": "vtt", "url": "chat"}],
            },
            "automatic_captions": {
                "ja-orig": [{"ext": "vtt", "url": "orig.vtt", "name": "Japanese (Original)"}],
                "en": [{"ext": "vtt", "url": "en.vtt", "name": "English"}],
                "fr": [{"ext": "vtt", "url": "fr.vtt", "name": "French"}],
            },
        });
        let labels =
            |info| -> Vec<String> { parse_subtitles(info).into_iter().map(|s| s.label).collect() };
        assert_eq!(
            labels(&info),
            ["English", "English (auto)", "Japanese (Original) (auto)"]
        );
        // Spoken in English: its own track already is English.
        let english = serde_json::json!({"automatic_captions": {
            "en-orig": [{"ext": "vtt", "url": "o.vtt", "name": "English (Original)"}],
            "en": [{"ext": "vtt", "url": "e.vtt", "name": "English"}],
        }});
        assert_eq!(labels(&english), ["English (Original) (auto)"]);
    }

    #[test]
    fn only_sign_in_trouble_tries_the_next_browser() {
        assert!(about_signing_in("Sign in to confirm you’re not a bot"));
        assert!(about_signing_in("could not find brave cookies database"));
        assert!(!about_signing_in("Video unavailable"));
        assert!(!about_signing_in("yt-dlp is not installed"));
    }

    #[test]
    fn errors_read_without_prefix_or_advice() {
        assert_eq!(
            short_reason("ERROR: [youtube] tRsQsTMvPNg: Requested format is not available. Use --list-formats for a list of available formats"),
            "Requested format is not available"
        );
        assert_eq!(short_reason("Nothing here."), "Nothing here");
    }

    #[test]
    fn expiry_reads_from_query_or_path() {
        assert_eq!(
            expires("https://x/videoplayback?a=1&expire=1792786475&b=2"),
            1792786475
        );
        assert_eq!(
            expires("https://x/videoplayback/id/6a/expire/1792786832/ei/MG"),
            1792786832
        );
        assert_eq!(expires("https://x/file"), 0);
    }

    #[test]
    fn streams_come_as_a_pair_or_one() {
        let pair = serde_json::json!({
            "title": "t",
            "requested_formats": [{"url": "v"}, {"url": "a"}],
        });
        let pair = parse_streams(&pair).unwrap();
        assert_eq!(
            (pair.video.as_str(), pair.audio.as_deref()),
            ("v", Some("a"))
        );
        let one = parse_streams(&serde_json::json!({"title": "t", "url": "va"})).unwrap();
        assert_eq!((one.video.as_str(), one.audio), ("va", None));
    }

    /// The proxy hands a range through in pieces, byte for byte.
    #[test]
    fn proxy_serves_a_range_from_pieces() {
        let data: Vec<u8> = (0..(CHUNK as usize * 2 + 77))
            .map(|i| (i % 251) as u8)
            .collect();
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_url = format!("http://{}/file", origin.local_addr().unwrap());
        thread::spawn({
            let data = data.clone();
            move || {
                for mut client in origin.incoming().map_while(Result::ok) {
                    let mut request = String::new();
                    let mut reader = BufReader::new(client.try_clone().unwrap());
                    let mut range = (0, data.len() - 1);
                    loop {
                        request.clear();
                        if reader.read_line(&mut request).unwrap() <= 2 {
                            break;
                        }
                        if let Some(bytes) =
                            request.to_ascii_lowercase().strip_prefix("range: bytes=")
                        {
                            let (a, b) = bytes.trim().split_once('-').unwrap();
                            range = (a.parse().unwrap(), b.parse().unwrap());
                        }
                    }
                    let body = &data[range.0..=range.1];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        range.0, range.1, data.len(), body.len()
                    );
                    client.write_all(head.as_bytes()).unwrap();
                    client.write_all(body).unwrap();
                }
            }
        });
        let proxy = Proxy::start().unwrap();
        proxy.routes.lock().unwrap().insert(
            proxy.path("video"),
            Route {
                url: origin_url,
                total: None,
            },
        );
        let from = 5;
        // Only the token's route answers.
        let guessed = format!("http://127.0.0.1:{}/video", proxy.port);
        assert!(matches!(
            ureq::get(&guessed).call(),
            Err(ureq::Error::Status(404, _))
        ));
        let response = ureq::get(&proxy.url("video"))
            .set("Range", &format!("bytes={from}-"))
            .call()
            .unwrap();
        assert_eq!(response.status(), 206);
        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body).unwrap();
        assert!(body == data[from..], "proxy changed the bytes");
        // "bytes=-N" is the last N, from the length learnt above.
        let response = ureq::get(&proxy.url("video"))
            .set("Range", "bytes=-10")
            .call()
            .unwrap();
        let mut tail = Vec::new();
        response.into_reader().read_to_end(&mut tail).unwrap();
        assert!(
            tail == data[data.len() - 10..],
            "suffix range served the wrong end"
        );
    }
}
