use bitflags::bitflags;
use iterm_profile::{ColorScheme, Rgba};

/// Compact 4-byte color representation (default, 256-palette index, or 24-bit truecolor).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct PackedColor(pub u32);

impl PackedColor {
    pub const DEFAULT: Self = Self(0);

    #[inline]
    pub const fn indexed(idx: u8) -> Self {
        Self(0x0100_0000 | (idx as u32))
    }

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(0x0200_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    #[inline]
    pub const fn is_default(self) -> bool {
        (self.0 >> 24) == 0
    }

    #[inline]
    pub const fn as_indexed(self) -> Option<u8> {
        if (self.0 >> 24) == 1 {
            Some((self.0 & 0xFF) as u8)
        } else {
            None
        }
    }

    #[inline]
    pub const fn as_rgb(self) -> Option<(u8, u8, u8)> {
        if (self.0 >> 24) == 2 {
            Some((
                ((self.0 >> 16) & 0xFF) as u8,
                ((self.0 >> 8) & 0xFF) as u8,
                (self.0 & 0xFF) as u8,
            ))
        } else {
            None
        }
    }

    #[inline]
    pub fn resolve_fg(self, scheme: &ColorScheme, bright_bold: bool, is_bold: bool) -> Rgba {
        match self.0 >> 24 {
            0 => {
                if is_bold && bright_bold {
                    scheme.bold
                } else {
                    scheme.foreground
                }
            }
            1 => {
                let mut idx = (self.0 & 0xFF) as u8;
                if is_bold && bright_bold && idx < 8 {
                    idx += 8;
                }
                scheme.indexed_color(idx)
            }
            _ => Rgba::from_u8(
                ((self.0 >> 16) & 0xFF) as u8,
                ((self.0 >> 8) & 0xFF) as u8,
                (self.0 & 0xFF) as u8,
                255,
            ),
        }
    }

    #[inline]
    pub fn resolve_bg(self, scheme: &ColorScheme) -> Rgba {
        match self.0 >> 24 {
            0 => scheme.background,
            1 => scheme.indexed_color((self.0 & 0xFF) as u8),
            _ => Rgba::from_u8(
                ((self.0 >> 16) & 0xFF) as u8,
                ((self.0 >> 8) & 0xFF) as u8,
                (self.0 & 0xFF) as u8,
                255,
            ),
        }
    }
}

impl std::fmt::Debug for PackedColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 >> 24 {
            0 => write!(f, "Default"),
            1 => write!(f, "Idx({})", self.0 & 0xFF),
            _ => write!(
                f,
                "Rgb({},{},{})",
                (self.0 >> 16) & 0xFF,
                (self.0 >> 8) & 0xFF,
                self.0 & 0xFF
            ),
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    #[repr(transparent)]
    pub struct CellFlags: u16 {
        const BOLD             = 1 << 0;
        const FAINT            = 1 << 1;
        const ITALIC           = 1 << 2;
        const UNDERLINE        = 1 << 3;
        const DOUBLE_UNDERLINE = 1 << 4;
        const CURLY_UNDERLINE  = 1 << 5;
        const BLINK            = 1 << 6;
        const INVERSE          = 1 << 7;
        const HIDDEN           = 1 << 8;
        const STRIKETHROUGH    = 1 << 9;
        const WIDE_CHAR           = 1 << 10;
        const WIDE_SPACER         = 1 << 11;
        const HAS_UNDERLINE_COLOR = 1 << 12;
        const HAS_HYPERLINK       = 1 << 13;
        const IMAGE_CELL          = 1 << 14;
        const HAS_GRAPHEME_TAIL   = 1 << 15;
    }
}

/// Rare per-cell metadata stored out-of-line (deduplicated & GC'd on `TerminalScreen`),
/// keeping the hot `Cell` struct at exactly 16 bytes (one 128-bit SIMD register!).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellExtra {
    pub underline_color: PackedColor,
    pub hyperlink_id: u32,
    pub image_ref_id: u32,
    pub grapheme_tail_id: u32,
}

/// Ultra-compact 16-byte `align(16)` POD terminal cell (4 cells per 64-byte L1 cache line!).
/// Zero heap allocations; 250x80 screen fits in 320 KiB (inside Apple Silicon L2 cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(16))]
pub struct Cell {
    pub ch: char,
    pub fg: PackedColor,
    pub bg: PackedColor,
    pub flags: CellFlags,
    /// Optional index into `TerminalScreen::cell_extras` (0 = none).
    pub extra_id: u16,
}

const _: () = assert!(std::mem::size_of::<Cell>() == 16);

impl Default for Cell {
    #[inline]
    fn default() -> Self {
        Self::BLANK
    }
}

impl Cell {
    pub const BLANK: Self = Self {
        ch: ' ',
        fg: PackedColor::DEFAULT,
        bg: PackedColor::DEFAULT,
        flags: CellFlags::empty(),
        extra_id: 0,
    };

    #[inline]
    pub const fn blank_with_bg(bg: PackedColor) -> Self {
        Self {
            bg,
            ..Self::BLANK
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ch == ' '
            && self.bg.is_default()
            && self.flags.is_empty()
            && self.extra_id == 0
    }
}

/// Active SGR pen state applied to newly printed characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pen {
    pub fg: PackedColor,
    pub bg: PackedColor,
    pub underline_color: PackedColor,
    pub flags: CellFlags,
    pub hyperlink_id: u32,
    /// Cached `extra_id` on `TerminalScreen` for `(underline_color, hyperlink_id)`.
    pub cached_extra_id: u16,
}

impl Pen {
    #[inline]
    pub fn reset(&mut self) {
        self.fg = PackedColor::DEFAULT;
        self.bg = PackedColor::DEFAULT;
        self.underline_color = PackedColor::DEFAULT;
        self.flags = CellFlags::empty();
        if self.hyperlink_id == 0 {
            self.cached_extra_id = 0;
        }
    }

    #[inline]
    pub fn make_cell(&self, ch: char) -> Cell {
        let mut flags = self.flags;
        if self.hyperlink_id != 0 {
            flags |= CellFlags::HAS_HYPERLINK;
        }
        if !self.underline_color.is_default() {
            flags |= CellFlags::HAS_UNDERLINE_COLOR;
        }
        Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
            flags,
            extra_id: self.cached_extra_id,
        }
    }
}
