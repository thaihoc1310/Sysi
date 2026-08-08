use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    Light,
    #[default]
    Gray,
    Dark,
}

impl ColorMode {
    pub fn next(self) -> Self {
        match self {
            Self::Light => Self::Gray,
            Self::Gray => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "LIGHT",
            Self::Gray => "GRAY",
            Self::Dark => "DARK",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Self::Light => "mode-light",
            Self::Gray => "mode-gray",
            Self::Dark => "mode-dark",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TimerStyle {
    #[default]
    Ring,
    Digital,
    Ticks,
    Arc,
}

impl TimerStyle {
    pub const ALL: [Self; 4] = [Self::Ring, Self::Digital, Self::Ticks, Self::Arc];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ring => "RING",
            Self::Digital => "DIGITAL",
            Self::Ticks => "TICKS",
            Self::Arc => "ARC",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Self::Ring => "timer-style-ring",
            Self::Digital => "timer-style-digital",
            Self::Ticks => "timer-style-ticks",
            Self::Arc => "timer-style-arc",
        }
    }

    pub fn default_size(self) -> Size {
        match self {
            Self::Ring | Self::Ticks | Self::Arc => Size {
                width: 116,
                height: 116,
            },
            Self::Digital => Size {
                width: 84,
                height: 36,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub system: bool,
    #[serde(default = "default_true")]
    pub timer: bool,
    #[serde(default = "default_true")]
    pub settings_button: bool,
    #[serde(default)]
    pub color_mode: ColorMode,
    #[serde(default)]
    pub system_details: SystemDetails,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            system: true,
            timer: true,
            settings_button: true,
            color_mode: ColorMode::default(),
            system_details: SystemDetails::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SystemDetails {
    #[serde(default = "default_true")]
    pub cpu: bool,
    #[serde(default = "default_true")]
    pub ram: bool,
    #[serde(default)]
    pub processes: bool,
    #[serde(default)]
    pub cores: bool,
}

impl Default for SystemDetails {
    fn default() -> Self {
        Self {
            cpu: true,
            ram: true,
            processes: false,
            cores: false,
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
    pub widget_color_modes: HashMap<String, ColorMode>,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default = "default_timer")]
    pub timer_seconds: i64,
    #[serde(default)]
    pub timer_style: TimerStyle,
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
            widget_color_modes: HashMap::new(),
            notes: Vec::new(),
            timer_seconds: default_timer(),
            timer_style: TimerStyle::default(),
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

#[cfg(test)]
mod tests {
    use super::{AppState, ColorMode, TimerStyle};

    #[test]
    fn old_settings_default_to_gray_mode() {
        let state: AppState = serde_json::from_str(
            r#"{"settings":{"mascot":true,"system":true,"timer":true,"settings_button":true}}"#,
        )
        .expect("legacy state should remain readable");
        assert_eq!(state.settings.color_mode, ColorMode::Gray);
    }

    #[test]
    fn color_mode_cycles_through_all_three_modes() {
        assert_eq!(ColorMode::Light.next(), ColorMode::Gray);
        assert_eq!(ColorMode::Gray.next(), ColorMode::Dark);
        assert_eq!(ColorMode::Dark.next(), ColorMode::Light);
    }

    #[test]
    fn old_state_defaults_to_no_widget_color_overrides() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"color_mode":"dark"}}"#)
            .expect("state without per-widget colors should remain readable");
        assert!(state.widget_color_modes.is_empty());
    }

    #[test]
    fn old_state_defaults_to_ring_timer_style() {
        let state: AppState = serde_json::from_str(r#"{"timer_seconds":120}"#)
            .expect("state without a timer style should remain readable");
        assert_eq!(state.timer_style, TimerStyle::Ring);
    }
}
