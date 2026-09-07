//! Small, read-only adapters for the usage surfaces Sysi can display.
//!
//! The adapters deliberately keep credentials and provider-specific JSON out of
//! the GTK layer. A snapshot contains only the values needed to render a quota
//! row, so an authentication failure can never accidentally end up in the
//! persisted Sysi state or in a widget label.

use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CODEX_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Source {
    Codex,
    Claude,
    Omp,
}

impl Source {
    pub const ALL: [Self; 3] = [Self::Codex, Self::Claude, Self::Omp];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "CODEX",
            Self::Claude => "CLAUDE",
            Self::Omp => "OMP",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Omp => "omp",
        }
    }

    pub fn from_key(value: &str) -> Self {
        match value {
            "claude" => Self::Claude,
            "omp" => Self::Omp,
            _ => Self::Codex,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub reset_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub source: Source,
    pub account: Option<String>,
    pub fetched_at_ms: i64,
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug)]
pub struct FetchError {
    pub message: String,
    pub retry_after: Duration,
}

impl From<String> for FetchError {
    fn from(message: String) -> Self {
        Self {
            message,
            retry_after: Duration::ZERO,
        }
    }
}

/// Per-source scheduling; manual refresh must also respect error cooldowns.
#[derive(Default)]
pub struct Schedule {
    pub busy: bool,
    next_poll: i64,
    blocked_until: i64,
    failures: u32,
    handled_reset: i64,
}

impl Schedule {
    pub fn retry_seconds(&self, now: i64) -> i64 {
        self.blocked_until
            .saturating_sub(now)
            .max(0)
            .saturating_add(999)
            / 1000
    }

    pub fn start(&mut self, now: i64, manual: bool, reset: Option<i64>) -> bool {
        let reset_due = reset.is_some_and(|reset| reset <= now && reset > self.handled_reset);
        if self.busy || now < self.blocked_until || (!manual && !reset_due && now < self.next_poll)
        {
            return false;
        }
        self.busy = true;
        if reset_due {
            self.handled_reset = reset.unwrap();
        }
        true
    }

    pub fn finish(&mut self, now: i64, source: Source, error: Option<&FetchError>) {
        self.busy = false;
        let delay = if let Some(error) = error {
            self.failures = self.failures.saturating_add(1);
            let backoff =
                (120u64.saturating_mul(1u64 << self.failures.min(4).saturating_sub(1))).min(900);
            backoff.max(
                error
                    .retry_after
                    .as_secs()
                    .saturating_add(u64::from(error.retry_after.subsec_nanos() > 0)),
            )
        } else {
            self.failures = 0;
            match source {
                Source::Codex => 60,
                _ => 120,
            }
        };
        self.next_poll = now.saturating_add(delay.saturating_mul(1000).min(i64::MAX as u64) as i64);
        self.blocked_until = if error.is_some() { self.next_poll } else { 0 };
    }
}

// Retry-After allows either delta seconds or an IMF-fixdate (RFC 9110).
fn retry_after(value: Option<&str>, now: i64) -> Duration {
    let Some(value) = value else {
        return Duration::ZERO;
    };
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Duration::from_secs(seconds);
    }
    let parts: Vec<_> = value.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" {
        return Duration::ZERO;
    }
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some(month) = months.iter().position(|month| *month == parts[2]) else {
        return Duration::ZERO;
    };
    let iso = format!("{}-{:02}-{}T{}Z", parts[3], month + 1, parts[1], parts[4]);
    let reset = parse_iso_reset(Some(&Value::String(iso))).unwrap_or(now);
    Duration::from_millis(reset.saturating_sub(now).max(0) as u64)
}

/// `force` marks a refresh the user asked for: OMP answers from a cache, so
/// without clearing it the card shows the same numbers with the same age.
pub fn fetch(source: Source, force: bool) -> Result<Snapshot, FetchError> {
    match source {
        Source::Codex => fetch_codex().map_err(Into::into),
        Source::Claude => fetch_claude(),
        Source::Omp => fetch_omp(force).map_err(Into::into),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    number.is_finite().then_some(number)
}

fn normalize_reset(value: Option<&Value>) -> Option<i64> {
    let raw = finite_number(value)?;
    if raw <= 0.0 || !raw.is_finite() {
        return None;
    }
    let millis = if raw > 1_000_000_000_000.0 {
        raw
    } else {
        raw * 1000.0
    };
    millis
        .is_finite()
        .then_some(millis.min(i64::MAX as f64) as i64)
}

fn parse_iso_reset(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?.trim();
    // GLib is already a Sysi dependency and understands offsets, fractional
    // seconds and the UTC `Z` spelling used by Anthropic.
    glib::DateTime::from_iso8601(text, None)
        .ok()
        .map(|datetime| datetime.to_unix().saturating_mul(1000))
}

fn window_from_percent(
    label: impl Into<String>,
    used_percent: Option<f64>,
    reset_at_ms: Option<i64>,
    duration_ms: Option<i64>,
) -> Option<Window> {
    let used_percent = used_percent.filter(|value| value.is_finite())?.max(0.0);
    Some(Window {
        label: label.into(),
        used_percent: Some(used_percent),
        remaining_percent: Some((100.0 - used_percent).clamp(0.0, 100.0)),
        reset_at_ms,
        duration_ms,
    })
}

fn codex_duration_label(minutes: Option<f64>, key: &str) -> (String, Option<i64>) {
    let Some(minutes) = minutes.filter(|value| value.is_finite() && *value > 0.0) else {
        return (
            match key {
                "primary" => "Primary".into(),
                "secondary" => "Weekly".into(),
                _ => key.to_owned(),
            },
            None,
        );
    };
    let rounded = minutes.round().max(1.0) as i64;
    let duration_ms = rounded.saturating_mul(60_000);
    let label = if rounded == 7 * 24 * 60 {
        "Weekly".to_owned()
    } else if rounded % (24 * 60) == 0 {
        format!("{}d", rounded / (24 * 60))
    } else if rounded % 60 == 0 {
        format!("{}h", rounded / 60)
    } else {
        format!("{}m", rounded)
    };
    (label, Some(duration_ms))
}

fn parse_codex_window(key: &str, value: &Value) -> Option<Window> {
    let object = value.as_object()?;
    let used = finite_number(
        object
            .get("usedPercent")
            .or_else(|| object.get("used_percent")),
    );
    let reset = normalize_reset(object.get("resetsAt").or_else(|| object.get("reset_at")));
    let minutes = finite_number(
        object
            .get("windowDurationMins")
            .or_else(|| object.get("window_duration_mins")),
    );
    let (duration_label, duration_ms) = codex_duration_label(minutes, key);
    window_from_percent(duration_label, used, reset, duration_ms)
}

fn parse_codex_limits(rate_limits: &Value) -> Vec<Window> {
    let mut windows = Vec::new();
    let mut seen = HashMap::<String, bool>::new();
    let mut add = |id: &str, value: &Value| {
        if let Some(window) = parse_codex_window(id, value) {
            let signature = format!(
                "{}:{:?}:{:?}",
                window.label, window.used_percent, window.reset_at_ms
            );
            if seen.insert(signature, true).is_none() {
                windows.push(window);
            }
        }
    };

    if let Some(object) = rate_limits.as_object() {
        if let Some(primary) = object.get("primary") {
            add("primary", primary);
        }
        if let Some(secondary) = object.get("secondary") {
            add("secondary", secondary);
        }
    }
    windows
}

fn append_unique_windows(windows: &mut Vec<Window>, extra: impl IntoIterator<Item = Window>) {
    for window in extra {
        let duplicate = windows.iter().any(|existing| {
            existing.label == window.label
                && existing.used_percent == window.used_percent
                && existing.reset_at_ms == window.reset_at_ms
        });
        if !duplicate {
            windows.push(window);
        }
    }
}

fn parse_codex_response(value: &Value, fetched_at_ms: i64) -> Result<Snapshot, String> {
    let result = value.get("result").unwrap_or(value);
    let object = result
        .as_object()
        .ok_or_else(|| "Codex returned an invalid rate-limit response".to_owned())?;
    let mut windows = object
        .get("rateLimits")
        .or_else(|| object.get("rate_limits"))
        .map(parse_codex_limits)
        .unwrap_or_default();

    // Newer app-server versions expose additional metered buckets. Keep them
    // when they carry useful windows, but never add the same primary/secondary
    // values twice when the map includes the canonical `codex` bucket.
    if let Some(buckets) = object
        .get("rateLimitsByLimitId")
        .or_else(|| object.get("rate_limits_by_limit_id"))
        .and_then(Value::as_object)
    {
        let mut ids = buckets.keys().collect::<Vec<_>>();
        ids.sort();
        for id in ids {
            let Some(bucket) = buckets.get(id) else {
                continue;
            };
            let mut extra = parse_codex_limits(bucket);
            if id != "codex" {
                for window in &mut extra {
                    window.label = format!("{} · {}", id, window.label);
                }
            }
            // The map is the reliable fallback when the compatibility field
            // contains null windows, and also fills a sparse response without
            // duplicating the canonical bucket.
            append_unique_windows(&mut windows, extra);
        }
    }
    if windows.is_empty() {
        return Err("Codex returned no usable windows".to_owned());
    }
    Ok(Snapshot {
        source: Source::Codex,
        account: None,
        fetched_at_ms,
        windows,
    })
}

fn resolve_executable(name: &str) -> PathBuf {
    let from_path = env::var_os("PATH").and_then(|path_var| {
        env::split_paths(&path_var)
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    });
    if let Some(path) = from_path {
        return path;
    }
    // Desktop launchers often start with a shorter PATH than an interactive
    // shell. Claude Code, Codex and OMP commonly live here after a per-user
    // install, so make that location work without requiring a shell wrapper.
    if let Some(home) = env::var_os("HOME") {
        let path = PathBuf::from(home).join(".local").join("bin").join(name);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(name)
}

fn command_start_error(name: &str, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        return format!("{name} CLI was not found; install it or add it to PATH");
    }
    format!("Could not start {name}: {error}")
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_codex_request() -> Result<Value, String> {
    let mut child = Command::new(resolve_executable("codex"))
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| command_start_error("Codex", error))?;
    let Some(mut stdin) = child.stdin.take() else {
        terminate_child(&mut child);
        return Err("Codex app-server has no stdin".to_owned());
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err("Codex app-server has no stdout".to_owned());
    };
    let (line_tx, line_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let messages = [
        json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "sysi",
                    "title": "Sysi",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
        json!({ "method": "initialized", "params": {} }),
        json!({ "method": "account/rateLimits/read", "id": 2, "params": {} }),
    ];
    for message in messages {
        let encoded = match serde_json::to_vec(&message) {
            Ok(encoded) => encoded,
            Err(error) => {
                terminate_child(&mut child);
                return Err(error.to_string());
            }
        };
        if let Err(error) = stdin
            .write_all(&encoded)
            .and_then(|_| stdin.write_all(b"\n"))
        {
            terminate_child(&mut child);
            return Err(format!("Could not query Codex app-server: {error}"));
        }
    }
    if let Err(error) = stdin.flush() {
        terminate_child(&mut child);
        return Err(format!("Could not flush Codex app-server request: {error}"));
    }
    // Keeping stdin alive lets app-server finish its asynchronous request. The
    // child is killed immediately after response id 2, so no process survives
    // a refresh.
    let deadline = Instant::now() + CODEX_TIMEOUT;
    let mut response = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(remaining) {
            Ok(line) => {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_i64) == Some(2) {
                    response = Some(value);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    terminate_child(&mut child);
    response.ok_or_else(|| "Timed out waiting for Codex usage".to_owned())
}

fn fetch_codex() -> Result<Snapshot, String> {
    let fetched_at_ms = now_ms();
    let response = run_codex_request()?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex rejected the usage request");
        return Err(message.to_owned());
    }
    let mut snapshot = parse_codex_response(&response, fetched_at_ms)?;
    snapshot.account = codex_email();
    Ok(snapshot)
}

/// Base64url payload of a JWT. Only used on locally stored tokens, so a
/// malformed token just means "no email", never an error the user sees.
fn jwt_payload(token: &str) -> Option<Value> {
    let mut bits = 0u32;
    let mut acc = 0u32;
    let mut bytes = Vec::new();
    for byte in token.split('.').nth(1)?.bytes() {
        let index = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(index);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    serde_json::from_slice(&bytes).ok()
}

fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn email(value: Option<&Value>) -> Option<String> {
    let email = value?.as_str()?.trim();
    (!email.is_empty()).then(|| email.to_owned())
}

/// The signed-in address, read from the CLI's own config. Both APIs return only
/// an opaque account id, which tells the person looking at the card nothing.
fn codex_email() -> Option<String> {
    let home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
    let auth = read_json(home.join("auth.json"))?;
    let payload = jwt_payload(auth.pointer("/tokens/id_token")?.as_str()?)?;
    email(payload.get("email"))
}

fn claude_email() -> Option<String> {
    let home = env::var_os("CLAUDE_CONFIG_DIR")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)?;
    let config = read_json(home.join(".claude.json"))?;
    email(config.pointer("/oauthAccount/emailAddress"))
}

fn claude_credentials_path() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
        .join(".credentials.json")
}

fn read_claude_token() -> Result<String, String> {
    if let Ok(token) = env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token.trim().to_owned());
        }
    }
    let path = claude_credentials_path();
    let raw = fs::read_to_string(&path)
        .map_err(|_| format!("Claude credentials not found at {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| "Claude credentials are not valid JSON".to_owned())?;
    let oauth = value.get("claudeAiOauth").unwrap_or(&value);
    oauth
        .get("accessToken")
        .or_else(|| oauth.get("access_token"))
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Claude OAuth access token is missing".to_owned())
}

fn claude_bucket(value: Option<&Value>, label: &str, duration_ms: i64) -> Option<Window> {
    let object = value?.as_object()?;
    let used =
        finite_number(object.get("utilization")).or_else(|| finite_number(object.get("percent")));
    let reset = parse_iso_reset(object.get("resets_at").or_else(|| object.get("resetsAt")));
    window_from_percent(label, used, reset, Some(duration_ms))
}

fn money_amount(value: Option<&Value>) -> Option<f64> {
    let object = value?.as_object()?;
    let minor = finite_number(object.get("amount_minor"))?;
    let exponent = finite_number(object.get("exponent")).unwrap_or(0.0);
    if minor < 0.0 || !(0.0..=18.0).contains(&exponent) {
        return None;
    }
    let amount = minor / 10_f64.powf(exponent);
    amount.is_finite().then_some(amount)
}

fn claude_extra_bucket(value: Option<&Value>, label: &str) -> Option<Window> {
    let object = value?.as_object()?;
    if object
        .get("is_enabled")
        .or_else(|| object.get("enabled"))
        .and_then(Value::as_bool)
        .is_some_and(|enabled| !enabled)
    {
        return None;
    }
    let used =
        finite_number(object.get("used_credits")).or_else(|| money_amount(object.get("used")))?;
    let limit =
        finite_number(object.get("monthly_limit")).or_else(|| money_amount(object.get("limit")))?;
    if used < 0.0 || limit <= 0.0 {
        return None;
    }
    let reset = parse_iso_reset(object.get("resets_at").or_else(|| object.get("resetsAt")));
    window_from_percent(
        label,
        Some(used / limit * 100.0),
        reset,
        Some(30 * 24 * 60 * 60 * 1000),
    )
}

fn parse_claude_response(value: &Value, fetched_at_ms: i64) -> Result<Snapshot, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Claude returned an invalid usage response".to_owned())?;
    let mut windows = Vec::new();
    if let Some(window) = claude_bucket(object.get("five_hour"), "5h", 5 * 60 * 60 * 1000) {
        windows.push(window);
    }
    if let Some(window) = claude_bucket(object.get("seven_day"), "Weekly", 7 * 24 * 60 * 60 * 1000)
    {
        windows.push(window);
    }
    for (key, label) in [
        ("seven_day_opus", "Weekly · Opus"),
        ("seven_day_sonnet", "Weekly · Sonnet"),
    ] {
        if let Some(window) = claude_bucket(object.get(key), label, 7 * 24 * 60 * 60 * 1000) {
            windows.push(window);
        }
    }
    // Enterprise/team accounts can expose only a monthly spend cap. It is a
    // real quota when both sides of the amount are present; never turn a
    // dollar total without a denominator into a percentage.
    if let Some(window) = claude_extra_bucket(object.get("spend"), "Monthly")
        .or_else(|| claude_extra_bucket(object.get("extra_usage"), "Monthly"))
    {
        windows.push(window);
    }
    if let Some(limits) = object.get("limits").and_then(Value::as_array) {
        for entry in limits {
            let Some(entry_object) = entry.as_object() else {
                continue;
            };
            if entry_object.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
                continue;
            }
            let name = entry_object
                .get("scope")
                .and_then(Value::as_object)
                .and_then(|scope| scope.get("model"))
                .and_then(Value::as_object)
                .and_then(|model| model.get("display_name"))
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("model");
            if let Some(window) = claude_bucket(
                Some(entry),
                &format!("Weekly · {name}"),
                7 * 24 * 60 * 60 * 1000,
            ) {
                windows.push(window);
            }
        }
    }
    if windows.is_empty() {
        return Err("Claude returned no usable usage windows".to_owned());
    }
    Ok(Snapshot {
        source: Source::Claude,
        account: None,
        fetched_at_ms,
        windows,
    })
}

fn fetch_claude() -> Result<Snapshot, FetchError> {
    let token = read_claude_token()?;
    let agent = ureq::builder()
        .timeout_connect(HTTP_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .redirects(0)
        .user_agent("sysi-usage")
        .build();
    let response = agent
        .get(CLAUDE_USAGE_URL)
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|error| {
            let delay = match &error {
                ureq::Error::Status(_, response) => {
                    retry_after(response.header("Retry-After"), now_ms())
                }
                _ => Duration::ZERO,
            };
            let message = match error {
                ureq::Error::Status(401, _) => {
                    "Claude login expired; sign in again with Claude Code".to_owned()
                }
                ureq::Error::Status(403, _) => {
                    "Claude usage unavailable for this credential (profile scope required)"
                        .to_owned()
                }
                ureq::Error::Status(429, _) => {
                    "Claude usage is rate limited; waiting before retry".to_owned()
                }
                ureq::Error::Status(code, _) => {
                    format!("Claude usage request failed (HTTP {code})")
                }
                _ => format!("Claude usage request failed: {error}"),
            };
            FetchError {
                message,
                retry_after: delay,
            }
        })?;
    if (300..400).contains(&response.status()) {
        return Err(
            "Claude usage endpoint redirected; no credential was forwarded"
                .to_owned()
                .into(),
        );
    }
    let body = response
        .into_string()
        .map_err(|error| format!("Could not read Claude usage: {error}"))?;
    let value: Value =
        serde_json::from_str(&body).map_err(|_| "Claude returned invalid usage JSON".to_owned())?;
    let mut snapshot = parse_claude_response(&value, now_ms())?;
    snapshot.account = claude_email();
    Ok(snapshot)
}

fn amount_percent(amount: &Value) -> Option<(f64, f64)> {
    let object = amount.as_object()?;
    let mut used_fraction = finite_number(object.get("usedFraction")).or_else(|| {
        let used = finite_number(object.get("used"))?;
        if let Some(limit) = finite_number(object.get("limit")).filter(|limit| *limit > 0.0) {
            return Some(used / limit);
        }
        (object.get("unit").and_then(Value::as_str) == Some("percent")).then_some(used / 100.0)
    });
    used_fraction = used_fraction.or_else(|| {
        let remaining = finite_number(object.get("remaining"))?;
        let limit = finite_number(object.get("limit"))?;
        (limit > 0.0).then_some(1.0 - remaining / limit)
    });
    let remaining_fraction = finite_number(object.get("remainingFraction"))
        .or_else(|| used_fraction.map(|fraction| 1.0 - fraction));
    used_fraction = used_fraction.or_else(|| remaining_fraction.map(|fraction| 1.0 - fraction));
    let used = used_fraction?.max(0.0) * 100.0;
    let remaining = remaining_fraction
        .or_else(|| used_fraction.map(|fraction| 1.0 - fraction))?
        .clamp(0.0, 1.0)
        * 100.0;
    Some((used, remaining))
}

fn parse_omp_report(
    report: &Value,
    windows: &mut Vec<Window>,
    accounts: &mut Vec<String>,
    fetched: &mut i64,
) {
    let Some(object) = report.as_object() else {
        return;
    };
    let provider = object
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("provider");
    if let Some(timestamp) = finite_number(object.get("fetchedAt")) {
        let timestamp = normalize_reset(Some(&Value::from(timestamp))).unwrap_or(0);
        if timestamp > 0 {
            *fetched = if *fetched == 0 {
                timestamp
            } else {
                (*fetched).min(timestamp)
            };
        }
    }
    if let Some(email) = object
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("email"))
        .and_then(Value::as_str)
    {
        if !email.trim().is_empty() && !accounts.iter().any(|existing| existing == email) {
            accounts.push(email.to_owned());
        }
    }
    let limits = object.get("limits").and_then(Value::as_array);
    if limits.is_none_or(|limits| limits.is_empty()) {
        let account = object
            .get("metadata")
            .and_then(|metadata| metadata.get("email"))
            .and_then(Value::as_str)
            .unwrap_or("account unspecified");
        windows.push(Window {
            label: format!("{provider} · {account} · No quota reported"),
            used_percent: None,
            remaining_percent: None,
            reset_at_ms: None,
            duration_ms: None,
        });
        return;
    }
    for limit in limits.unwrap() {
        let Some(limit_object) = limit.as_object() else {
            continue;
        };
        let percent = limit_object.get("amount").and_then(amount_percent);
        let window = limit_object.get("window").and_then(Value::as_object);
        let reset = window.and_then(|window| {
            normalize_reset(window.get("resetsAt").or_else(|| window.get("resets_at")))
        });
        let duration = window
            .and_then(|window| finite_number(window.get("durationMs")))
            .map(|value| value.clamp(0.0, i64::MAX as f64) as i64);
        let label = limit_object
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("Usage");
        let account = limit_object
            .get("scope")
            .and_then(|scope| scope.get("accountId"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                object
                    .get("metadata")
                    .and_then(|metadata| metadata.get("email"))
                    .and_then(Value::as_str)
            });
        let label = match account {
            Some(account) => format!("{provider} · {account} · {label}"),
            None => format!("{provider} · account unspecified · {label}"),
        };
        windows.push(Window {
            label,
            used_percent: percent.map(|(used, _)| used),
            remaining_percent: percent.map(|(_, remaining)| remaining),
            reset_at_ms: reset,
            duration_ms: duration,
        });
    }
}

fn invalidate_omp_cache() {
    let Ok(mut child) = Command::new(resolve_executable("omp"))
        .args(["usage", "invalidate"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = Instant::now() + HTTP_TIMEOUT;
    while Instant::now() < deadline {
        // A failure here is not worth surfacing: the report still renders,
        // just from the cache the refresh meant to skip.
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    terminate_child(&mut child);
}

fn fetch_omp(force: bool) -> Result<Snapshot, String> {
    if force {
        invalidate_omp_cache();
    }
    let mut child = Command::new(resolve_executable("omp"))
        .args(["usage", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| command_start_error("OMP", error))?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err("OMP has no stdout".to_owned());
    };
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut output = Vec::new();
        let _ = BufReader::new(stdout).read_to_end(&mut output);
        let _ = output_tx.send(output);
    });
    let deadline = Instant::now() + CODEX_TIMEOUT;
    let status;
    loop {
        match child.try_wait() {
            Ok(Some(next_status)) => {
                status = next_status;
                break;
            }
            Ok(None) => {}
            Err(error) => {
                terminate_child(&mut child);
                return Err(format!("Could not read OMP status: {error}"));
            }
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            return Err("Timed out waiting for OMP usage".to_owned());
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !status.success() {
        return Err(format!("OMP usage exited with {status}"));
    }
    let output = output_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "Timed out reading OMP usage output".to_owned())?;
    let value: Value = serde_json::from_slice(&output)
        .map_err(|_| "OMP returned invalid usage JSON".to_owned())?;
    let reports = value
        .get("reports")
        .and_then(Value::as_array)
        .ok_or_else(|| "OMP JSON has no reports array".to_owned())?;
    let mut windows = Vec::new();
    let mut accounts = Vec::new();
    let mut fetched = 0;
    for report in reports {
        parse_omp_report(report, &mut windows, &mut accounts, &mut fetched);
    }
    if windows.is_empty() {
        return Err("OMP has no reported quota windows".to_owned());
    }
    Ok(Snapshot {
        source: Source::Omp,
        account: (!accounts.is_empty()).then(|| accounts.join(", ")),
        fetched_at_ms: if fetched > 0 { fetched } else { now_ms() },
        windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_payload_reads_the_signed_in_email() {
        let token = format!(
            "header.{}.signature",
            "eyJlbWFpbCI6ICJwZXJzb25AZXhhbXBsZS5jb20iLCAic3ViIjogIngifQ"
        );
        assert_eq!(
            email(jwt_payload(&token).as_ref().and_then(|p| p.get("email"))).as_deref(),
            Some("person@example.com")
        );
        assert!(jwt_payload("not-a-jwt").is_none());
        assert!(jwt_payload("header.***.signature").is_none());
    }

    #[test]
    fn independent_sources_and_duplicate_refreshes() {
        let mut codex = Schedule::default();
        let mut claude = Schedule::default();
        let mut omp = Schedule::default();
        assert!(codex.start(1000, true, None));
        assert!(claude.start(1000, true, None));
        assert!(omp.start(1000, true, None));
        assert!(!codex.start(1001, true, None));
        codex.finish(2000, Source::Codex, None);
        assert!(codex.start(2001, true, None));
    }

    #[test]
    fn cooldown_blocks_manual_poll_and_reset_until_retry_after() {
        let mut schedule = Schedule::default();
        let error = FetchError {
            message: "429".into(),
            retry_after: Duration::from_secs(1800),
        };
        schedule.finish(1000, Source::Claude, Some(&error));
        for manual in [true, false] {
            assert!(!schedule.start(1_800_999, manual, Some(2000)));
        }
        assert!(schedule.start(1_801_000, true, None));
    }

    #[test]
    fn errors_back_off_and_success_recovers() {
        let mut schedule = Schedule::default();
        let error = FetchError::from("offline".to_owned());
        for seconds in [120, 240, 480, 900, 900] {
            schedule.finish(0, Source::Codex, Some(&error));
            assert_eq!(schedule.blocked_until, seconds * 1000);
        }
        schedule.finish(0, Source::Codex, None);
        assert!(schedule.start(1, true, None));
    }

    #[test]
    fn reset_fetches_once_without_waiting_for_poll() {
        let mut schedule = Schedule::default();
        schedule.finish(1000, Source::Claude, None);
        assert!(!schedule.start(1999, false, Some(2000)));
        assert!(schedule.start(2000, false, Some(2000)));
        schedule.finish(2001, Source::Claude, None);
        assert!(!schedule.start(2002, false, Some(2000)));
        assert!(schedule.start(3000, false, Some(3000)));
    }

    #[test]
    fn retry_after_parses_seconds_dates_and_invalid_values() {
        assert_eq!(retry_after(Some("1800"), 0).as_secs(), 1800);
        let now = parse_iso_reset(Some(&json!("2030-09-14T12:00:00Z"))).unwrap();
        assert_eq!(
            retry_after(Some("Sat, 14 Sep 2030 12:05:00 GMT"), now).as_secs(),
            300
        );
        assert_eq!(
            retry_after(Some("Sat, 14 Sep 2030 11:00:00 GMT"), now),
            Duration::ZERO
        );
        assert_eq!(retry_after(Some("invalid"), now), Duration::ZERO);
    }

    #[test]
    fn omp_accounts_remain_distinct_and_all_rows_survive() {
        let limits: Vec<_> = (0..20)
            .map(|i| {
                json!({
                    "label": "Weekly", "scope": {"accountId": format!("account-{i}")},
                    "amount": {"usedFraction": 0.25}
                })
            })
            .collect();
        let mut windows = Vec::new();
        parse_omp_report(
            &json!({"provider": "codex", "limits": limits}),
            &mut windows,
            &mut Vec::new(),
            &mut 0,
        );
        assert_eq!(windows.len(), 20);
        assert_ne!(windows[0].label, windows[1].label);
        assert!(windows[19].label.contains("account-19"));
    }

    #[test]
    fn omp_age_uses_oldest_report_and_retains_overage() {
        let mut windows = Vec::new();
        let mut accounts = Vec::new();
        let mut fetched = 0;
        for timestamp in [1_800_000_000_000i64, 1_700_000_000_000] {
            parse_omp_report(
                &json!({"provider": "test", "fetchedAt": timestamp, "limits": []}),
                &mut windows,
                &mut accounts,
                &mut fetched,
            );
        }
        assert_eq!(fetched, 1_700_000_000_000);
        assert_eq!(
            amount_percent(&json!({"usedFraction": 1.2})),
            Some((120.0, 0.0))
        );
        assert_eq!(
            amount_percent(&json!({"used": 25, "limit": 50, "unit": "percent"})),
            Some((50.0, 50.0))
        );
    }

    #[test]
    fn codex_windows_use_remaining_percent_and_server_resets() {
        let value = serde_json::json!({
            "result": {"rateLimits": {
                "primary": {"usedPercent": 34, "windowDurationMins": 300, "resetsAt": 1_800_000_000},
                "secondary": {"usedPercent": 37, "windowDurationMins": 10080, "resetsAt": 1_900_000_000}
            }}
        });
        let snapshot = parse_codex_response(&value, 10).expect("valid Codex fixture");
        assert_eq!(snapshot.windows[0].label, "5h");
        assert_eq!(snapshot.windows[0].remaining_percent, Some(66.0));
        assert_eq!(snapshot.windows[1].label, "Weekly");
        assert_eq!(snapshot.windows[1].reset_at_ms, Some(1_900_000_000_000));
    }

    #[test]
    fn codex_bucket_map_fills_a_sparse_compatibility_field() {
        let value = serde_json::json!({
            "result": {
                "rateLimits": {"primary": null, "secondary": null},
                "rateLimitsByLimitId": {
                    "codex": {"primary": {"usedPercent": 8, "windowDurationMins": 300}}
                }
            }
        });
        let snapshot = parse_codex_response(&value, 10).expect("mapped fixture");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].label, "5h");
        assert_eq!(snapshot.windows[0].remaining_percent, Some(92.0));
    }

    #[test]
    fn claude_null_bucket_is_omitted_without_becoming_full() {
        let value = serde_json::json!({
            "five_hour": null,
            "seven_day": {"utilization": 12, "resets_at": "2030-09-14T12:00:00Z"}
        });
        let snapshot = parse_claude_response(&value, 10).expect("weekly fixture");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].remaining_percent, Some(88.0));
    }

    #[test]
    fn claude_monthly_spend_is_reported_only_with_a_cap() {
        let value = serde_json::json!({
            "five_hour": null,
            "seven_day": null,
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 600,
                "used_credits": 434.43
            }
        });
        let snapshot = parse_claude_response(&value, 10).expect("monthly fixture");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].label, "Monthly");
        assert!((snapshot.windows[0].used_percent.unwrap() - 72.405).abs() < 0.001);
        assert!((snapshot.windows[0].remaining_percent.unwrap() - 27.595).abs() < 0.001);
    }

    #[test]
    fn omp_prefers_remaining_fraction_and_keeps_provider_labels() {
        let value = serde_json::json!({
            "reports": [{"provider": "anthropic", "fetchedAt": 1_700_000_000_000i64, "limits": [{
                "label": "5 Hour limit",
                "amount": {"usedFraction": 0.34, "remainingFraction": 0.66, "unit": "percent"},
                "window": {"resetsAt": 1_800_000_000_000i64}
            }]}]
        });
        let snapshot = {
            let reports = value.get("reports").and_then(Value::as_array).unwrap();
            let mut windows = Vec::new();
            let mut accounts = Vec::new();
            let mut fetched = 0;
            for report in reports {
                parse_omp_report(report, &mut windows, &mut accounts, &mut fetched);
            }
            (windows, fetched)
        };
        assert_eq!(
            snapshot.0[0].label,
            "anthropic · account unspecified · 5 Hour limit"
        );
        assert_eq!(snapshot.0[0].remaining_percent, Some(66.0));
        assert_eq!(snapshot.1, 1_700_000_000_000);
    }

    #[test]
    fn omp_does_not_turn_an_undivided_cost_into_a_percentage() {
        let value = serde_json::json!({
            "provider": "anthropic",
            "limits": [{
                "label": "Cost",
                "amount": {"used": 3.0, "unit": "usd"}
            }]
        });
        let mut windows = Vec::new();
        let mut accounts = Vec::new();
        let mut fetched = 0;
        parse_omp_report(&value, &mut windows, &mut accounts, &mut fetched);
        assert_eq!(windows[0].remaining_percent, None);
    }
}
