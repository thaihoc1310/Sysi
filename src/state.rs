use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    Light,
    Dark,
    /// Liquid glass drawn by the GNOME Shell extension under a clear card,
    /// with white text the glass dims itself to keep legible. The removed
    /// AUTO and INVERT modes (and GRAY before them) were the see-through ones,
    /// so they load as this.
    #[default]
    #[serde(alias = "auto", alias = "invert", alias = "gray")]
    Glass,
}

impl ColorMode {
    pub fn next(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Glass,
            Self::Glass => Self::Light,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "LIGHT",
            Self::Dark => "DARK",
            Self::Glass => "GLASS",
        }
    }

    /// What the GNOME panel prints on its colour-mode button, and what it
    /// writes into the shared panel-state file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Glass => "glass",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VoiceModel {
    #[default]
    GeminiFlashLite,
    GeminiFlashLiteLatest,
    GeminiTranscribe,
    GroqWhisper,
}

impl VoiceModel {
    pub const ALL: [VoiceModel; 4] = [
        VoiceModel::GeminiFlashLite,
        VoiceModel::GeminiFlashLiteLatest,
        VoiceModel::GeminiTranscribe,
        VoiceModel::GroqWhisper,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::GeminiFlashLite => "gemini-flash-lite",
            Self::GeminiFlashLiteLatest => "gemini-flash-lite-latest",
            Self::GeminiTranscribe => "gemini-transcribe",
            Self::GroqWhisper => "groq-whisper",
        }
    }

    /// The short name the settings menu and the voice HUD show.
    pub fn label(self) -> &'static str {
        match self {
            Self::GeminiFlashLite => "3.5-fl",
            Self::GeminiFlashLiteLatest => "fl-latest",
            Self::GeminiTranscribe => "3.5-trans",
            Self::GroqWhisper => "groq",
        }
    }

    /// The Gemini model behind this choice; Groq's Whisper has none.
    pub fn gemini_model(self) -> Option<&'static str> {
        match self {
            Self::GeminiFlashLite => Some("gemini-3.5-flash-lite"),
            Self::GeminiFlashLiteLatest => Some("gemini-flash-lite-latest"),
            Self::GeminiTranscribe => Some("gemini-3.5-transcribe"),
            Self::GroqWhisper => None,
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "gemini-flash-lite" | "flash-lite" | "3.5-flash-lite" | "gemini-3.5-flash-lite" => {
                Some(Self::GeminiFlashLite)
            }
            "gemini-flash-lite-latest" | "flash-lite-latest" | "lite-latest" => {
                Some(Self::GeminiFlashLiteLatest)
            }
            "gemini-transcribe" | "transcribe" | "3.5-transcribe" | "gemini-3.5-transcribe" => {
                Some(Self::GeminiTranscribe)
            }
            "groq" | "groq-whisper" | "whisper" | "whisper-large-v3" => Some(Self::GroqWhisper),
            _ => None,
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
    pub settings_button: bool,
    #[serde(default)]
    pub translate_open: bool,
    #[serde(default)]
    pub usage_open: bool,
    #[serde(default)]
    pub sessions_open: bool,
    #[serde(default = "default_usage_source")]
    pub usage_source: String,
    #[serde(default = "default_usage_period")]
    pub usage_period: String,
    #[serde(default)]
    pub color_mode: ColorMode,
    /// What every note's highlighter is loaded with. One pen for the whole
    /// desk: picking a colour in one note is picking it everywhere.
    #[serde(default)]
    pub highlight_color: HighlightColor,
    #[serde(default = "default_font_size")]
    pub font_size: i32,
    #[serde(default)]
    pub system_details: SystemDetails,
    #[serde(default)]
    pub voice_model: VoiceModel,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            system: true,
            settings_button: true,
            translate_open: false,
            usage_open: false,
            sessions_open: false,
            usage_source: default_usage_source(),
            usage_period: default_usage_period(),
            color_mode: ColorMode::default(),
            highlight_color: HighlightColor::default(),
            font_size: default_font_size(),
            system_details: SystemDetails::default(),
            voice_model: VoiceModel::default(),
        }
    }
}

/// Which readings SYSTEM shows in the top bar (see `panel_system`).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SystemDetails {
    #[serde(default = "default_true")]
    pub cpu: bool,
    #[serde(default = "default_true")]
    pub ram: bool,
    #[serde(default)]
    pub swap: bool,
    #[serde(default)]
    pub gpus: bool,
    #[serde(default)]
    pub cpu_temp: bool,
    #[serde(default)]
    pub gpu_temp: bool,
    #[serde(default)]
    pub ssd_temp: bool,
    /// Each GPU's own video memory, used over total.
    #[serde(default)]
    pub gpu_memory: bool,
    /// How full each drive is.
    #[serde(default)]
    pub ssd_usage: bool,
    #[serde(default)]
    pub network: bool,
    /// What the machine draws, in watts.
    #[serde(default)]
    pub power: bool,
    /// How long the battery lasts, or takes to fill.
    #[serde(default)]
    pub battery_time: bool,
    /// RAM, swap and drives as used over total (`12G/16G`) rather than a
    /// percentage.
    #[serde(default)]
    pub amounts: bool,
}

impl Default for SystemDetails {
    fn default() -> Self {
        Self {
            cpu: true,
            ram: true,
            swap: false,
            gpus: false,
            cpu_temp: false,
            gpu_temp: false,
            ssd_temp: false,
            gpu_memory: false,
            power: false,
            battery_time: false,
            ssd_usage: false,
            amounts: false,
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

/// What a highlighter can be loaded with. Four is enough to sort one note's
/// ideas apart and few enough that the menu stays one glance tall.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HighlightColor {
    #[default]
    Yellow,
    Green,
    // Pink until it was swapped for red; old notes still say "pink".
    #[serde(alias = "pink")]
    Red,
    Blue,
}

impl HighlightColor {
    pub const ALL: [Self; 4] = [Self::Yellow, Self::Green, Self::Red, Self::Blue];

    pub fn label(self) -> &'static str {
        match self {
            Self::Yellow => "YELLOW",
            Self::Green => "GREEN",
            Self::Red => "RED",
            Self::Blue => "BLUE",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Red => "red",
            Self::Blue => "blue",
        }
    }

    /// The wash itself. Translucent on purpose: the note's own background
    /// shows through, so one set of colours reads on LIGHT, DARK and GLASS
    /// alike and nothing has to be repainted when a note changes mode.
    /// Each alpha is as bright as the wash goes while light text on a DARK
    /// plate still keeps 4.5:1 contrast; yellow is the one that runs out first.
    pub fn rgba(self) -> (f64, f64, f64, f64) {
        match self {
            Self::Yellow => (1.00, 0.84, 0.16, 0.46),
            Self::Green => (0.30, 0.90, 0.42, 0.48),
            Self::Red => (1.00, 0.26, 0.24, 0.60),
            Self::Blue => (0.30, 0.66, 1.00, 0.56),
        }
    }

    /// The same colour with the wash taken off, for the dot beside a menu row.
    /// At 40% alpha over an unknown menu background a swatch reads as grey.
    pub fn swatch(self) -> &'static str {
        match self {
            Self::Yellow => "#e8b41f",
            Self::Green => "#3fbf5f",
            Self::Red => "#ef4444",
            Self::Blue => "#4aa3f5",
        }
    }
}

/// One stretch of highlighted text, in characters from the start of the note.
///
/// The offsets are never maintained by hand: GTK moves a tag with the text it
/// covers, so these are read back out of the buffer when the note is saved and
/// applied again when it is opened.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NoteHighlight {
    pub start: i32,
    pub end: i32,
    #[serde(default)]
    pub color: HighlightColor,
}

/// One stretch of pasted text that kept the look it was copied in: a colour,
/// bold, italic or faint. Kept the way highlights are (see `NoteHighlight`):
/// GTK moves the tag with the words, and these are read back out on save.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct NoteInk {
    pub start: i32,
    pub end: i32,
    /// As copied, saved as `#rrggbb`. Drawn lighter or darker to read on the
    /// note. Held as bytes: the undo history keeps a copy of every stretch
    /// per step.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "write_ink_color",
        deserialize_with = "read_ink_color"
    )]
    pub color: Option<[u8; 3]>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dim: bool,
}

pub fn hex_color(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// `#rrggbb` back to its channels.
pub fn parse_hex_color(text: &str) -> Option<[u8; 3]> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

fn write_ink_color<S: serde::Serializer>(
    color: &Option<[u8; 3]>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    color.map(hex_color).serialize(serializer)
}

/// A colour that does not read is dropped, never the state file it is in.
fn read_ink_color<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u8; 3]>, D::Error> {
    let text = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(text
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .and_then(parse_hex_color))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Note {
    pub id: u64,
    pub text: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub position: Point,
    #[serde(default)]
    pub images: Vec<NoteImage>,
    #[serde(default)]
    pub highlights: Vec<NoteHighlight>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ink: Vec<NoteInk>,
    /// The tags it was given in Notes, in the order they were added.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The longest a tag may be: it has to sit on a row's meta line beside the
/// age and still leave room for another.
pub const TAG_MAX_CHARS: usize = 24;

/// A tag as typed, made one: no `#` in front, no space around it, runs of
/// space as one, at most `TAG_MAX_CHARS`. `None` if nothing is left.
pub fn clean_tag(raw: &str) -> Option<String> {
    let words: Vec<&str> = raw
        .trim()
        .trim_start_matches('#')
        .split_whitespace()
        .collect();
    let tag: String = words.join(" ").chars().take(TAG_MAX_CHARS).collect();
    let tag = tag.trim_end().to_owned();
    (!tag.is_empty()).then_some(tag)
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
    pub widget_font_sizes: HashMap<String, i32>,
    #[serde(default)]
    pub notes: Vec<Note>,
    /// The dictionary windows that exist, in the order they were opened.
    #[serde(default)]
    pub dictionaries: Vec<DictionaryWindow>,
    /// The last few dictionary queries, most recent first.
    #[serde(default)]
    pub recent_searches: Vec<String>,
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
            widget_font_sizes: HashMap::new(),
            notes: Vec::new(),
            dictionaries: Vec::new(),
            recent_searches: Vec::new(),
            next_note_id: 1,
            next_dictionary_id: 1,
            next_image_id: 1,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_usage_source() -> String {
    "codex".to_owned()
}

fn default_usage_period() -> String {
    "30d".to_owned()
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

// Pasted images and the token ledger are user data, not a cache: losing them
// cannot be undone, so they go under XDG_DATA_HOME rather than the cache dir.
pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sysi")
}

pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sysi")
}

pub fn default_font_size() -> i32 {
    13
}

impl Note {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }
}

impl AppState {
    pub fn font_size(&self, key: &str) -> i32 {
        self.widget_font_sizes
            .get(key)
            .copied()
            .unwrap_or(self.settings.font_size)
            .clamp(8, 26)
    }

    pub fn change_font_size(&mut self, key: Option<&str>, delta: i32) {
        if let Some(key) = key {
            self.widget_font_sizes
                .insert(key.to_owned(), (self.font_size(key) + delta).clamp(8, 26));
        } else {
            self.settings.font_size = (self.settings.font_size.clamp(8, 26) + delta).clamp(8, 26);
            self.widget_font_sizes.clear();
        }
    }

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

    /// Every tag some note has, with how many notes have it: the most used
    /// first, then by name. Counted from the notes themselves, so a tag no
    /// note has any more is gone rather than left at zero.
    pub fn tag_counts(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for tag in self.notes.iter().flat_map(|note| note.tags.iter()) {
            match counts.iter_mut().find(|(name, _)| name == tag) {
                Some((_, count)) => *count += 1,
                None => counts.push((tag.clone(), 1)),
            }
        }
        counts.sort_by(|(a, a_count), (b, b_count)| {
            b_count
                .cmp(a_count)
                .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
        });
        counts
    }

    /// Give a note a tag, or take it off if it has it. A new name that
    /// matches a tag already in use, but for case, joins that tag. Returns
    /// whether the note has the tag now. Tagging is not an edit: the note's
    /// age and place in the list stay as they were.
    pub fn toggle_note_tag(&mut self, id: u64, raw: &str) -> bool {
        let Some(tag) = clean_tag(raw) else {
            return false;
        };
        let tag = self
            .notes
            .iter()
            .flat_map(|note| note.tags.iter())
            .find(|known| known.to_lowercase() == tag.to_lowercase())
            .cloned()
            .unwrap_or(tag);
        let Some(note) = self.notes.iter_mut().find(|note| note.id == id) else {
            return false;
        };
        if let Some(index) = note.tags.iter().position(|known| *known == tag) {
            note.tags.remove(index);
            false
        } else {
            note.tags.push(tag);
            true
        }
    }

    /// Take a tag off every note that has it, and say how many had it. Like
    /// tagging, it is not an edit of the notes.
    pub fn delete_tag(&mut self, tag: &str) -> usize {
        let mut had = 0;
        for note in &mut self.notes {
            let before = note.tags.len();
            note.tags.retain(|known| known != tag);
            had += before - note.tags.len();
        }
        had
    }

    /// Give a tag a new name on every note, and return the name it ends up
    /// with. A name another tag has already (but for case) merges the two:
    /// a note that had both keeps the tag once, where the first of them was.
    /// `None` if the new name is empty once cleaned.
    pub fn rename_tag(&mut self, tag: &str, raw: &str) -> Option<String> {
        let name = clean_tag(raw)?;
        let name = self
            .notes
            .iter()
            .flat_map(|note| note.tags.iter())
            .find(|known| *known != tag && known.to_lowercase() == name.to_lowercase())
            .cloned()
            .unwrap_or(name);
        for note in &mut self.notes {
            let mut seen = false;
            note.tags.retain_mut(|known| {
                if *known == tag {
                    *known = name.clone();
                }
                if *known != name {
                    return true;
                }
                !std::mem::replace(&mut seen, true)
            });
        }
        Some(name)
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
    use super::{clean_tag, AppState, ColorMode, HighlightColor, Note, NoteInk};

    #[test]
    fn tags_are_counted_from_the_notes_and_toggle_on_and_off() {
        let mut state: AppState = serde_json::from_str(
            r#"{"notes":[{"id":1,"text":"a"},{"id":2,"text":"b"},{"id":3,"text":"c"}]}"#,
        )
        .unwrap();
        assert!(state.toggle_note_tag(1, "  #speaking   part 2 "));
        assert!(state.toggle_note_tag(2, "vocab"));
        assert!(state.toggle_note_tag(3, "Vocab"));
        // Case aside, a name in use joins its tag rather than making a second.
        assert_eq!(state.notes[2].tags, ["vocab"]);
        assert_eq!(
            state.tag_counts(),
            [("vocab".to_owned(), 2), ("speaking part 2".to_owned(), 1)]
        );
        // Given again, it comes off; the last note off it takes the tag away.
        assert!(!state.toggle_note_tag(1, "speaking part 2"));
        assert_eq!(state.tag_counts(), [("vocab".to_owned(), 2)]);
        assert!(!state.toggle_note_tag(9, "vocab"));
        // Deleted, a tag leaves every note it was on, and only that tag.
        assert!(state.toggle_note_tag(3, "ielts"));
        assert_eq!(state.delete_tag("vocab"), 2);
        assert_eq!(state.tag_counts(), [("ielts".to_owned(), 1)]);
        assert_eq!(state.delete_tag("vocab"), 0);
        // Renamed, a tag keeps its notes; onto a name in use, the two merge
        // and a note with both keeps one, where the first was.
        assert!(state.toggle_note_tag(1, "writing"));
        assert!(state.toggle_note_tag(1, "ielts"));
        assert_eq!(
            state.rename_tag("ielts", " IELTS 7 "),
            Some("IELTS 7".to_owned())
        );
        assert_eq!(state.notes[0].tags, ["writing", "IELTS 7"]);
        assert_eq!(
            state.rename_tag("IELTS 7", "Writing"),
            Some("writing".to_owned())
        );
        assert_eq!(state.notes[0].tags, ["writing"]);
        assert_eq!(state.notes[2].tags, ["writing"]);
        assert_eq!(state.rename_tag("writing", " # "), None);
        assert_eq!(state.tag_counts(), [("writing".to_owned(), 2)]);
        assert!(!state.toggle_note_tag(1, " # "));
        assert_eq!(clean_tag(&"x".repeat(40)).map(|tag| tag.len()), Some(24));
    }

    #[test]
    fn a_pink_highlight_saved_before_red_loads_as_red() {
        let old: HighlightColor = serde_json::from_str("\"pink\"").unwrap();
        assert_eq!(old, HighlightColor::Red);
        assert_eq!(serde_json::to_string(&old).unwrap(), "\"red\"");
    }

    #[test]
    fn a_notes_ink_saves_compactly_and_a_bad_colour_loses_only_itself() {
        let ink = NoteInk {
            start: 2,
            end: 5,
            color: Some([0x09, 0x69, 0xda]),
            bold: true,
            ..NoteInk::default()
        };
        assert_eq!(
            serde_json::to_string(&ink).unwrap(),
            r##"{"start":2,"end":5,"color":"#0969da","bold":true}"##
        );
        let note: Note = serde_json::from_str(
            r##"{"id":1,"text":"hello","ink":[{"start":0,"end":2,"color":"teal","dim":true},{"start":2,"end":4,"color":7}]}"##,
        )
        .unwrap();
        assert_eq!(note.ink[0].color, None);
        assert!(note.ink[0].dim);
        assert_eq!(note.ink[1].color, None);
        // A note without any is saved without the field.
        let plain: Note = serde_json::from_str(r#"{"id":2,"text":"x"}"#).unwrap();
        assert!(!serde_json::to_string(&plain).unwrap().contains("ink"));
    }

    #[test]
    fn font_overrides_reset_on_global_change_and_survive_save() {
        let mut state: AppState = serde_json::from_str("{}").unwrap();
        assert_eq!(state.font_size("note:1"), 13);
        state.change_font_size(Some("note:1"), 2);
        assert_eq!(state.font_size("note:1"), 15);
        assert_eq!(state.font_size("timer"), 13);
        let mut state: AppState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(state.font_size("note:1"), 15);
        state.change_font_size(None, -1);
        assert_eq!(state.font_size("note:1"), 12);
        assert!(state.widget_font_sizes.is_empty());
        state.change_font_size(None, -100);
        assert_eq!(state.font_size("timer"), 8);
        state.change_font_size(Some("timer"), 100);
        assert_eq!(state.font_size("timer"), 26);
    }

    #[test]
    fn only_contentless_notes_are_empty() {
        let mut note: super::Note = serde_json::from_str(r#"{"id":1,"text":" \n \t "}"#).unwrap();
        assert!(note.is_empty());
        note.text = "Untitled".into();
        assert!(!note.is_empty());
        note.text.clear();
        note.images.push(super::NoteImage {
            file: "image.png".into(),
            width: 1,
            height: 1,
        });
        assert!(!note.is_empty());
    }

    #[test]
    fn old_settings_default_to_glass_mode() {
        let state: AppState = serde_json::from_str(
            r#"{"settings":{"mascot":true,"system":true,"timer":true,"settings_button":true}}"#,
        )
        .expect("legacy state should remain readable");
        assert_eq!(state.settings.color_mode, ColorMode::Glass);
    }

    #[test]
    fn color_mode_cycles_through_every_mode() {
        assert_eq!(ColorMode::Light.next(), ColorMode::Dark);
        assert_eq!(ColorMode::Dark.next(), ColorMode::Glass);
        assert_eq!(ColorMode::Glass.next(), ColorMode::Light);
    }

    #[test]
    fn removed_see_through_modes_migrate_to_glass() {
        for old in ["gray", "auto", "invert"] {
            let state: AppState = serde_json::from_str(&format!(
                r#"{{"settings":{{"color_mode":"{old}"}},"widget_color_modes":{{"note:1":"{old}"}}}}"#
            ))
            .expect("a removed mode should remain readable");
            assert_eq!(state.settings.color_mode, ColorMode::Glass);
            assert_eq!(state.widget_color_modes["note:1"], ColorMode::Glass);
            assert!(serde_json::to_string(&state)
                .expect("migrated state should serialize")
                .contains(r#""color_mode":"glass""#));
        }
    }

    #[test]
    fn old_state_defaults_to_no_widget_color_overrides() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"color_mode":"dark"}}"#)
            .expect("state without per-widget colors should remain readable");
        assert!(state.widget_color_modes.is_empty());
    }

    #[test]
    fn old_state_defaults_to_a_closed_translate_window() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"system":true}}"#)
            .expect("state without a translate flag should remain readable");
        assert!(!state.settings.translate_open);
    }

    #[test]
    fn old_state_defaults_to_a_closed_usage_window() {
        let state: AppState = serde_json::from_str(r#"{"settings":{"system":true}}"#)
            .expect("state without a usage flag should remain readable");
        assert!(!state.settings.usage_open);
        assert!(!state.settings.sessions_open);
        assert_eq!(state.settings.usage_source, "codex");
        assert_eq!(state.settings.usage_period, "30d");
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
    fn state_from_before_the_timer_moved_to_the_top_bar_still_loads() {
        let state: AppState = serde_json::from_str(
            r#"{"settings":{"timer":false},"timer_seconds":900,"timer_style":"digital","next_note_id":7}"#,
        )
        .expect("the desk timer's old fields are ignored");
        assert_eq!(state.next_note_id, 7);
    }

    #[test]
    fn a_state_saved_before_the_new_sensors_keeps_the_sections_it_had() {
        // What a card with the disk meters on used to save. The sections it
        // never knew about have to come back off rather than switch themselves
        // on for someone who never asked, and the ones SYSTEM no longer has
        // (processes, cores, the / and /home disks) are simply dropped.
        let state: AppState = serde_json::from_str(
            r#"{"settings":{"system_details":{"cpu":true,"ram":true,"gpus":true,"root_disk":true,"home_disk":true,"processes":false,"cores":false}}}"#,
        )
        .expect("settings saved before the new sensors should remain readable");
        let details = state.settings.system_details;
        assert!(details.cpu && details.ram && details.gpus);
        assert!(!details.ssd_usage && !details.amounts);
        assert!(!details.swap);
        assert!(!details.cpu_temp && !details.gpu_temp && !details.ssd_temp);
        assert!(!details.network);
    }
}
