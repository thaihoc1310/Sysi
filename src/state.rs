use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub mascot: bool,
    #[serde(default = "default_true")]
    pub system: bool,
    #[serde(default = "default_true")]
    pub timer: bool,
    #[serde(default = "default_true")]
    pub settings_button: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mascot: true,
            system: true,
            timer: true,
            settings_button: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Note {
    pub id: u64,
    pub text: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub position: Point,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppState {
    #[serde(default)]
    pub layout_version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub positions: HashMap<String, Point>,
    #[serde(default)]
    pub sizes: HashMap<String, Size>,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default = "default_timer")]
    pub timer_seconds: i64,
    #[serde(default = "default_next_id")]
    pub next_note_id: u64,
}

impl Default for AppState {
    fn default() -> Self {
        let mut positions = HashMap::new();
        positions.insert("system".into(), Point { x: 34, y: 52 });
        positions.insert("notes".into(), Point { x: 34, y: 246 });
        Self {
            layout_version: 0,
            settings: Settings::default(),
            positions,
            sizes: HashMap::new(),
            notes: Vec::new(),
            timer_seconds: default_timer(),
            next_note_id: 1,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_timer() -> i64 {
    25 * 60
}

fn default_next_id() -> u64 {
    1
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sysi")
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sysi")
}

impl AppState {
    pub fn load() -> Self {
        let path = config_dir().join("state.json");
        fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join("state.json");
        let temp = dir.join("state.json.tmp");
        let raw = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fs::write(&temp, raw)?;
        fs::rename(temp, path)
    }
}
