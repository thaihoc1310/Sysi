use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    #[default]
    #[serde(alias = "gray")]
    Auto,
    Light,
    Dark,
}

impl ColorMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Light, Self::Dark];

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Light => "LIGHT",
            Self::Dark => "DARK",
        }
    }

    /// What the GNOME panel prints on its colour-mode button, and what it
    /// writes into the shared panel-state file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
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
    pub history_open: bool,
    #[serde(default)]
    pub translate_open: bool,
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
            history_open: false,
            translate_open: false,
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
    pub swap: bool,
    #[serde(default)]
    pub processes: bool,
    #[serde(default)]
    pub cores: bool,
    #[serde(default)]
    pub gpus: bool,
    #[serde(default)]
    pub cpu_temp: bool,
    #[serde(default)]
    pub gpu_temp: bool,
    #[serde(default)]
    pub ssd_temp: bool,
    #[serde(default)]
    pub memory_detail: bool,
    #[serde(default)]
    pub root_disk: bool,
    #[serde(default)]
    pub home_disk: bool,
    #[serde(default)]
    pub network: bool,
}

impl Default for SystemDetails {
    fn default() -> Self {
        Self {
            cpu: true,
            ram: true,
            swap: false,
            processes: false,
            cores: false,
            gpus: false,
            cpu_temp: false,
            gpu_temp: false,
            ssd_temp: false,
            memory_detail: false,
            root_disk: false,
            home_disk: false,
            network: false,
        }
    }
}

// A pasted image lives as a file next to the notes, and the note text keeps a
// U+FFFC object-replacement character where it sits. The images list runs in
// the same order as those placeholders, so text and images stay interleaved
// through a save/load round trip.
pub const IMAGE_PLACEHOLDER: char = '\u{fffc}';

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NoteImage {
    pub file: String,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Note {
    pub id: u64,
    pub text: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub position: Point,
    #[serde(default)]
    pub images: Vec<NoteImage>,
}

/// One dictionary window. The queries it has shown are kept with it so that
/// back and forward still work after a restart, the way browser tabs do.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DictionaryWindow {
    pub id: u64,
    /// Oldest first; `cursor` points at the entry currently on screen.
    #[serde(default)]
    pub history: Vec<String>,
    #[serde(default)]
    pub cursor: usize,
}

impl DictionaryWindow {
    pub fn query(&self) -> Option<&str> {
        self.history.get(self.cursor).map(String::as_str)
    }
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
    /// The dictionary windows that exist, in the order they were opened.
    #[serde(default)]
    pub dictionaries: Vec<DictionaryWindow>,
    /// The last few dictionary queries, most recent first.
    #[serde(default)]
    pub recent_searches: Vec<String>,
    #[serde(default = "default_timer")]
    pub timer_seconds: i64,
    #[serde(default)]
    pub timer_style: TimerStyle,
    #[serde(default = "default_next_id")]
    pub next_note_id: u64,
    #[serde(default = "default_next_id")]
    pub next_dictionary_id: u64,
    #[serde(default = "default_next_id")]
    pub next_image_id: u64,
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
            dictionaries: Vec::new(),
            recent_searches: Vec::new(),
            timer_seconds: default_timer(),
            timer_style: TimerStyle::default(),
            next_note_id: 1,
            next_dictionary_id: 1,
            next_image_id: 1,
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

// Pasted images are user data, not a cache: losing them would gut the note
// that shows them, so they go under XDG_DATA_HOME rather than the cache dir.
pub fn images_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sysi")
        .join("images")
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
        let Ok(raw) = fs::read_to_string(&path) else {
            // No file yet, or it cannot be read at all. Either way there is
            // nothing to lose by starting fresh.
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(state) => state,
            Err(error) => {
                // Quietly starting from defaults would be silent data loss:
                // the very first save overwrites the file that still holds
                // every note. Move it aside and say where it went instead.
                let kept = path.with_extension("json.unreadable");
                eprintln!(
                    "Could not read Sysi state ({error}). The old file has been kept at {}.",
                    kept.display()
                );
                let _ = fs::rename(&path, &kept);
                Self::default()
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let result = (|| {
            let dir = config_dir();
            fs::create_dir_all(&dir)?;
            let path = dir.join("state.json");
            let temp = dir.join("state.json.tmp");
            let raw = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
            fs::write(&temp, raw)?;
            fs::rename(temp, path)
        })();
        if let Err(error) = &result {
            eprintln!("Could not save Sysi state: {error}");
        }
        result
    }

    // Delete image files no note points at any more. Deleting a note that
    // showed an image, or backspacing over the placeholder, would otherwise
    // leave the file behind for good.
    pub fn prune_orphan_images(&self) {
        let referenced = self.referenced_image_files();
        let Ok(entries) = fs::read_dir(images_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !referenced.contains(name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    pub fn referenced_image_files(&self) -> std::collections::HashSet<String> {
        self.notes
            .iter()
            .flat_map(|note| note.images.iter())
            .map(|image| image.file.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, ColorMode, TimerStyle};

    #[test]
    fn old_settings_default_to_auto_mode() {
        let state: AppState = serde_json::from_str(
            r#"{"settings":{"mascot":true,"system":true,"timer":true,"settings_button":true}}"#,
        )
        .expect("legacy state should remain readable");
        assert_eq!(state.settings.color_mode, ColorMode::Auto);
    }

    #[test]
    fn color_mode_cycles_through_all_three_modes() {
        assert_eq!(ColorMode::Auto.next(), ColorMode::Light);
        assert_eq!(ColorMode::Light.next(), ColorMode::Dark);
        assert_eq!(ColorMode::Dark.next(), ColorMode::Auto);
    }

    #[test]
    fn a_widget_color_menu_has_only_the_other_two_modes() {
        for current in ColorMode::ALL {
            let options: Vec<_> = ColorMode::ALL
                .into_iter()
                .filter(|mode| *mode != current)
                .collect();
            assert_eq!(options.len(), 2);
            assert!(!options.contains(&current));
        }
    }

    #[test]
    fn legacy_gray_mode_migrates_to_auto() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"color_mode":"gray"}}"#)
            .expect("the removed gray mode should remain readable");
        assert_eq!(state.settings.color_mode, ColorMode::Auto);
        assert!(serde_json::to_string(&state)
            .expect("migrated state should serialize")
            .contains(r#""color_mode":"auto""#));
    }

    #[test]
    fn old_state_defaults_to_no_widget_color_overrides() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"color_mode":"dark"}}"#)
            .expect("state without per-widget colors should remain readable");
        assert!(state.widget_color_modes.is_empty());
    }

    #[test]
    fn old_state_defaults_to_a_closed_history_window() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"system":true}}"#)
            .expect("state without a history flag should remain readable");
        assert!(!state.settings.history_open);
    }

    #[test]
    fn old_state_defaults_to_a_closed_translate_window() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"system":true}}"#)
            .expect("state without a translate flag should remain readable");
        assert!(!state.settings.translate_open);
    }

    #[test]
    fn old_state_loads_without_recent_searches() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"system":true}}"#)
            .expect("state saved before search history should remain readable");
        assert!(state.recent_searches.is_empty());
    }

    #[test]
    fn old_notes_load_without_images() {
        let state: AppState = serde_json::from_str(
            r#"{"notes":[{"id":1,"text":"hello","pinned":true}],"next_note_id":2}"#,
        )
        .expect("notes saved before image support should remain readable");
        assert!(state.notes[0].images.is_empty());
        assert_eq!(state.next_image_id, 1);
    }

    #[test]
    fn orphan_image_files_are_the_ones_no_note_references() {
        let state: AppState = serde_json::from_str(
            r#"{"notes":[{"id":1,"text":"a\ufffcb","images":[{"file":"7.png","width":80,"height":60}]}]}"#,
        )
        .expect("a note with an image should be readable");
        let referenced = state.referenced_image_files();
        assert!(referenced.contains("7.png"));
        assert!(!referenced.contains("8.png"));
    }

    #[test]
    fn old_state_defaults_to_ring_timer_style() {
        let state: AppState = serde_json::from_str(r#"{"timer_seconds":120}"#)
            .expect("state without a timer style should remain readable");
        assert_eq!(state.timer_style, TimerStyle::Ring);
    }

    #[test]
    fn a_state_saved_before_the_new_sensors_keeps_the_sections_it_had() {
        // What a card with the disk meters on used to save. The sections it
        // never knew about have to come back off rather than switch themselves
        // on for someone who never asked.
        let state: AppState = serde_json::from_str(
            r#"{"settings":{"system_details":{"cpu":true,"ram":true,"gpus":true,"root_disk":true,"home_disk":true,"processes":false,"cores":false}}}"#,
        )
        .expect("settings saved before the new sensors should remain readable");
        let details = state.settings.system_details;
        assert!(details.cpu && details.ram && details.gpus);
        assert!(details.root_disk && details.home_disk);
        assert!(!details.swap);
        assert!(!details.cpu_temp && !details.gpu_temp && !details.ssd_temp);
        assert!(!details.memory_detail);
        assert!(!details.network);
    }
}
