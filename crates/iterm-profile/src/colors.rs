use plist::Dictionary;
use serde::{Deserialize, Serialize};

/// Linear/sRGB floating-point RGBA color in [0.0, 1.0].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const BLACK: Self = Self::rgb(0.0, 0.0, 0.0);
    pub const WHITE: Self = Self::rgb(1.0, 1.0, 1.0);
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    #[inline]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    #[inline]
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    #[inline]
    pub fn from_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }

    #[inline]
    pub fn to_u8(self) -> [u8; 4] {
        [
            (self.r.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.g.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        ]
    }

    #[inline]
    pub fn to_packed_rgba8(self) -> u32 {
        let [r, g, b, a] = self.to_u8();
        (r as u32) | ((g as u32) << 8) | ((b as u32) << 16) | ((a as u32) << 24)
    }

    #[inline]
    pub fn from_packed_rgba8(v: u32) -> Self {
        Self::from_u8(
            (v & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
            ((v >> 24) & 0xFF) as u8,
        )
    }

    #[inline]
    pub fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    #[inline]
    pub fn with_alpha(self, a: f32) -> Self {
        Self { a: a.clamp(0.0, 1.0), ..self }
    }

    /// Relative perceptual brightness (BT.601 luma, matching iTerm2's minimum contrast check).
    #[inline]
    pub fn luma(self) -> f32 {
        0.299 * self.r + 0.587 * self.g + 0.114 * self.b
    }

    /// Blend self over dst using linear interpolation `t`.
    #[inline]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }

    /// Adjust foreground color to satisfy iTerm2's `Minimum Contrast` setting in [0.0, 1.0].
    #[inline]
    pub fn enforce_min_contrast(self, bg: Self, min_contrast: f32) -> Self {
        if min_contrast <= 0.001 {
            return self;
        }
        let fg_l = self.luma();
        let bg_l = bg.luma();
        let diff = (fg_l - bg_l).abs();
        if diff >= min_contrast {
            return self;
        }
        let target_light = if bg_l < 0.5 { Self::WHITE } else { Self::BLACK };
        let target_l = target_light.luma();
        let desired_l = if bg_l < 0.5 {
            (bg_l + min_contrast).min(1.0)
        } else {
            (bg_l - min_contrast).max(0.0)
        };
        let denom = (target_l - fg_l).abs().max(0.001);
        let needed = ((desired_l - fg_l).abs() / denom).clamp(0.0, 1.0);
        self.lerp(target_light, needed).with_alpha(self.a)
    }

    /// Parse an iTerm2 plist color dictionary (`Red Component`, `Green Component`, `Blue Component`, optional `Alpha Component`).
    pub fn from_iterm_plist_dict(dict: &Dictionary) -> Option<Self> {
        let r = dict.get("Red Component")?.as_real()? as f32;
        let g = dict.get("Green Component")?.as_real()? as f32;
        let b = dict.get("Blue Component")?.as_real()? as f32;
        let a = dict
            .get("Alpha Component")
            .and_then(|v| v.as_real())
            .unwrap_or(1.0) as f32;
        Some(Self::new(
            r.clamp(0.0, 1.0),
            g.clamp(0.0, 1.0),
            b.clamp(0.0, 1.0),
            a.clamp(0.0, 1.0),
        ))
    }

    /// Parse an iTerm2 JSON color object (used in Dynamic Profiles).
    pub fn from_iterm_json(val: &serde_json::Value) -> Option<Self> {
        let obj = val.as_object()?;
        let r = obj.get("Red Component")?.as_f64()? as f32;
        let g = obj.get("Green Component")?.as_f64()? as f32;
        let b = obj.get("Blue Component")?.as_f64()? as f32;
        let a = obj
            .get("Alpha Component")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0) as f32;
        Some(Self::new(
            r.clamp(0.0, 1.0),
            g.clamp(0.0, 1.0),
            b.clamp(0.0, 1.0),
            a.clamp(0.0, 1.0),
        ))
    }
}

/// Complete iTerm2 terminal color scheme (16 ANSI colors + semantic colors).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorScheme {
    pub ansi: [Rgba; 16],
    pub foreground: Rgba,
    pub background: Rgba,
    pub bold: Rgba,
    pub cursor: Rgba,
    pub cursor_text: Rgba,
    pub selection: Rgba,
    pub selected_text: Rgba,
    pub link: Rgba,
    pub tab: Option<Rgba>,
    pub cursor_guide: Rgba,
}

impl Default for ColorScheme {
    fn default() -> Self {
        Self::dracula_plus()
    }
}

impl ColorScheme {
    /// Dracula+ preset matching the user's iTerm2 default color scheme.
    pub fn dracula_plus() -> Self {
        Self {
            ansi: [
                Rgba::from_u8(33, 34, 44, 255),    // 0 black
                Rgba::from_u8(255, 85, 85, 255),   // 1 red
                Rgba::from_u8(80, 250, 123, 255),  // 2 green
                Rgba::from_u8(255, 203, 107, 255), // 3 yellow
                Rgba::from_u8(130, 170, 255, 255), // 4 blue
                Rgba::from_u8(199, 146, 234, 255), // 5 magenta
                Rgba::from_u8(139, 233, 253, 255), // 6 cyan
                Rgba::from_u8(242, 248, 242, 255), // 7 white
                Rgba::from_u8(84, 84, 84, 255),    // 8 bright black
                Rgba::from_u8(255, 110, 110, 255), // 9 bright red
                Rgba::from_u8(105, 255, 148, 255), // 10 bright green
                Rgba::from_u8(255, 203, 107, 255), // 11 bright yellow
                Rgba::from_u8(214, 172, 255, 255), // 12 bright blue
                Rgba::from_u8(255, 146, 223, 255), // 13 bright magenta
                Rgba::from_u8(164, 255, 255, 255), // 14 bright cyan
                Rgba::from_u8(242, 248, 242, 255), // 15 bright white
            ],
            foreground: Rgba::from_u8(242, 248, 242, 255),
            background: Rgba::from_u8(33, 33, 33, 255),
            bold: Rgba::WHITE,
            cursor: Rgba::from_u8(236, 239, 244, 255),
            cursor_text: Rgba::from_u8(40, 40, 40, 255),
            selection: Rgba::from_u8(242, 248, 242, 255),
            selected_text: Rgba::from_u8(84, 84, 84, 255),
            link: Rgba::from_u8(254, 255, 255, 255),
            tab: Some(Rgba::from_u8(59, 66, 82, 255)),
            cursor_guide: Rgba::from_u8(60, 56, 54, 255),
        }
    }

    /// Parse a color scheme from an iTerm2 plist dictionary (profile dict or `.itermcolors` root).
    pub fn from_iterm_plist_dict(dict: &Dictionary) -> Self {
        let mut scheme = Self::dracula_plus();
        for i in 0..16 {
            let key = format!("Ansi {i} Color");
            if let Some(c) = dict
                .get(&key)
                .and_then(|v| v.as_dictionary())
                .and_then(Rgba::from_iterm_plist_dict)
            {
                scheme.ansi[i] = c;
            }
        }
        let get = |k: &str| {
            dict.get(k)
                .and_then(|v| v.as_dictionary())
                .and_then(Rgba::from_iterm_plist_dict)
        };
        if let Some(c) = get("Foreground Color") {
            scheme.foreground = c;
        }
        if let Some(c) = get("Background Color") {
            scheme.background = c;
        }
        if let Some(c) = get("Bold Color") {
            scheme.bold = c;
        }
        if let Some(c) = get("Cursor Color") {
            scheme.cursor = c;
        }
        if let Some(c) = get("Cursor Text Color") {
            scheme.cursor_text = c;
        }
        if let Some(c) = get("Selection Color") {
            scheme.selection = c;
        }
        if let Some(c) = get("Selected Text Color") {
            scheme.selected_text = c;
        }
        if let Some(c) = get("Link Color") {
            scheme.link = c;
        }
        if let Some(c) = get("Tab Color") {
            scheme.tab = Some(c);
        }
        if let Some(c) = get("Cursor Guide Color") {
            scheme.cursor_guide = c;
        }
        scheme
    }

    /// Parse a color scheme from an iTerm2 Dynamic Profile JSON object.
    pub fn from_iterm_json(obj: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut scheme = Self::dracula_plus();
        for i in 0..16 {
            let key = format!("Ansi {i} Color");
            if let Some(c) = obj.get(&key).and_then(Rgba::from_iterm_json) {
                scheme.ansi[i] = c;
            }
        }
        let get = |k: &str| obj.get(k).and_then(Rgba::from_iterm_json);
        if let Some(c) = get("Foreground Color") {
            scheme.foreground = c;
        }
        if let Some(c) = get("Background Color") {
            scheme.background = c;
        }
        if let Some(c) = get("Bold Color") {
            scheme.bold = c;
        }
        if let Some(c) = get("Cursor Color") {
            scheme.cursor = c;
        }
        if let Some(c) = get("Cursor Text Color") {
            scheme.cursor_text = c;
        }
        if let Some(c) = get("Selection Color") {
            scheme.selection = c;
        }
        if let Some(c) = get("Selected Text Color") {
            scheme.selected_text = c;
        }
        if let Some(c) = get("Link Color") {
            scheme.link = c;
        }
        if let Some(c) = get("Tab Color") {
            scheme.tab = Some(c);
        }
        if let Some(c) = get("Cursor Guide Color") {
            scheme.cursor_guide = c;
        }
        scheme
    }

    /// Resolve a standard 256-color xterm palette index to RGBA.
    #[inline]
    pub fn indexed_color(&self, idx: u8) -> Rgba {
        match idx {
            0..=15 => self.ansi[idx as usize],
            16..=231 => {
                let v = idx - 16;
                let r = v / 36;
                let g = (v % 36) / 6;
                let b = v % 6;
                let to_ch = |c: u8| if c == 0 { 0u8 } else { 55 + c * 40 };
                Rgba::from_u8(to_ch(r), to_ch(g), to_ch(b), 255)
            }
            232..=255 => {
                let gray = 8 + (idx - 232) * 10;
                Rgba::from_u8(gray, gray, gray, 255)
            }
        }
    }
}
