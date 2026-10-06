use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
pub const SAMPLE_RATE: u32 = 16000;
pub const CHANNELS: u16 = 1;
pub const BYTES_PER_SAMPLE: usize = 2;
const CHUNK_DURATION_MS: u32 = 20;
const CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize * CHUNK_DURATION_MS as usize) / 1000; // 320 samples
const CHUNK_BYTES: usize = CHUNK_SAMPLES * BYTES_PER_SAMPLE; // 640 bytes

// 7 minutes: as base64 WAV that is about 18 MB, under the 20 MB Gemini takes
// inline in one request.
pub const MAX_RECORDING_SECS: u64 = 420;
pub const MIN_RECORDING_SECS: f32 = 0.4;
const SILENCE_THRESHOLD_RMS: f32 = 0.035;

const GROQ_ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
const MODEL_NAME: &str = "whisper-large-v3";
const DEFAULT_PROMPT: &str = "Bản ghi âm tiếng Việt xen lẫn tiếng Anh kỹ thuật: Hãy thử check cái này for me, fix bug, deploy, API, review, frontend, backend, database, refactor, meeting, commit, merge, pull request, code, terminal, sysi.";

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

pub fn groq_key_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        })
        .join("sysi/groq.key")
}

pub fn read_groq_key() -> Option<String> {
    if let Ok(key) = std::env::var("GROQ_API_KEY") {
        let trimmed = key.trim().to_owned();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    let path = groq_key_path();
    fs::read_to_string(path).ok().and_then(|content| {
        let trimmed = content.trim().to_owned();
        if !trimmed.is_empty() {
            Some(trimmed)
        } else {
            None
        }
    })
}

pub fn save_groq_key(key: &str) -> std::io::Result<()> {
    let path = groq_key_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(key.trim().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

static HTTP_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::builder()
        .timeout_connect(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .user_agent("sysi/2.3")
        .build()
});

fn http_agent() -> ureq::Agent {
    HTTP_AGENT.clone()
}
/// Pre-warm DNS and TLS connection to Groq in background so transcribe latency is minimal.
pub fn prewarm_connection() {
    thread::spawn(|| {
        let _ = http_agent()
            .get("https://api.groq.com/openai/v1/models")
            .call();
    });
}

pub struct ActiveRecording {
    stop_flag: Arc<AtomicBool>,
    // None once the capture thread has reaped the recorder, so a late stop
    // or cancel never signals a pid the kernel may have handed to another process.
    child: Arc<Mutex<Option<Child>>>,
    pub level_rx: async_channel::Receiver<f32>,
    pub outcome_rx: async_channel::Receiver<Result<Vec<u8>, String>>,
}

impl ActiveRecording {
    fn signal(&self, signal: i32) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = child.as_ref() {
            unsafe {
                libc::kill(child.id() as i32, signal);
            }
        }
    }

    /// SIGINT lets pw-record exit gracefully and flush stdout.
    pub fn stop(&self) {
        self.signal(libc::SIGINT);
    }

    pub fn cancel(&self) {
        self.signal(libc::SIGKILL);
    }
}

pub fn start_recording() -> Result<ActiveRecording, String> {
    // Try pw-record first, fallback to parec
    let mut child = Command::new("pw-record")
        .args([
            "--rate",
            &SAMPLE_RATE.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            "--format",
            "s16",
            "--raw",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .or_else(|_| {
            Command::new("parec")
                .args([
                    "--rate",
                    &SAMPLE_RATE.to_string(),
                    "--channels",
                    &CHANNELS.to_string(),
                    "--format=s16le",
                    "--raw",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        })
        .map_err(|e| format!("Could not launch audio recorder: {e}"))?;

    let mut stdout = child.stdout.take().ok_or("No recorder stdout")?;
    let child = Arc::new(Mutex::new(Some(child)));
    let child_thread = child.clone();

    let (level_tx, level_rx) = async_channel::unbounded();
    let (outcome_tx, outcome_rx) = async_channel::bounded(1);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_thread = stop_flag.clone();

    // Kick off TLS pre-warm concurrently
    prewarm_connection();

    thread::Builder::new()
        .name("sysi-audio-capture".into())
        .spawn(move || {
            let start_time = Instant::now();
            let mut pcm_buffer: Vec<u8> = Vec::with_capacity(SAMPLE_RATE as usize * 2 * 10);
            let mut chunk = [0u8; CHUNK_BYTES];
            let mut peak_level = 0.0f32;

            loop {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                if start_time.elapsed() >= Duration::from_secs(MAX_RECORDING_SECS) {
                    break;
                }

                match stdout.read_exact(&mut chunk) {
                    Ok(()) => {
                        let level = compute_rms(&chunk);
                        if level > peak_level {
                            peak_level = level;
                        }
                        let _ = level_tx.try_send(level);
                        pcm_buffer.extend_from_slice(&chunk);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }

            // Ensure child is collected
            let mut taken = child_thread.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut child) = taken.take() {
                let _ = child.kill();
                let _ = child.wait();
            }

            let elapsed_secs = start_time.elapsed().as_secs_f32();
            if elapsed_secs < MIN_RECORDING_SECS {
                let _ = outcome_tx.send_blocking(Err("TOO SHORT".to_owned()));
                return;
            }
            if peak_level < SILENCE_THRESHOLD_RMS {
                let _ = outcome_tx.send_blocking(Err("NO SPEECH".to_owned()));
                return;
            }

            let _ = outcome_tx.send_blocking(Ok(pcm_buffer));
        })
        .map_err(|e| format!("Could not spawn audio reader: {e}"))?;

    Ok(ActiveRecording {
        stop_flag,
        child,
        level_rx,
        outcome_rx,
    })
}

pub fn compute_rms(chunk: &[u8]) -> f32 {
    if chunk.is_empty() {
        return 0.0;
    }
    let mut sum_sq = 0.0f64;
    let mut count = 0usize;
    for bytes in chunk.chunks_exact(2) {
        let sample = i16::from_le_bytes([bytes[0], bytes[1]]) as f64;
        sum_sq += sample * sample;
        count += 1;
    }
    if count == 0 {
        return 0.0;
    }
    let rms = (sum_sq / count as f64).sqrt();
    let norm = (rms / 32768.0) as f32;
    // Boost quiet speech so normal vocal amplitude fills 30-80% of the visual meter
    (norm.sqrt() * 1.6).clamp(0.0, 1.0)
}

pub fn pcm_to_wav(pcm: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    let mut wav = Vec::with_capacity(44 + pcm.len());
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let data_len = pcm.len() as u32;
    let riff_len = 36 + data_len;

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());

    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);

    wav
}

pub fn transcribe_audio(wav_data: &[u8], api_key: &str) -> Result<String, String> {
    if wav_data.is_empty() {
        return Err("NO AUDIO".to_owned());
    }
    let key = api_key.trim();
    if key.is_empty() {
        return Err("KEY NOT SET".to_owned());
    }

    let boundary = "----SysiVoiceBoundary7MA4YWxkTrZu0gW";
    let fields = [
        ("model", MODEL_NAME),
        ("response_format", "verbose_json"),
        ("prompt", DEFAULT_PROMPT),
        ("temperature", "0"),
    ];

    let body = make_multipart_body(
        boundary,
        &fields,
        "file",
        "audio.wav",
        "audio/wav",
        wav_data,
    );

    let agent = http_agent();
    let response = agent
        .post(GROQ_ENDPOINT)
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .set("Authorization", &format!("Bearer {key}"))
        .send_bytes(&body);

    match response {
        Ok(res) => {
            let text = res.into_string().map_err(|e| format!("Read error: {e}"))?;
            parse_transcription_response(&text)
        }
        Err(ureq::Error::Status(401, _)) => Err("INVALID KEY".to_owned()),
        Err(ureq::Error::Status(429, _)) => Err("RATE LIMIT".to_owned()),
        Err(ureq::Error::Status(code, res)) => {
            let detail = res.into_string().unwrap_or_default();
            eprintln!("Groq transcription error {code}: {detail}");
            Err(format!("HTTP {code}"))
        }
        Err(ureq::Error::Transport(e)) => {
            eprintln!("Groq transport error: {e}");
            Err("OFFLINE".to_owned())
        }
    }
}

fn parse_transcription_response(raw_json: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw_json).map_err(|e| format!("JSON error: {e}"))?;

    // Check segments for no_speech_prob
    if let Some(segments) = value.get("segments").and_then(|s| s.as_array()) {
        if !segments.is_empty() {
            let mut total_prob = 0.0f64;
            let mut count = 0;
            for seg in segments {
                if let Some(prob) = seg.get("no_speech_prob").and_then(|p| p.as_f64()) {
                    total_prob += prob;
                    count += 1;
                }
            }
            if count > 0 && (total_prob / count as f64) > 0.65 {
                return Err("NO SPEECH".to_owned());
            }
        }
    }

    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .unwrap_or_default();

    if text.is_empty() {
        return Err("NO SPEECH".to_owned());
    }

    Ok(text.to_owned())
}

fn make_multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field: &str,
    filename: &str,
    content_type: &str,
    file_data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, val) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}
pub fn gemini_key_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        })
        .join("sysi/gemini.key")
}

pub fn read_gemini_key() -> Option<String> {
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        let trimmed = key.trim().to_owned();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    let path = gemini_key_path();
    fs::read_to_string(path).ok().and_then(|content| {
        let trimmed = content.trim().to_owned();
        if !trimmed.is_empty() {
            Some(trimmed)
        } else {
            None
        }
    })
}

pub fn save_gemini_key(key: &str) -> std::io::Result<()> {
    let path = gemini_key_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(key.trim().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn transcribe_gemini_model(
    wav_data: &[u8],
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    let b64_audio = base64_encode(wav_data);
    let payload = if model == "gemini-3.5-transcribe" {
        serde_json::json!({
            "contents": [{
                "parts": [
                    { "inline_data": { "mime_type": "audio/wav", "data": b64_audio } }
                ]
            }]
        })
    } else {
        let prompt = "Transcribe this audio verbatim in Vietnamese and English. Output ONLY the plain transcription text without any explanation, markdown formatting, or notes.";
        serde_json::json!({
            "contents": [{
                "parts": [
                    { "text": prompt },
                    { "inline_data": { "mime_type": "audio/wav", "data": b64_audio } }
                ]
            }],
            "generationConfig": {
                "temperature": 0.0
            }
        })
    };

    let payload_str = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let agent = http_agent();
    let key = api_key.trim();
    // The key goes in a header, not the query: ureq's errors print the URL.
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");

    let resp = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("x-goog-api-key", key)
        .send_string(&payload_str);

    match resp {
        Ok(res) => {
            let body = res.into_string().map_err(|e| e.to_string())?;
            parse_gemini_response(&body)
        }
        Err(ureq::Error::Status(400 | 401 | 403, _)) => Err("INVALID KEY".to_owned()),
        Err(ureq::Error::Status(429, _)) => Err("RATE LIMIT".to_owned()),
        Err(ureq::Error::Status(code, res)) => {
            let detail = res.into_string().unwrap_or_default();
            eprintln!("Gemini {model} error {code}: {detail}");
            Err(format!("HTTP {code}"))
        }
        Err(ureq::Error::Transport(e)) => {
            eprintln!("Gemini {model} transport error: {e}");
            Err("OFFLINE".to_owned())
        }
    }
}

fn parse_gemini_response(raw_json: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw_json).map_err(|e| format!("JSON error: {e}"))?;
    let part0 = value
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0));

    let text = part0
        .and_then(|p| {
            p.get("text")
                .or_else(|| p.get("audioTranscription").and_then(|at| at.get("text")))
        })
        .and_then(|t| t.as_str())
        .map(str::trim)
        .unwrap_or_default();
    if text.is_empty() {
        return Err("NO SPEECH".to_owned());
    }

    Ok(text.to_owned())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscribeResult {
    pub text: String,
    pub provider: &'static str,
}

/// Transcribe with the chosen model, then with each of the others in turn if
/// it gives nothing back. A model without its key is skipped. When none
/// answers, the first one's error says why.
pub fn transcribe_audio_multi(
    pcm_bytes: &[u8],
    preference: crate::state::VoiceModel,
) -> Result<TranscribeResult, String> {
    use crate::state::VoiceModel;
    let wav = pcm_to_wav(pcm_bytes, SAMPLE_RATE, CHANNELS);
    let order = std::iter::once(preference).chain(
        VoiceModel::ALL
            .into_iter()
            .filter(|model| *model != preference),
    );
    let mut first_error = None;
    for model in order {
        let attempt = match model.gemini_model() {
            Some(name) => read_gemini_key().map(|key| transcribe_gemini_model(&wav, &key, name)),
            None => read_groq_key().map(|key| transcribe_audio(&wav, &key)),
        };
        match attempt {
            Some(Ok(text)) if !text.trim().is_empty() => {
                return Ok(TranscribeResult {
                    text: text.trim().to_owned(),
                    provider: model.label(),
                });
            }
            Some(Err(error)) => {
                first_error.get_or_insert(error);
            }
            _ => {}
        }
    }
    Err(first_error.unwrap_or_else(|| "KEY NOT SET".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pcm_to_wav_header() {
        let pcm = vec![0u8; 3200]; // 100ms at 16kHz mono 16-bit
        let wav = pcm_to_wav(&pcm, 16000, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + 3200);
    }

    #[test]
    fn test_compute_rms_silence_and_signal() {
        let silence = vec![0u8; 640];
        assert_eq!(compute_rms(&silence), 0.0);

        // Sine or full scale signal
        let mut full_scale = Vec::new();
        for _ in 0..320 {
            full_scale.extend_from_slice(&32767i16.to_le_bytes());
        }
        let rms = compute_rms(&full_scale);
        assert!(rms > 0.9);
    }

    #[test]
    fn test_parse_transcription_clean() {
        let json =
            r#"{"text": "Hello world, fix bug nhé", "segments": [{"no_speech_prob": 0.02}]}"#;
        assert_eq!(
            parse_transcription_response(json).unwrap(),
            "Hello world, fix bug nhé"
        );
    }

    #[test]
    fn test_parse_transcription_high_no_speech() {
        let json = r#"{"text": "Subscribe kênh nhé", "segments": [{"no_speech_prob": 0.95}]}"#;
        assert_eq!(parse_transcription_response(json).unwrap_err(), "NO SPEECH");
    }
}
