use plist::Dictionary;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// macOS AppKit modifier flag bitmasks used in iTerm2 `Keyboard Map` keys.
pub mod modifiers {
    pub const SHIFT: u32 = 1 << 17;       // 0x020000 NSEventModifierFlagShift
    pub const CONTROL: u32 = 1 << 18;     // 0x040000 NSEventModifierFlagControl
    pub const OPTION: u32 = 1 << 19;      // 0x080000 NSEventModifierFlagOption
    pub const COMMAND: u32 = 1 << 20;     // 0x100000 NSEventModifierFlagCommand
    pub const NUMERIC_PAD: u32 = 1 << 21; // 0x200000 NSEventModifierFlagNumericPad
    pub const RELEVANT_MASK: u32 = SHIFT | CONTROL | OPTION | COMMAND | NUMERIC_PAD;
}

/// Option key behavior matching iTerm2's `Option Key Sends` / `Right Option Key Sends`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OptionKeyMode {
    #[default]
    Normal = 0,
    Meta = 1,
    EscPlus = 2,
}

impl OptionKeyMode {
    pub fn from_iterm_int(v: i64) -> Self {
        match v {
            1 => Self::Meta,
            2 => Self::EscPlus,
            _ => Self::Normal,
        }
    }
}

/// Global hotkey window configuration matching iTerm2's top-level & profile hotkey keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub key_code: u16,
    pub modifiers: u32,
    pub char_code: u32,
    pub toggles_window: bool,
    pub profile_guid: Option<String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key_code: 34,          // 'i'
            modifiers: modifiers::OPTION,
            char_code: 'i' as u32,
            toggles_window: false,
            profile_guid: None,
        }
    }
}

/// Key combination in an iTerm2 `Keyboard Map` or `GlobalKeyMap`.
///
/// iTerm2 encodes keys as `"0x<unicode_or_special>-0x<modifier_flags>"` or
/// `"0x<unicode>-0x<modifier_flags>-0x<virtual_keycode>"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyCombo {
    pub key_code: u32,
    pub modifiers: u32,
    pub virtual_key: Option<u16>,
}

impl KeyCombo {
    pub fn parse_iterm_key(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return None;
        }
        let parse_hex = |p: &str| {
            let trimmed = p.trim();
            if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                trimmed.parse::<u32>().ok()
            }
        };
        let key_code = parse_hex(parts[0])?;
        let mods = parse_hex(parts[1])? & modifiers::RELEVANT_MASK;
        let virtual_key = if parts.len() == 3 {
            parse_hex(parts[2]).map(|v| v as u16)
        } else {
            None
        };
        Some(Self {
            key_code,
            modifiers: mods,
            virtual_key,
        })
    }
}

/// Retained iTerm2 key actions (exact numeric tags from `iTermKeyBindingAction.h`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    NextSession,
    NextWindow,
    PreviousSession,
    PreviousWindow,
    ScrollEnd,
    ScrollHome,
    ScrollLineDown,
    ScrollLineUp,
    ScrollPageDown,
    ScrollPageUp,
    SendEscapeSequence(String),
    SendHexCode(Vec<u8>),
    SendText(String),
    Ignore,
    SendControlHBackspace,
    SendControlQuestionBackspace,
    SelectPaneLeft,
    SelectPaneRight,
    SelectPaneAbove,
    SelectPaneBelow,
    ToggleFullscreen,
    SelectMenuItem(String),
    NewWindowWithProfile(String),
    NewTabWithProfile(String),
    SplitHorizontallyWithProfile(String),
    SplitVerticallyWithProfile(String),
    NextPane,
    PreviousPane,
    MoveTabLeft,
    MoveTabRight,
    FindAgainDown,
    FindAgainUp,
    ToggleMouseReporting,
    PasteSpecial,
    SwapPaneLeft,
    SwapPaneRight,
    SwapPaneAbove,
    SwapPaneBelow,
    DuplicateTab,
    Bypass,
    Unsupported(i32),
}

impl KeyAction {
    /// Parse an iTerm2 Keyboard Map action entry `{ "Action": <int>, "Text": <string> }`.
    pub fn from_iterm_parts(action: i32, text: &str, escaping: i32) -> Self {
        match action {
            0 => Self::NextSession,
            1 => Self::NextWindow,
            2 => Self::PreviousSession,
            3 => Self::PreviousWindow,
            4 => Self::ScrollEnd,
            5 => Self::ScrollHome,
            6 => Self::ScrollLineDown,
            7 => Self::ScrollLineUp,
            8 => Self::ScrollPageDown,
            9 => Self::ScrollPageUp,
            10 => Self::SendEscapeSequence(text.to_string()),
            11 => Self::SendHexCode(parse_hex_sequence(text)),
            12 => Self::SendText(expand_send_text(text, escaping)),
            13 => Self::Ignore,
            16 => Self::SendControlHBackspace,
            17 => Self::SendControlQuestionBackspace,
            18 => Self::SelectPaneLeft,
            19 => Self::SelectPaneRight,
            20 => Self::SelectPaneAbove,
            21 => Self::SelectPaneBelow,
            23 => Self::ToggleFullscreen,
            25 => Self::SelectMenuItem(text.to_string()),
            26 => Self::NewWindowWithProfile(text.to_string()),
            27 => Self::NewTabWithProfile(text.to_string()),
            28 => Self::SplitHorizontallyWithProfile(text.to_string()),
            29 => Self::SplitVerticallyWithProfile(text.to_string()),
            30 => Self::NextPane,
            31 => Self::PreviousPane,
            33 => Self::MoveTabLeft,
            34 => Self::MoveTabRight,
            41 => Self::PasteSpecial,
            53 => Self::SwapPaneLeft,
            54 => Self::SwapPaneRight,
            55 => Self::SwapPaneAbove,
            56 => Self::SwapPaneBelow,
            57 => Self::FindAgainDown,
            58 => Self::FindAgainUp,
            59 => Self::ToggleMouseReporting,
            61 => Self::DuplicateTab,
            74 => Self::Bypass,
            other => Self::Unsupported(other),
        }
    }

    /// If this action directly emits bytes to the PTY, return them.
    pub fn to_pty_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::SendEscapeSequence(seq) => {
                let mut out = Vec::with_capacity(1 + seq.len());
                out.push(0x1B);
                out.extend_from_slice(seq.as_bytes());
                Some(out)
            }
            Self::SendHexCode(bytes) => Some(bytes.clone()),
            Self::SendText(s) => Some(s.as_bytes().to_vec()),
            Self::SendControlHBackspace => Some(vec![0x08]),
            Self::SendControlQuestionBackspace => Some(vec![0x7F]),
            _ => None,
        }
    }
}

/// Parse space-separated hex codes such as `"0x1b 0x7f"` or `"0x15"` or `"1b 5b 41"`.
pub fn parse_hex_sequence(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let hex = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
            .unwrap_or(token);
        if let Ok(byte) = u8::from_str_radix(hex, 16) {
            out.push(byte);
        }
    }
    out
}

/// Expand iTerm2 escape sequences in SendText (`\n`, `\r`, `\t`, `\e`, `\a`, `\\`).
fn expand_send_text(text: &str, escaping: i32) -> String {
    if escaping == 0 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('e') => out.push('\x1b'),
                Some('a') => out.push('\x07'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parsed iTerm2 keymap table with O(1) lookup by `(key_code, modifiers)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyMap {
    /// Exact `(key_code, modifiers, virtual_key)` bindings.
    exact: HashMap<KeyCombo, KeyAction>,
    /// Fallback `(key_code, modifiers)` bindings (without virtual key specificity).
    generic: HashMap<(u32, u32), KeyAction>,
}

impl KeyMap {
    pub fn insert(&mut self, combo: KeyCombo, action: KeyAction) {
        if combo.virtual_key.is_some() {
            self.exact.insert(combo, action);
        } else {
            self.generic.insert((combo.key_code, combo.modifiers), action);
        }
    }

    pub fn len(&self) -> usize {
        self.exact.len() + self.generic.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.generic.is_empty()
    }

    /// Look up a key event, checking virtual-key-specific binding first, then generic,
    /// then stripping the numeric-pad flag if the user pressed a non-numpad arrow key.
    pub fn lookup(&self, key_code: u32, mods: u32, virtual_key: Option<u16>) -> Option<&KeyAction> {
        let masked = mods & modifiers::RELEVANT_MASK;
        if let Some(vk) = virtual_key {
            let exact_combo = KeyCombo {
                key_code,
                modifiers: masked,
                virtual_key: Some(vk),
            };
            if let Some(a) = self.exact.get(&exact_combo) {
                return Some(a);
            }
        }
        if let Some(a) = self.generic.get(&(key_code, masked)) {
            return Some(a);
        }
        // Arrow/function keys in macOS AppKit often set NSEventModifierFlagNumericPad (0x200000);
        // allow matching entries with or without NUMERIC_PAD when appropriate.
        if (masked & modifiers::NUMERIC_PAD) != 0 {
            let without_np = masked & !modifiers::NUMERIC_PAD;
            if let Some(a) = self.generic.get(&(key_code, without_np)) {
                return Some(a);
            }
        } else if (0xF700..=0xF73F).contains(&key_code) {
            let with_np = masked | modifiers::NUMERIC_PAD;
            if let Some(a) = self.generic.get(&(key_code, with_np)) {
                return Some(a);
            }
        }
        None
    }

    /// Parse an iTerm2 plist `Keyboard Map` or `GlobalKeyMap` dictionary.
    pub fn from_iterm_plist_dict(dict: &Dictionary) -> Self {
        let mut map = Self::default();
        for (k, v) in dict {
            let Some(combo) = KeyCombo::parse_iterm_key(k) else {
                continue;
            };
            let Some(entry) = v.as_dictionary() else {
                continue;
            };
            let action_code = entry
                .get("Action")
                .and_then(|v| v.as_signed_integer())
                .unwrap_or(-1) as i32;
            let text = entry
                .get("Text")
                .and_then(|v| v.as_string())
                .unwrap_or("");
            let escaping = entry
                .get("Escaping")
                .and_then(|v| v.as_signed_integer())
                .unwrap_or(1) as i32;
            let action = KeyAction::from_iterm_parts(action_code, text, escaping);
            map.insert(combo, action);
        }
        map
    }

    /// Parse an iTerm2 JSON `Keyboard Map` object.
    pub fn from_iterm_json(obj: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut map = Self::default();
        for (k, v) in obj {
            let Some(combo) = KeyCombo::parse_iterm_key(k) else {
                continue;
            };
            let Some(entry) = v.as_object() else {
                continue;
            };
            let action_code = entry
                .get("Action")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1) as i32;
            let text = entry
                .get("Text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let escaping = entry
                .get("Escaping")
                .and_then(|v| v.as_i64())
                .unwrap_or(1) as i32;
            map.insert(combo, KeyAction::from_iterm_parts(action_code, text, escaping));
        }
        map
    }

    /// Default macOS natural text editing keymap matching standard iTerm2 profiles.
    pub fn default_macos() -> Self {
        let mut map = Self::default();
        // Opt+Left / Opt+Right word navigation
        map.insert(
            KeyCombo { key_code: 0xF702, modifiers: modifiers::OPTION | modifiers::NUMERIC_PAD, virtual_key: None },
            KeyAction::SendEscapeSequence("b".into()),
        );
        map.insert(
            KeyCombo { key_code: 0xF703, modifiers: modifiers::OPTION | modifiers::NUMERIC_PAD, virtual_key: None },
            KeyAction::SendEscapeSequence("f".into()),
        );
        // Opt+Backspace delete word, Cmd+Backspace delete line
        map.insert(
            KeyCombo { key_code: 0x7F, modifiers: modifiers::OPTION, virtual_key: None },
            KeyAction::SendHexCode(vec![0x1B, 0x7F]),
        );
        map.insert(
            KeyCombo { key_code: 0x7F, modifiers: modifiers::COMMAND, virtual_key: None },
            KeyAction::SendHexCode(vec![0x15]),
        );
        // Forward delete & Opt+Forward delete
        map.insert(
            KeyCombo { key_code: 0xF728, modifiers: 0, virtual_key: None },
            KeyAction::SendHexCode(vec![0x04]),
        );
        map.insert(
            KeyCombo { key_code: 0xF728, modifiers: modifiers::OPTION, virtual_key: None },
            KeyAction::SendEscapeSequence("d".into()),
        );
        map
    }
}
