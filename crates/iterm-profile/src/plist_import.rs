use crate::colors::ColorScheme;
use crate::keymap::{HotkeyConfig, KeyMap, OptionKeyMode};
use crate::semantic_history::SemanticHistoryConfig;
use crate::smart_selection::{
    default_smart_selection_rules, parse_iterm_plist_rules, SmartSelectionRule,
};
use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("failed to read file {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("failed to parse plist {0}: {1}")]
    Plist(PathBuf, #[source] plist::Error),
    #[error("failed to parse json {0}: {1}")]
    Json(PathBuf, #[source] serde_json::Error),
    #[error("plist root is not a dictionary")]
    InvalidRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CursorStyle {
    Underline = 0,
    VerticalBar = 1,
    #[default]
    Block = 2,
}

impl CursorStyle {
    pub fn from_iterm_int(v: i64) -> Self {
        match v {
            0 => Self::Underline,
            1 => Self::VerticalBar,
            _ => Self::Block,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FontSpec {
    /// PostScript or family name, e.g. `"FiraCode-Retina"` or `"Menlo-Regular"`.
    pub name: String,
    /// Point size, e.g. `20.0`.
    pub size: f32,
}

impl FontSpec {
    /// Parse iTerm2's `"FontName Size"` string (e.g. `"FiraCode-Retina 20"`, `"Menlo-Regular 14.5"`).
    pub fn parse_iterm(s: &str) -> Self {
        let trimmed = s.trim();
        if let Some((name, size_str)) = trimmed.rsplit_once(' ') {
            if let Ok(size) = size_str.parse::<f32>() {
                return Self {
                    name: name.trim().to_string(),
                    size: size.clamp(6.0, 144.0),
                };
            }
        }
        Self {
            name: if trimmed.is_empty() {
                "Menlo-Regular".to_string()
            } else {
                trimmed.to_string()
            },
            size: 14.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InitialDirectory {
    #[default]
    Home,
    Recycle,
    Custom(String),
}

/// Complete iTerm2-compatible session profile (`New Bookmarks` item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub guid: String,
    pub name: String,
    pub colors: ColorScheme,
    pub normal_font: FontSpec,
    pub non_ascii_font: FontSpec,
    pub use_non_ascii_font: bool,
    pub ascii_ligatures: bool,
    pub non_ascii_ligatures: bool,
    pub horizontal_spacing: f32,
    pub vertical_spacing: f32,
    pub use_bold_font: bool,
    pub use_bright_bold: bool,
    pub use_italic_font: bool,
    pub minimum_contrast: f32,
    pub columns: u16,
    pub rows: u16,
    pub unlimited_scrollback: bool,
    pub scrollback_lines: usize,
    pub scrollback_in_alternate_screen: bool,
    pub cursor_style: CursorStyle,
    pub blinking_cursor: bool,
    pub smart_cursor_color: bool,
    pub transparency: f32,
    pub blur: bool,
    pub blur_radius: f32,
    pub silence_bell: bool,
    pub visual_bell: bool,
    pub flashing_bell: bool,
    pub mouse_reporting: bool,
    pub option_key_sends: OptionKeyMode,
    pub right_option_key_sends: OptionKeyMode,
    pub terminal_type: String,
    pub custom_command: Option<String>,
    pub initial_directory: InitialDirectory,
    pub close_sessions_on_end: bool,
    pub disable_window_resizing: bool,
    pub ambiguous_double_width: bool,
    pub keymap: KeyMap,
    pub semantic_history: SemanticHistoryConfig,
    pub smart_selection_rules: Vec<SmartSelectionRule>,
    pub hotkey: Option<HotkeyConfig>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            guid: "DEFAULT-ITERM-MINIMAL".into(),
            name: "Default".into(),
            colors: ColorScheme::dracula_plus(),
            normal_font: FontSpec {
                name: "FiraCode-Retina".into(),
                size: 16.0,
            },
            non_ascii_font: FontSpec {
                name: "Monaco".into(),
                size: 16.0,
            },
            use_non_ascii_font: true,
            ascii_ligatures: true,
            non_ascii_ligatures: false,
            horizontal_spacing: 1.0,
            vertical_spacing: 1.0,
            use_bold_font: true,
            use_bright_bold: true,
            use_italic_font: true,
            minimum_contrast: 0.0,
            columns: 100,
            rows: 30,
            unlimited_scrollback: true,
            scrollback_lines: 100_000,
            scrollback_in_alternate_screen: true,
            cursor_style: CursorStyle::Block,
            blinking_cursor: false,
            smart_cursor_color: true,
            transparency: 0.0,
            blur: false,
            blur_radius: 2.0,
            silence_bell: true,
            visual_bell: true,
            flashing_bell: false,
            mouse_reporting: true,
            option_key_sends: OptionKeyMode::Normal,
            right_option_key_sends: OptionKeyMode::Normal,
            terminal_type: "xterm-256color".into(),
            custom_command: None,
            initial_directory: InitialDirectory::Recycle,
            close_sessions_on_end: true,
            disable_window_resizing: true,
            ambiguous_double_width: false,
            keymap: KeyMap::default_macos(),
            semantic_history: SemanticHistoryConfig::default(),
            smart_selection_rules: default_smart_selection_rules(),
            hotkey: None,
        }
    }
}

/// Top-level application preferences imported from `com.googlecode.iterm2.plist`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub default_profile_guid: Option<String>,
    pub profiles: Vec<Profile>,
    pub color_presets: HashMap<String, ColorScheme>,
    pub global_keymap: KeyMap,
    pub hotkey: HotkeyConfig,
    pub copy_selection: bool,
    pub copy_last_newline: bool,
    pub allow_clipboard_access: bool,
    pub word_characters: String,
    pub triple_click_selects_full_wrapped_lines: bool,
    pub dim_inactive_split_panes: bool,
    pub dim_background_windows: bool,
    pub dim_only_text: bool,
    pub split_pane_dimming_amount: f32,
    pub hide_scrollbar: bool,
    pub hide_tab_bar_when_single: bool,
    pub hide_tab_close_button: bool,
    pub show_pane_titles: bool,
    pub show_window_number: bool,
    pub show_job_name: bool,
    pub use_border: bool,
    pub prompt_on_quit: bool,
    pub quit_when_all_windows_closed: bool,
    pub adjust_window_for_font_size_change: bool,
    pub disable_fullscreen_transparency: bool,
    pub use_lion_style_fullscreen: bool,
    pub focus_follows_mouse: bool,
    pub download_dir: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let default_profile = Profile::default();
        let default_guid = default_profile.guid.clone();
        Self {
            default_profile_guid: Some(default_guid),
            profiles: vec![default_profile],
            color_presets: HashMap::new(),
            global_keymap: KeyMap::default(),
            hotkey: HotkeyConfig::default(),
            copy_selection: true,
            copy_last_newline: false,
            allow_clipboard_access: true,
            word_characters: "/-+\\~_.".into(),
            triple_click_selects_full_wrapped_lines: false,
            dim_inactive_split_panes: true,
            dim_background_windows: false,
            dim_only_text: false,
            split_pane_dimming_amount: 0.35,
            hide_scrollbar: true,
            hide_tab_bar_when_single: true,
            hide_tab_close_button: false,
            show_pane_titles: true,
            show_window_number: true,
            show_job_name: true,
            use_border: false,
            prompt_on_quit: true,
            quit_when_all_windows_closed: false,
            adjust_window_for_font_size_change: true,
            disable_fullscreen_transparency: false,
            use_lion_style_fullscreen: true,
            focus_follows_mouse: false,
            download_dir: PathBuf::from(home).join("Downloads"),
        }
    }
}

impl AppConfig {
    /// Return the active default profile (matching `Default Bookmark Guid` or first profile).
    pub fn default_profile(&self) -> &Profile {
        if let Some(guid) = &self.default_profile_guid {
            if let Some(p) = self.profiles.iter().find(|p| &p.guid == guid) {
                return p;
            }
        }
        self.profiles
            .first()
            .expect("AppConfig always retains at least one profile")
    }

    pub fn default_profile_mut(&mut self) -> &mut Profile {
        if let Some(ref guid) = self.default_profile_guid {
            if let Some(idx) = self.profiles.iter().position(|p| &p.guid == guid) {
                return &mut self.profiles[idx];
            }
        }
        self.profiles
            .first_mut()
            .expect("AppConfig always retains at least one profile")
    }

    pub fn profile_by_name(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn profile_by_guid(&self, guid: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.guid == guid)
    }

    /// Standard path for our cached native config JSON (`~/.config/jterm/config.json`).
    pub fn native_config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".config")
            .join("jterm")
            .join("config.json")
    }

    /// Legacy config path (`~/.config/iterm-minimal/config.json`) checked as seamless migration fallback.
    fn legacy_config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".config")
            .join("iterm-minimal")
            .join("config.json")
    }

    /// Standard macOS iTerm2 plist path (`~/Library/Preferences/com.googlecode.iterm2.plist`).
    pub fn standard_iterm2_plist_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join("Library")
            .join("Preferences")
            .join("com.googlecode.iterm2.plist")
    }

    /// Load configuration in priority order:
    /// 1. Explicit `--plist <path>` if provided
    /// 2. Cached `~/.config/jterm/config.json` (or legacy `~/.config/iterm-minimal/config.json`)
    /// 3. Automatic one-time import from `~/Library/Preferences/com.googlecode.iterm2.plist`
    /// 4. Built-in Dracula+ / FiraCode default profile
    pub fn load_auto(explicit_plist: Option<&Path>) -> Result<Self, ProfileError> {
        if let Some(p) = explicit_plist {
            return Self::from_iterm2_plist(p);
        }
        let native = Self::native_config_path();
        if native.exists() {
            return Self::from_json_file(&native);
        }
        let legacy = Self::legacy_config_path();
        if legacy.exists() {
            return Self::from_json_file(&legacy);
        }
        let iterm_plist = Self::standard_iterm2_plist_path();
        if iterm_plist.exists() {
            let mut cfg = Self::from_iterm2_plist(&iterm_plist)?;
            // Also merge any iTerm2 Dynamic Profiles if present
            let dyn_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join("Library/Application Support/iTerm2/DynamicProfiles");
            cfg.load_dynamic_profiles_dir(&dyn_dir);
            return Ok(cfg);
        }
        Ok(Self::default())
    }

    /// Save imported config as clean fast JSON (`~/.config/jterm/config.json`).
    pub fn save_json(&self, path: &Path) -> Result<(), ProfileError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ProfileError::Io(parent.to_path_buf(), e))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| ProfileError::Json(path.to_path_buf(), e))?;
        std::fs::write(path, text).map_err(|e| ProfileError::Io(path.to_path_buf(), e))
    }

    pub fn from_json_file(path: &Path) -> Result<Self, ProfileError> {
        let bytes = std::fs::read(path).map_err(|e| ProfileError::Io(path.to_path_buf(), e))?;
        serde_json::from_slice(&bytes).map_err(|e| ProfileError::Json(path.to_path_buf(), e))
    }

    /// Parse an iTerm2 `com.googlecode.iterm2.plist` (XML or binary plist).
    pub fn from_iterm2_plist(path: &Path) -> Result<Self, ProfileError> {
        let val = Value::from_file(path).map_err(|e| ProfileError::Plist(path.to_path_buf(), e))?;
        let dict = val.as_dictionary().ok_or(ProfileError::InvalidRoot)?;
        Ok(Self::from_iterm2_plist_dict(dict))
    }

    pub fn from_iterm2_plist_dict(dict: &Dictionary) -> Self {
        let mut cfg = Self::default();

        let get_bool = |k: &str, d: bool| dict.get(k).and_then(|v| v.as_boolean()).unwrap_or(d);
        let get_f32 = |k: &str, d: f32| dict.get(k).and_then(|v| v.as_real()).map(|v| v as f32).unwrap_or(d);
        let get_i64 = |k: &str, d: i64| dict.get(k).and_then(|v| v.as_signed_integer()).unwrap_or(d);
        let get_str = |k: &str| dict.get(k).and_then(|v| v.as_string()).map(ToOwned::to_owned);

        cfg.default_profile_guid = get_str("Default Bookmark Guid").map(|s| s.trim().to_string());
        cfg.copy_selection = get_bool("CopySelection", cfg.copy_selection);
        cfg.copy_last_newline = get_bool("CopyLastNewline", cfg.copy_last_newline);
        cfg.allow_clipboard_access = get_bool("AllowClipboardAccess", cfg.allow_clipboard_access);
        if let Some(wc) = get_str("WordCharacters") {
            cfg.word_characters = wc;
        }
        cfg.triple_click_selects_full_wrapped_lines = get_bool(
            "TripleClickSelectsFullWrappedLines",
            cfg.triple_click_selects_full_wrapped_lines,
        );
        cfg.dim_inactive_split_panes = get_bool("DimInactiveSplitPanes", cfg.dim_inactive_split_panes);
        cfg.dim_background_windows = get_bool("DimBackgroundWindows", cfg.dim_background_windows);
        cfg.dim_only_text = get_bool("DimOnlyText", cfg.dim_only_text);
        cfg.split_pane_dimming_amount = get_f32("SplitPaneDimmingAmount", cfg.split_pane_dimming_amount);
        cfg.hide_scrollbar = get_bool("HideScrollbar", cfg.hide_scrollbar);
        cfg.hide_tab_bar_when_single = get_bool("HideTab", cfg.hide_tab_bar_when_single)
            || get_bool("OnlyWhenMoreTabs", false);
        cfg.hide_tab_close_button = get_bool("HideTabCloseButton", cfg.hide_tab_close_button);
        cfg.show_pane_titles = get_bool("ShowPaneTitles", cfg.show_pane_titles);
        cfg.show_window_number = get_bool("WindowNumber", cfg.show_window_number);
        cfg.show_job_name = get_bool("JobName", cfg.show_job_name);
        cfg.use_border = get_bool("UseBorder", cfg.use_border);
        cfg.prompt_on_quit = get_bool("PromptOnQuit", cfg.prompt_on_quit);
        cfg.quit_when_all_windows_closed = get_bool(
            "QuitWhenAllWindowsClosed",
            cfg.quit_when_all_windows_closed,
        );
        cfg.adjust_window_for_font_size_change = get_bool(
            "AdjustWindowForFontSizeChange",
            cfg.adjust_window_for_font_size_change,
        );
        cfg.disable_fullscreen_transparency = get_bool(
            "DisableFullscreenTransparency",
            cfg.disable_fullscreen_transparency,
        );
        cfg.use_lion_style_fullscreen = get_bool(
            "UseLionStyleFullscreen",
            cfg.use_lion_style_fullscreen,
        );
        cfg.focus_follows_mouse = get_bool("FocusFollowsMouse", cfg.focus_follows_mouse);

        // Global hotkey
        cfg.hotkey = HotkeyConfig {
            enabled: get_bool("Hotkey", false),
            key_code: get_i64("HotkeyCode", 34) as u16,
            modifiers: get_i64("HotkeyModifiers", 0x80000) as u32,
            char_code: get_i64("HotkeyChar", 105) as u32,
            toggles_window: get_bool("HotKeyTogglesWindow", false),
            profile_guid: get_str("HotKeyBookmark"),
        };

        // Custom color presets
        if let Some(presets) = dict.get("Custom Color Presets").and_then(|v| v.as_dictionary()) {
            for (name, val) in presets {
                if let Some(cdict) = val.as_dictionary() {
                    cfg.color_presets
                        .insert(name.clone(), ColorScheme::from_iterm_plist_dict(cdict));
                }
            }
        }

        // GlobalKeyMap
        if let Some(gkm) = dict.get("GlobalKeyMap").and_then(|v| v.as_dictionary()) {
            cfg.global_keymap = KeyMap::from_iterm_plist_dict(gkm);
        }

        // Profiles (`New Bookmarks`)
        if let Some(bookmarks) = dict.get("New Bookmarks").and_then(|v| v.as_array()) {
            let mut imported = Vec::with_capacity(bookmarks.len());
            for item in bookmarks {
                if let Some(pdict) = item.as_dictionary() {
                    imported.push(Profile::from_iterm_plist_dict(pdict));
                }
            }
            if !imported.is_empty() {
                cfg.profiles = imported;
            }
        }

        cfg
    }

    /// Load a `.itermcolors` color preset file and apply it to the default profile.
    pub fn load_itermcolors(path: &Path) -> Result<ColorScheme, ProfileError> {
        let val = Value::from_file(path).map_err(|e| ProfileError::Plist(path.to_path_buf(), e))?;
        let dict = val.as_dictionary().ok_or(ProfileError::InvalidRoot)?;
        Ok(ColorScheme::from_iterm_plist_dict(dict))
    }

    /// Merge iTerm2 Dynamic Profiles JSON files from a directory.
    pub fn load_dynamic_profiles_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(arr) = val.get("Profiles").and_then(|v| v.as_array()) else {
                continue;
            };
            for item in arr {
                if let Some(obj) = item.as_object() {
                    let profile = Profile::from_iterm_json(obj);
                    if let Some(existing) = self.profiles.iter_mut().find(|p| p.guid == profile.guid)
                    {
                        *existing = profile;
                    } else {
                        self.profiles.push(profile);
                    }
                }
            }
        }
    }
}

impl Profile {
    pub fn from_iterm_plist_dict(dict: &Dictionary) -> Self {
        let mut p = Self::default();
        let get_bool = |k: &str, d: bool| dict.get(k).and_then(|v| v.as_boolean()).unwrap_or(d);
        let get_f32 = |k: &str, d: f32| dict.get(k).and_then(|v| v.as_real()).map(|v| v as f32).unwrap_or(d);
        let get_i64 = |k: &str, d: i64| dict.get(k).and_then(|v| v.as_signed_integer()).unwrap_or(d);
        let get_str = |k: &str| dict.get(k).and_then(|v| v.as_string());

        if let Some(g) = get_str("Guid") {
            p.guid = g.trim().to_string();
        }
        if let Some(n) = get_str("Name") {
            p.name = n.trim().to_string();
        }
        p.colors = ColorScheme::from_iterm_plist_dict(dict);
        if let Some(nf) = get_str("Normal Font") {
            p.normal_font = FontSpec::parse_iterm(nf);
        }
        if let Some(naf) = get_str("Non Ascii Font") {
            p.non_ascii_font = FontSpec::parse_iterm(naf);
        }
        p.use_non_ascii_font = get_bool("Use Non-ASCII Font", true);
        // Enable ligatures by default if FiraCode/JetBrainsMono/Cascadia/Iosevka or explicitly enabled
        let font_has_ligatures = p.normal_font.name.to_ascii_lowercase().contains("fira")
            || p.normal_font.name.to_ascii_lowercase().contains("jetbrains")
            || p.normal_font.name.to_ascii_lowercase().contains("cascadia")
            || p.normal_font.name.to_ascii_lowercase().contains("iosevka");
        p.ascii_ligatures = get_bool("ASCII Ligatures", font_has_ligatures);
        p.non_ascii_ligatures = get_bool("Non-ASCII Ligatures", false);
        p.horizontal_spacing = get_f32("Horizontal Spacing", 1.0).clamp(0.5, 2.0);
        p.vertical_spacing = get_f32("Vertical Spacing", 1.0).clamp(0.5, 2.5);
        p.use_bold_font = get_bool("Use Bold Font", true);
        p.use_bright_bold = get_bool("Use Bright Bold", true);
        p.use_italic_font = get_bool("Use Italic Font", true);
        p.minimum_contrast = get_f32("Minimum Contrast", 0.0).clamp(0.0, 1.0);
        p.columns = get_i64("Columns", 100).clamp(20, 1000) as u16;
        p.rows = get_i64("Rows", 30).clamp(5, 500) as u16;
        p.unlimited_scrollback = get_bool("Unlimited Scrollback", true);
        let sb = get_i64("Scrollback Lines", 100_000).max(0) as usize;
        p.scrollback_lines = if p.unlimited_scrollback {
            // Cap in-memory unlimited scrollback at 1,000,000 lines (~120 MB worst case)
            // to guarantee bounded RAM while feeling unlimited.
            1_000_000
        } else {
            sb.max(1000)
        };
        p.scrollback_in_alternate_screen = get_bool("Scrollback in Alternate Screen", true);
        p.cursor_style = CursorStyle::from_iterm_int(get_i64("Cursor Type", 2));
        p.blinking_cursor = get_bool("Blinking Cursor", false);
        p.smart_cursor_color = get_bool("Smart Cursor Color", true);
        p.transparency = get_f32("Transparency", 0.0).clamp(0.0, 0.95);
        p.blur = get_bool("Blur", false);
        p.blur_radius = get_f32("Blur Radius", 2.0).clamp(0.0, 40.0);
        p.silence_bell = get_bool("Silence Bell", true);
        p.visual_bell = get_bool("Visual Bell", true);
        p.flashing_bell = get_bool("Flashing Bell", false);
        p.mouse_reporting = get_bool("Mouse Reporting", true);
        p.option_key_sends = OptionKeyMode::from_iterm_int(get_i64("Option Key Sends", 0));
        p.right_option_key_sends =
            OptionKeyMode::from_iterm_int(get_i64("Right Option Key Sends", 0));
        if let Some(tt) = get_str("Terminal Type") {
            if !tt.is_empty() {
                p.terminal_type = tt.to_string();
            }
        }
        if get_str("Custom Command") == Some("Yes") {
            if let Some(cmd) = get_str("Command").filter(|s| !s.is_empty()) {
                p.custom_command = Some(cmd.to_string());
            }
        }
        p.initial_directory = match get_str("Custom Directory") {
            Some("Recycle") => InitialDirectory::Recycle,
            Some("Yes") => InitialDirectory::Custom(
                get_str("Working Directory").unwrap_or("").to_string(),
            ),
            _ => InitialDirectory::Home,
        };
        p.close_sessions_on_end = get_bool("Close Sessions On End", true);
        p.disable_window_resizing = get_bool("Disable Window Resizing", true);
        p.ambiguous_double_width = get_bool("Ambiguous Double Width", false);

        if let Some(km) = dict.get("Keyboard Map").and_then(|v| v.as_dictionary()) {
            p.keymap = KeyMap::from_iterm_plist_dict(km);
        }
        if let Some(sh) = dict.get("Semantic History").and_then(|v| v.as_dictionary()) {
            p.semantic_history = SemanticHistoryConfig::from_iterm_plist_dict(sh);
        }
        if let Some(rules) = dict
            .get("Smart Selection Rules")
            .and_then(|v| v.as_array())
        {
            let parsed = parse_iterm_plist_rules(rules);
            if !parsed.is_empty() {
                p.smart_selection_rules = parsed;
            }
        }
        if get_bool("Has Hotkey", false) {
            p.hotkey = Some(HotkeyConfig {
                enabled: true,
                key_code: get_i64("HotKey Key Code", 34) as u16,
                modifiers: get_i64("HotKey Modifier Flags", 0x80000) as u32,
                char_code: get_i64("HotKey Characters", 0) as u32,
                toggles_window: true,
                profile_guid: Some(p.guid.clone()),
            });
        }
        p
    }

    pub fn from_iterm_json(obj: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut p = Self::default();
        if let Some(g) = obj.get("Guid").and_then(|v| v.as_str()) {
            p.guid = g.to_string();
        }
        if let Some(n) = obj.get("Name").and_then(|v| v.as_str()) {
            p.name = n.to_string();
        }
        p.colors = ColorScheme::from_iterm_json(obj);
        if let Some(nf) = obj.get("Normal Font").and_then(|v| v.as_str()) {
            p.normal_font = FontSpec::parse_iterm(nf);
        }
        if let Some(naf) = obj.get("Non Ascii Font").and_then(|v| v.as_str()) {
            p.non_ascii_font = FontSpec::parse_iterm(naf);
        }
        if let Some(b) = obj.get("ASCII Ligatures").and_then(|v| v.as_bool()) {
            p.ascii_ligatures = b;
        }
        if let Some(c) = obj.get("Columns").and_then(|v| v.as_u64()) {
            p.columns = c.clamp(20, 1000) as u16;
        }
        if let Some(r) = obj.get("Rows").and_then(|v| v.as_u64()) {
            p.rows = r.clamp(5, 500) as u16;
        }
        if let Some(t) = obj.get("Transparency").and_then(|v| v.as_f64()) {
            p.transparency = (t as f32).clamp(0.0, 0.95);
        }
        if let Some(km) = obj.get("Keyboard Map").and_then(|v| v.as_object()) {
            p.keymap = KeyMap::from_iterm_json(km);
        }
        p
    }
}
