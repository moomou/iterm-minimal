use crate::cell::{Cell, CellExtra, CellFlags, PackedColor, Pen};
use crate::grid::{ActiveGrid, PromptMark, StringPool};
use crate::inline_image::{
    decode_transfer, parse_remote_artifact, DecodedTransfer, FileTransferParams, ImageCellRef,
    InlineImage, MultipartTransfer,
};
use crate::parser::{CsiParams, VtParser, VtSink};
use base64::Engine;
use bitflags::bitflags;
use iterm_profile::{CursorStyle, RemoteArtifact, Rgba};
use rustc_hash::FxHashMap;
use unicode_width::UnicodeWidthChar;

use crate::scrollback::Scrollback;

bitflags! {
    /// Active terminal mode flags (DEC private modes + ANSI modes).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct TermModes: u32 {
        const APPLICATION_CURSOR   = 1 << 0;  // DECCKM (?1)
        const ORIGIN               = 1 << 1;  // DECOM (?6)
        const AUTO_WRAP            = 1 << 2;  // DECAWM (?7)
        const CURSOR_VISIBLE       = 1 << 3;  // DECTCEM (?25)
        const ALT_SCREEN           = 1 << 4;  // ?47 / ?1047 / ?1049
        const MOUSE_X10            = 1 << 5;  // ?9
        const MOUSE_VT200          = 1 << 6;  // ?1000
        const MOUSE_BUTTON         = 1 << 7;  // ?1002
        const MOUSE_ANY            = 1 << 8;  // ?1003
        const FOCUS_EVENTS         = 1 << 9;  // ?1004
        const MOUSE_SGR            = 1 << 10; // ?1006
        const ALT_SCROLL           = 1 << 11; // ?1007
        const BRACKETED_PASTE      = 1 << 12; // ?2004
        const SYNC_OUTPUT          = 1 << 13; // ?2026 (Synchronized Output — eliminates TUI tearing!)
        const INSERT_MODE          = 1 << 14; // IRM (4)
        const LINEFEED_NEWLINE     = 1 << 15; // LNM (20)
        const APPLICATION_KEYPAD   = 1 << 16; // DECPAM / DECPNM
        const REVERSE_VIDEO        = 1 << 17; // DECSCNM (?5)
    }
}

/// Cursor position and saved state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub col: usize,
    pub row: usize,
    pub pending_wrap: bool,
    pub style: CursorStyle,
    pub blinking: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            col: 0,
            row: 0,
            pending_wrap: false,
            style: CursorStyle::Block,
            blinking: false,
        }
    }
}

/// VT100 G0/G1 character set (`ESC ( 0` / `ESC ( B`) for ncurses/htop line-drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Charset {
    #[default]
    Ascii,
    DecSpecialGraphics,
}

/// DEC Special Graphics translation table for ASCII bytes `0x5F..=0x7E` (`_`..`~`).
const DEC_SPECIAL_GRAPHICS: [char; 32] = [
    '\u{00A0}', // 0x5F '_' -> NBSP
    '\u{25C6}', // 0x60 '`' -> ◆
    '\u{2592}', // 0x61 'a' -> ▒
    '\u{2409}', // 0x62 'b' -> ␉
    '\u{240C}', // 0x63 'c' -> ␌
    '\u{240D}', // 0x64 'd' -> ␍
    '\u{240A}', // 0x65 'e' -> ␊
    '\u{00B0}', // 0x66 'f' -> °
    '\u{00B1}', // 0x67 'g' -> ±
    '\u{2424}', // 0x68 'h' -> ␤
    '\u{240B}', // 0x69 'i' -> ␋
    '\u{2518}', // 0x6A 'j' -> ┘
    '\u{2510}', // 0x6B 'k' -> ┐
    '\u{250C}', // 0x6C 'l' -> ┌
    '\u{2514}', // 0x6D 'm' -> └
    '\u{253C}', // 0x6E 'n' -> ┼
    '\u{23BA}', // 0x6F 'o' -> ⎺
    '\u{23BB}', // 0x70 'p' -> ⎻
    '\u{2500}', // 0x71 'q' -> ─
    '\u{23BC}', // 0x72 'r' -> ⎼
    '\u{23BD}', // 0x73 's' -> ⎽
    '\u{251C}', // 0x74 't' -> ├
    '\u{2524}', // 0x75 'u' -> ┤
    '\u{2534}', // 0x76 'v' -> ┴
    '\u{252C}', // 0x77 'w' -> ┬
    '\u{2502}', // 0x78 'x' -> │
    '\u{2264}', // 0x79 'y' -> ≤
    '\u{2265}', // 0x7A 'z' -> ≥
    '\u{03C0}', // 0x7B '{' -> π
    '\u{2260}', // 0x7C '|' -> ≠
    '\u{00A3}', // 0x7D '}' -> £
    '\u{00B7}', // 0x7E '~' -> ·
];

#[inline(always)]
fn translate_dec_graphics(b: u8) -> char {
    if (0x5F..=0x7E).contains(&b) {
        DEC_SPECIAL_GRAPHICS[(b - 0x5F) as usize]
    } else {
        b as char
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SavedCursor {
    cursor: Cursor,
    pen: Pen,
    origin_mode: bool,
    charsets: [Charset; 2],
    active_charset: u8,
}

/// Out-of-band events produced by terminal output for the window/session host.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    TitleChanged(String),
    Bell,
    TabColorChanged(Option<Rgba>),
    ClipboardSet(String),
    ClipboardRequest,
    WriteBack(Vec<u8>),
    FileDownload { name: String, data: Vec<u8> },
    ProfileChangeRequested(String),
    RemoteHostChanged(String),
    CurrentDirChanged(String),
    UserVarChanged { key: String, value: String },
    Notification { title: String, body: String },
    RequestAttention,
}

/// Text selection range in absolute line coordinates (`0` = first ever line; active screen starts at `evicted + scrollback.len()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub abs_line: u64,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Normal,
    Word,
    Line,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub mode: SelectionMode,
}

impl Selection {
    #[inline]
    pub fn ordered(&self) -> (SelectionPoint, SelectionPoint) {
        if (self.start.abs_line, self.start.col) <= (self.end.abs_line, self.end.col) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    #[inline]
    pub fn contains(&self, abs_line: u64, col: usize) -> bool {
        let (s, e) = self.ordered();
        if abs_line < s.abs_line || abs_line > e.abs_line {
            return false;
        }
        match self.mode {
            SelectionMode::Block => {
                let cmin = self.start.col.min(self.end.col);
                let cmax = self.start.col.max(self.end.col);
                col >= cmin && col <= cmax
            }
            SelectionMode::Line => true,
            SelectionMode::Normal | SelectionMode::Word => {
                if s.abs_line == e.abs_line {
                    col >= s.col && col <= e.col
                } else if abs_line == s.abs_line {
                    col >= s.col
                } else if abs_line == e.abs_line {
                    col <= e.col
                } else {
                    true
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub abs_line: u64,
    pub start_col: usize,
    pub end_col: usize,
}

/// Full VT100/xterm-256color/iTerm2 terminal screen state.
pub struct TerminalScreen {
    pub primary: ActiveGrid,
    pub alternate: ActiveGrid,
    pub scrollback: Scrollback,
    pub cursor: Cursor,
    saved_primary: SavedCursor,
    saved_alt: SavedCursor,
    pub pen: Pen,
    pub modes: TermModes,
    scroll_top: usize,
    scroll_bottom: usize,
    tab_stops: Vec<bool>,

    /// Viewport scroll offset upward into scrollback (`0` = live bottom).
    pub viewport_offset: usize,

    /// Title & iTerm2 tab color state (`OSC 0/2`, `CSI 22/23 t` stack, and `OSC 6`).
    pub title: String,
    title_stack: Vec<String>,
    pub tab_color: Option<Rgba>,
    tab_r: Option<u8>,
    tab_g: Option<u8>,
    tab_b: Option<u8>,

    /// Shell integration metadata (interned into `strings` pool for O(1) row stamping).
    pub strings: StringPool,
    pub current_host_id: u32,
    pub current_cwd_id: u32,
    pub shell_integration_version: Option<String>,
    pub user_vars: FxHashMap<String, String>,

    /// Sparse side tables (indexed by `Cell.extra_id` when corresponding flag is set; auto-GC'd!).
    cell_extras: Vec<CellExtra>,
    cell_extra_lookup: FxHashMap<CellExtra, u16>,
    hyperlinks: Vec<String>,
    hyperlink_lookup: FxHashMap<String, u32>,
    grapheme_tails: Vec<String>,
    grapheme_tail_lookup: FxHashMap<String, u32>,
    pub images: FxHashMap<u32, InlineImage>,
    image_cells: Vec<ImageCellRef>,
    next_image_id: u32,
    multipart: Option<MultipartTransfer>,
    /// True if the last printed codepoint was U+200D (ZWJ), so the next emoji joins its cluster!
    pending_zwj: bool,
    /// Sparse per-absolute-line explicit `OSC 1337;Artifact=...` map (0 bytes on normal rows!).
    pub artifacts: FxHashMap<u64, Vec<RemoteArtifact>>,

    /// Cell pixel size hint used for inline image sizing (`imgcat`).
    pub cell_width_px: f32,
    pub cell_height_px: f32,

    /// Default terminal palette hints reported to `OSC 10/11/12;?` color queries (`neovim`/`bat`/`delta`/`fzf`).
    pub default_fg_rgb: (u8, u8, u8),
    pub default_bg_rgb: (u8, u8, u8),
    pub default_cursor_rgb: (u8, u8, u8),

    /// VT100 G0/G1 character sets (`ESC ( 0` / `ESC ( B`) and active shift (`SO`/`SI`).
    charsets: [Charset; 2],
    active_charset: u8,

    /// Kitty keyboard protocol mode stack.
    kitty_kbd_stack: Vec<u8>,

    /// Active text selection & search matches.
    pub selection: Option<Selection>,
    pub search_matches: Vec<SearchMatch>,
    pub active_search_match: Option<usize>,

    /// Pending out-of-band events drained by the UI/session host.
    pub events: Vec<TerminalEvent>,

    /// Internal VT parser (kept inside or driven via `feed`).
    parser: VtParser,
    /// Reusable scratch buffer for `try_bulk_ascii_lines` (zero heap allocation on hot path!).
    bulk_lines_scratch: Vec<(usize, usize, bool)>,
}

impl TerminalScreen {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut tab_stops = vec![false; cols];
        for i in (8..cols).step_by(8) {
            tab_stops[i] = true;
        }
        Self {
            primary: ActiveGrid::new(cols, rows),
            alternate: ActiveGrid::new(cols, rows),
            scrollback: Scrollback::new(max_scrollback),
            cursor: Cursor::default(),
            saved_primary: SavedCursor::default(),
            saved_alt: SavedCursor::default(),
            pen: Pen::default(),
            modes: TermModes::AUTO_WRAP | TermModes::CURSOR_VISIBLE,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            tab_stops,
            viewport_offset: 0,
            title: String::new(),
            title_stack: Vec::new(),
            tab_color: None,
            tab_r: None,
            tab_g: None,
            tab_b: None,
            strings: StringPool::new(),
            current_host_id: 0,
            current_cwd_id: 0,
            shell_integration_version: None,
            user_vars: FxHashMap::default(),
            cell_extras: vec![CellExtra::default()], // 0 = none
            cell_extra_lookup: FxHashMap::default(),
            hyperlinks: vec![String::new()], // 0 = none
            hyperlink_lookup: FxHashMap::default(),
            grapheme_tails: vec![String::new()], // 0 = none
            grapheme_tail_lookup: FxHashMap::default(),
            images: FxHashMap::default(),
            image_cells: vec![ImageCellRef {
                image_id: 0,
                tile_col: 0,
                tile_row: 0,
                total_cols: 0,
                total_rows: 0,
            }],
            next_image_id: 1,
            multipart: None,
            pending_zwj: false,
            artifacts: FxHashMap::default(),
            cell_width_px: 10.0,
            cell_height_px: 20.0,
            default_fg_rgb: (248, 248, 242),
            default_bg_rgb: (40, 42, 54),
            default_cursor_rgb: (248, 248, 242),
            charsets: [Charset::Ascii; 2],
            active_charset: 0,
            kitty_kbd_stack: Vec::new(),
            selection: None,
            search_matches: Vec::new(),
            active_search_match: None,
            events: Vec::new(),
            parser: VtParser::new(),
            bulk_lines_scratch: Vec::new(),
        }
    }

    /// Feed raw PTY output bytes directly into the VT parser & screen state.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(bytes, self);
        self.parser = parser;
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.active_grid().cols()
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.active_grid().rows()
    }

    #[inline]
    pub fn active_grid(&self) -> &ActiveGrid {
        if self.modes.contains(TermModes::ALT_SCREEN) {
            &self.alternate
        } else {
            &self.primary
        }
    }

    #[inline]
    pub fn active_grid_mut(&mut self) -> &mut ActiveGrid {
        if self.modes.contains(TermModes::ALT_SCREEN) {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    /// Absolute line number corresponding to logical row 0 of the active screen.
    #[inline]
    pub fn screen_top_abs_line(&self) -> u64 {
        self.scrollback.evicted_lines() + self.scrollback.len() as u64
    }

    /// Absolute line number corresponding to visible viewport row `vis_row` (accounting for `viewport_offset`).
    #[inline]
    pub fn visible_abs_line(&self, vis_row: usize) -> u64 {
        let top = self.screen_top_abs_line();
        top.saturating_sub(self.viewport_offset as u64) + vis_row as u64
    }

    /// Intern a `CellExtra` entry and return its compact 16-bit ID (auto-GCing before u16 saturation!).
    #[inline]
    fn intern_cell_extra(&mut self, extra: CellExtra) -> u16 {
        if extra == CellExtra::default() {
            return 0;
        }
        if let Some(&id) = self.cell_extra_lookup.get(&extra) {
            return id;
        }
        if self.cell_extras.len() >= 48_000 {
            self.gc_side_tables();
            if let Some(&id) = self.cell_extra_lookup.get(&extra) {
                return id;
            }
        }
        if self.cell_extras.len() >= u16::MAX as usize {
            return 0;
        }
        let id = self.cell_extras.len() as u16;
        self.cell_extras.push(extra);
        self.cell_extra_lookup.insert(extra, id);
        id
    }

    fn intern_grapheme_tail(&mut self, tail: &str) -> u32 {
        if tail.is_empty() {
            return 0;
        }
        if let Some(&id) = self.grapheme_tail_lookup.get(tail) {
            return id;
        }
        let id = self.grapheme_tails.len() as u32;
        self.grapheme_tails.push(tail.to_string());
        self.grapheme_tail_lookup.insert(tail.to_string(), id);
        id
    }

    /// Garbage-collect unreachable `cell_extras`, `hyperlinks`, `grapheme_tails`, `images`,
    /// `image_cells`, and evicted `artifacts` so long-lived sessions never leak RAM or saturate `u16::MAX`!
    pub fn gc_side_tables(&mut self) {
        let evicted = self.scrollback.evicted_lines();
        self.artifacts.retain(|&abs_line, _| abs_line >= evicted);

        let mut new_extras = vec![CellExtra::default()];
        let mut new_extra_lookup = FxHashMap::default();
        let mut new_hyperlinks = vec![String::new()];
        let mut new_hyperlink_lookup = FxHashMap::default();
        let mut new_tails = vec![String::new()];
        let mut new_tail_lookup = FxHashMap::default();
        let mut new_image_cells = vec![ImageCellRef {
            image_id: 0,
            tile_col: 0,
            tile_row: 0,
            total_cols: 0,
            total_rows: 0,
        }];
        let mut new_images = FxHashMap::default();
        let mut old_to_new: FxHashMap<u16, u16> = FxHashMap::default();

        let old_extras = &self.cell_extras;
        let old_hyperlinks = &self.hyperlinks;
        let old_tails = &self.grapheme_tails;
        let old_image_cells = &self.image_cells;
        let old_images = &self.images;

        let mut remap_id = |old_id: u16| -> u16 {
            if old_id == 0 {
                return 0;
            }
            if let Some(&mapped) = old_to_new.get(&old_id) {
                return mapped;
            }
            let Some(&ex) = old_extras.get(old_id as usize) else {
                return 0;
            };
            let new_hid = if ex.hyperlink_id != 0 {
                if let Some(url) = old_hyperlinks.get(ex.hyperlink_id as usize) {
                    if let Some(&id) = new_hyperlink_lookup.get(url) {
                        id
                    } else {
                        let id = new_hyperlinks.len() as u32;
                        new_hyperlinks.push(url.clone());
                        new_hyperlink_lookup.insert(url.clone(), id);
                        id
                    }
                } else {
                    0
                }
            } else {
                0
            };
            let new_tid = if ex.grapheme_tail_id != 0 {
                if let Some(t) = old_tails.get(ex.grapheme_tail_id as usize) {
                    if let Some(&id) = new_tail_lookup.get(t) {
                        id
                    } else {
                        let id = new_tails.len() as u32;
                        new_tails.push(t.clone());
                        new_tail_lookup.insert(t.clone(), id);
                        id
                    }
                } else {
                    0
                }
            } else {
                0
            };
            let new_iref = if ex.image_ref_id != 0 {
                if let Some(&iref) = old_image_cells.get(ex.image_ref_id as usize) {
                    if let Some(img) = old_images.get(&iref.image_id) {
                        new_images.entry(iref.image_id).or_insert_with(|| img.clone());
                    }
                    let id = new_image_cells.len() as u32;
                    new_image_cells.push(iref);
                    id
                } else {
                    0
                }
            } else {
                0
            };

            let new_ex = CellExtra {
                underline_color: ex.underline_color,
                hyperlink_id: new_hid,
                image_ref_id: new_iref,
                grapheme_tail_id: new_tid,
            };
            let mapped = if new_ex == CellExtra::default() {
                0
            } else if let Some(&existing) = new_extra_lookup.get(&new_ex) {
                existing
            } else if new_extras.len() < u16::MAX as usize {
                let id = new_extras.len() as u16;
                new_extras.push(new_ex);
                new_extra_lookup.insert(new_ex, id);
                id
            } else {
                0
            };
            old_to_new.insert(old_id, mapped);
            mapped
        };

        for cell in self.primary.cells_mut() {
            if cell.extra_id != 0 {
                cell.extra_id = remap_id(cell.extra_id);
            }
        }
        for cell in self.alternate.cells_mut() {
            if cell.extra_id != 0 {
                cell.extra_id = remap_id(cell.extra_id);
            }
        }
        self.scrollback.for_each_styled_cell_mut(|cell| {
            cell.extra_id = remap_id(cell.extra_id);
        });

        // Remap active and saved pen extra IDs before dropping `remap_id`
        for pen in [
            &mut self.pen,
            &mut self.saved_primary.pen,
            &mut self.saved_alt.pen,
        ] {
            if pen.cached_extra_id != 0 {
                pen.cached_extra_id = remap_id(pen.cached_extra_id);
            }
        }
        drop(remap_id);

        for pen in [
            &mut self.pen,
            &mut self.saved_primary.pen,
            &mut self.saved_alt.pen,
        ] {
            pen.hyperlink_id = if pen.cached_extra_id != 0 {
                new_extras
                    .get(pen.cached_extra_id as usize)
                    .map(|e| e.hyperlink_id)
                    .unwrap_or(0)
            } else {
                0
            };
        }

        self.cell_extras = new_extras;
        self.cell_extra_lookup = new_extra_lookup;
        self.hyperlinks = new_hyperlinks;
        self.hyperlink_lookup = new_hyperlink_lookup;
        self.grapheme_tails = new_tails;
        self.grapheme_tail_lookup = new_tail_lookup;
        self.image_cells = new_image_cells;
        self.images = new_images;
    }

    #[inline]
    fn sync_pen_extra(&mut self) {
        if self.pen.hyperlink_id == 0 && self.pen.underline_color.is_default() {
            self.pen.cached_extra_id = 0;
        } else {
            let ex = CellExtra {
                underline_color: self.pen.underline_color,
                hyperlink_id: self.pen.hyperlink_id,
                image_ref_id: 0,
                grapheme_tail_id: 0,
            };
            self.pen.cached_extra_id = self.intern_cell_extra(ex);
        }
    }

    #[inline]
    pub fn cell_grapheme_tail(&self, cell: &Cell) -> Option<&str> {
        if cell.extra_id == 0 || !cell.flags.contains(CellFlags::HAS_GRAPHEME_TAIL) {
            return None;
        }
        let tid = self.cell_extras.get(cell.extra_id as usize)?.grapheme_tail_id;
        if tid == 0 {
            return None;
        }
        self.grapheme_tails.get(tid as usize).map(|s| s.as_str())
    }

    /// Append a zero-width combining mark / variation selector / ZWJ-joined emoji onto the preceding cell.
    fn attach_combining_char(&mut self, ch: char) {
        let cols = self.cols();
        let row_idx = self.cursor.row.min(self.rows() - 1);
        let mut target_col = if self.cursor.pending_wrap {
            cols - 1
        } else if self.cursor.col > 0 {
            self.cursor.col - 1
        } else {
            return;
        };
        let prev_cell = self.active_grid().row(row_idx)[target_col];
        if prev_cell.flags.contains(CellFlags::WIDE_SPACER) && target_col > 0 {
            target_col -= 1;
        }
        let base_cell = self.active_grid().row(row_idx)[target_col];
        let existing_ex = self
            .cell_extras
            .get(base_cell.extra_id as usize)
            .copied()
            .unwrap_or_default();
        let mut tail = if existing_ex.grapheme_tail_id != 0 {
            self.grapheme_tails
                .get(existing_ex.grapheme_tail_id as usize)
                .cloned()
                .unwrap_or_default()
        } else {
            String::new()
        };
        // Cap cluster length safely against adversarial combining-mark spam
        if tail.chars().count() >= 16 {
            return;
        }
        tail.push(ch);
        let new_tid = self.intern_grapheme_tail(&tail);
        let new_ex = CellExtra {
            grapheme_tail_id: new_tid,
            ..existing_ex
        };
        let new_extra_id = self.intern_cell_extra(new_ex);
        let (row_cells, meta) = self.active_grid_mut().row_and_meta_mut(row_idx);
        meta.set_has_non_ascii(true);
        meta.set_styled(true);
        row_cells[target_col].flags.insert(CellFlags::HAS_GRAPHEME_TAIL);
        row_cells[target_col].extra_id = new_extra_id;
    }

    pub fn side_table_len(&self) -> usize {
        self.cell_extras.len()
    }

    #[inline]
    pub fn cell_underline_color(&self, cell: &Cell) -> PackedColor {
        if cell.extra_id == 0 || !cell.flags.contains(CellFlags::HAS_UNDERLINE_COLOR) {
            return PackedColor::DEFAULT;
        }
        self.cell_extras
            .get(cell.extra_id as usize)
            .map(|e| e.underline_color)
            .unwrap_or(PackedColor::DEFAULT)
    }

    #[inline]
    pub fn cell_hyperlink_url(&self, cell: &Cell) -> Option<&str> {
        if cell.extra_id == 0 || !cell.flags.contains(CellFlags::HAS_HYPERLINK) {
            return None;
        }
        let hid = self.cell_extras.get(cell.extra_id as usize)?.hyperlink_id;
        self.hyperlink_url(hid)
    }

    #[inline]
    pub fn hyperlink_url(&self, id: u32) -> Option<&str> {
        if id == 0 {
            return None;
        }
        self.hyperlinks.get(id as usize).map(|s| s.as_str())
    }

    #[inline]
    pub fn image_cell_ref(&self, extra_id: u16) -> Option<ImageCellRef> {
        if extra_id == 0 {
            return None;
        }
        let img_ref_id = self.cell_extras.get(extra_id as usize)?.image_ref_id;
        if img_ref_id == 0 {
            return None;
        }
        self.image_cells.get(img_ref_id as usize).copied()
    }

    #[inline]
    pub fn artifacts_at_abs_line(&self, abs_line: u64) -> Option<&[RemoteArtifact]> {
        self.artifacts.get(&abs_line).map(|v| v.as_slice())
    }

    pub fn current_host(&self) -> Option<&str> {
        self.strings.get(self.current_host_id)
    }

    pub fn current_cwd(&self) -> Option<&str> {
        self.strings.get(self.current_cwd_id)
    }

    pub fn drain_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn scroll_viewport(&mut self, delta_lines: isize) {
        if self.modes.contains(TermModes::ALT_SCREEN) {
            self.viewport_offset = 0;
            return;
        }
        let max_off = self.scrollback.len();
        if delta_lines > 0 {
            self.viewport_offset = (self.viewport_offset + delta_lines as usize).min(max_off);
        } else {
            self.viewport_offset = self.viewport_offset.saturating_sub((-delta_lines) as usize);
        }
    }

    pub fn clear_scrollback_and_screen(&mut self) {
        self.scrollback.clear();
        self.viewport_offset = 0;
        let bg = self.pen.bg;
        let host = self.current_host_id;
        let cwd = self.current_cwd_id;
        let rows = self.rows();
        let grid = self.active_grid_mut();
        for r in 0..rows {
            grid.clear_row(r, bg, host, cwd);
        }
        self.cursor.col = 0;
        self.cursor.row = 0;
        self.cursor.pending_wrap = false;
        self.selection = None;
        self.gc_side_tables();
    }

    /// Resize primary and alternate screens, reflowing soft-wrapped lines (including scrollback
    /// pull-back when widening or growing taller!) and tracking the cursor's exact logical offset.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(2);
        let new_rows = new_rows.max(1);
        let old_cols = self.primary.cols();
        let old_rows = self.primary.rows();

        if (new_cols != old_cols || new_rows != old_rows)
            && !self.modes.contains(TermModes::ALT_SCREEN)
        {
            self.reflow_primary(new_cols, new_rows);
        } else if old_rows != new_rows || old_cols != new_cols {
            self.primary.resize_simple(new_cols, new_rows);
        }

        self.alternate.resize_simple(new_cols, new_rows);
        self.scroll_top = 0;
        self.scroll_bottom = new_rows - 1;
        self.cursor.col = self.cursor.col.min(new_cols - 1);
        self.cursor.row = self.cursor.row.min(new_rows - 1);
        self.cursor.pending_wrap = false;
        self.tab_stops.resize(new_cols, false);
        for i in (8..new_cols).step_by(8) {
            self.tab_stops[i] = true;
        }
    }

    /// Unwrap recent scrollback + primary screen rows into logical paragraphs and re-wrap to `new_cols`.
    /// When widening or growing taller, recently scrolled-off lines flow naturally back onto the primary screen!
    fn reflow_primary(&mut self, new_cols: usize, new_rows: usize) {
        let old_rows = self.primary.rows();
        let cur_r = self.cursor.row.min(old_rows.saturating_sub(1));
        let cur_c = self.cursor.col;

        // 1. Pull back recent scrollback lines so widening / taller resizes restore scrolled history cleanly!
        let pull_target = new_rows.saturating_mul(2).min(512);
        let mut pulled: Vec<(Vec<Cell>, bool, u32, u32, Option<PromptMark>)> = Vec::new();
        while pulled.len() < pull_target {
            let Some((cells, meta)) = self.scrollback.pop_back_line() else {
                break;
            };
            pulled.push((
                cells,
                meta.wrapped(),
                meta.host_id,
                meta.cwd_id,
                meta.prompt_mark(),
            ));
        }
        // If the oldest pulled line continues a wrapped paragraph from an earlier scrollback line,
        // pull until we hit a non-wrapped boundary (bounded safely at +64 lines) so we never split a line!
        let mut extra = 0usize;
        while extra < 64 && !self.scrollback.is_empty() {
            let prev_idx = self.scrollback.len() - 1;
            let prev_wrapped = self
                .scrollback
                .get(prev_idx)
                .is_some_and(|(_, m)| m.wrapped());
            if !prev_wrapped {
                break;
            }
            let Some((cells, meta)) = self.scrollback.pop_back_line() else {
                break;
            };
            pulled.push((
                cells,
                meta.wrapped(),
                meta.host_id,
                meta.cwd_id,
                meta.prompt_mark(),
            ));
            extra += 1;
        }
        pulled.reverse();

        // 2. Collect logical paragraphs from pulled scrollback + primary grid (up to last non-empty row)
        let mut last_non_empty = cur_r;
        for r in (0..old_rows).rev() {
            if r <= cur_r || self.primary.meta(r).used_cols > 0 {
                last_non_empty = r;
                break;
            }
        }

        struct Paragraph {
            cells: Vec<Cell>,
            host_id: u32,
            cwd_id: u32,
            prompt_mark: Option<PromptMark>,
            cursor_idx: Option<usize>,
        }

        let mut paragraphs: Vec<Paragraph> = Vec::new();
        let mut open_para: Option<Paragraph> = None;

        // Feed pulled scrollback rows first
        for (cells, wrapped, host_id, cwd_id, prompt_mark) in pulled {
            let para = open_para.get_or_insert_with(|| Paragraph {
                cells: Vec::new(),
                host_id,
                cwd_id,
                prompt_mark,
                cursor_idx: None,
            });
            for cell in cells {
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                para.cells.push(cell);
            }
            if !wrapped {
                if let Some(mut finished) = open_para.take() {
                    while finished.cells.last().is_some_and(|c| c.is_empty()) {
                        finished.cells.pop();
                    }
                    paragraphs.push(finished);
                }
            }
        }

        // Feed primary grid rows 0..=last_non_empty (joining cleanly if scrollback ended wrapped!)
        for r in 0..=last_non_empty {
            let row = self.primary.row(r);
            let meta = *self.primary.meta(r);
            let wrapped = meta.wrapped();
            let para = open_para.get_or_insert_with(|| Paragraph {
                cells: Vec::new(),
                host_id: meta.host_id,
                cwd_id: meta.cwd_id,
                prompt_mark: meta.prompt_mark(),
                cursor_idx: None,
            });
            let mut len = if wrapped {
                row.len()
            } else {
                (meta.used_cols as usize).min(row.len())
            };
            if r == cur_r {
                len = len.max((cur_c + 1).min(row.len()));
            }
            for (c_idx, &cell) in row[..len].iter().enumerate() {
                if r == cur_r && c_idx == cur_c {
                    para.cursor_idx = Some(para.cells.len());
                }
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                para.cells.push(cell);
            }
            if r == cur_r && para.cursor_idx.is_none() {
                para.cursor_idx = Some(para.cells.len());
            }
            if !wrapped || r == last_non_empty {
                if let Some(mut finished) = open_para.take() {
                    let min_keep = finished.cursor_idx.map(|i| i + 1).unwrap_or(0);
                    while finished.cells.len() > min_keep
                        && finished.cells.last().is_some_and(|c| c.is_empty())
                    {
                        finished.cells.pop();
                    }
                    paragraphs.push(finished);
                }
            }
        }
        if let Some(finished) = open_para.take() {
            paragraphs.push(finished);
        }

        // 2. Re-wrap every logical paragraph to `new_cols`
        struct ReflowedRow {
            cells: Vec<Cell>,
            wrapped: bool,
            host_id: u32,
            cwd_id: u32,
            prompt_mark: Option<PromptMark>,
        }

        let mut out_rows: Vec<ReflowedRow> = Vec::with_capacity(new_rows);
        let mut new_cur_row = 0usize;
        let mut new_cur_col = 0usize;

        for para in paragraphs {
            if para.cells.is_empty() {
                if para.cursor_idx.is_some() {
                    new_cur_row = out_rows.len();
                    new_cur_col = 0;
                }
                out_rows.push(ReflowedRow {
                    cells: Vec::new(),
                    wrapped: false,
                    host_id: para.host_id,
                    cwd_id: para.cwd_id,
                    prompt_mark: para.prompt_mark,
                });
                continue;
            }

            let mut cur_line: Vec<Cell> = Vec::with_capacity(new_cols);
            let mut is_first_seg = true;
            let para_len = para.cells.len();
            for (idx, cell) in para.cells.into_iter().enumerate() {
                let w = if cell.flags.contains(CellFlags::WIDE_CHAR) { 2 } else { 1 };
                if cur_line.len() + w > new_cols && !cur_line.is_empty() {
                    out_rows.push(ReflowedRow {
                        cells: std::mem::take(&mut cur_line),
                        wrapped: true,
                        host_id: para.host_id,
                        cwd_id: para.cwd_id,
                        prompt_mark: if is_first_seg { para.prompt_mark } else { None },
                    });
                    is_first_seg = false;
                }
                if para.cursor_idx == Some(idx) {
                    new_cur_row = out_rows.len();
                    new_cur_col = cur_line.len().min(new_cols - 1);
                }
                cur_line.push(cell);
                if w == 2 && cur_line.len() < new_cols {
                    cur_line.push(Cell {
                        ch: ' ',
                        fg: cell.fg,
                        bg: cell.bg,
                        flags: CellFlags::WIDE_SPACER,
                        extra_id: 0,
                    });
                }
            }
            if para.cursor_idx == Some(para_len) {
                new_cur_row = out_rows.len();
                new_cur_col = cur_line.len().min(new_cols - 1);
            }
            out_rows.push(ReflowedRow {
                cells: cur_line,
                wrapped: false,
                host_id: para.host_id,
                cwd_id: para.cwd_id,
                prompt_mark: if is_first_seg { para.prompt_mark } else { None },
            });
        }

        // 3. Push overflow top lines into scrollback and populate the new primary ActiveGrid
        let overflow = out_rows.len().saturating_sub(new_rows);
        let mut next = ActiveGrid::new(new_cols, new_rows);
        for (i, rrow) in out_rows.into_iter().enumerate() {
            if i < overflow {
                let mut m = crate::grid::RowMeta::default();
                m.set_wrapped(rrow.wrapped);
                m.used_cols = rrow.cells.len() as u16;
                m.host_id = rrow.host_id;
                m.cwd_id = rrow.cwd_id;
                m.set_prompt_mark(rrow.prompt_mark);
                if rrow.cells.iter().any(|c| !c.ch.is_ascii() || !c.fg.is_default() || !c.bg.is_default() || !c.flags.is_empty()) {
                    m.set_styled(true);
                }
                self.scrollback.push_row(&rrow.cells, &m);
            } else {
                let dst_r = i - overflow;
                if dst_r < new_rows {
                    let (dst_cells, dst_meta) = next.row_and_meta_mut(dst_r);
                    let n = rrow.cells.len().min(new_cols);
                    dst_cells[..n].copy_from_slice(&rrow.cells[..n]);
                    dst_meta.used_cols = n as u16;
                    dst_meta.set_wrapped(rrow.wrapped);
                    dst_meta.host_id = rrow.host_id;
                    dst_meta.cwd_id = rrow.cwd_id;
                    dst_meta.set_prompt_mark(rrow.prompt_mark);
                    if rrow.cells.iter().any(|c| !c.ch.is_ascii()) {
                        dst_meta.set_has_non_ascii(true);
                    }
                    if rrow.cells.iter().any(|c| !c.fg.is_default() || !c.bg.is_default() || !c.flags.is_empty() || c.extra_id != 0) {
                        dst_meta.set_styled(true);
                    }
                }
            }
        }
        self.primary = next;
        self.cursor.row = new_cur_row.saturating_sub(overflow).min(new_rows - 1);
        self.cursor.col = new_cur_col.min(new_cols - 1);
    }

    /// Look up visible row cells + metadata (handles both scrollback and live screen transparently).
    pub fn read_visible_row<'a>(
        &'a self,
        vis_row: usize,
        scratch: &'a mut Vec<Cell>,
    ) -> (&'a [Cell], u64, u32, u32, Option<&'a [RemoteArtifact]>) {
        let cols = self.cols();
        let abs_line = self.visible_abs_line(vis_row);
        let arts = self.artifacts.get(&abs_line).map(|v| v.as_slice());

        if self.modes.contains(TermModes::ALT_SCREEN) || self.viewport_offset == 0 {
            let grid = self.active_grid();
            let row = grid.row(vis_row);
            let meta = grid.meta(vis_row);
            return (row, meta.generation, meta.host_id, meta.cwd_id, arts);
        }

        let sb_len = self.scrollback.len();
        let start_idx = sb_len.saturating_sub(self.viewport_offset);
        let target = start_idx + vis_row;
        if target < sb_len {
            scratch.resize(cols, Cell::BLANK);
            if let Some(meta) = self.scrollback.copy_line_padded(target, scratch) {
                let sb_gen = (1u64 << 62)
                    | ((self.scrollback.epoch() & 0x3FFF) << 48)
                    | ((self.scrollback.evicted_lines() + target as u64) & 0xFFFF_FFFF_FFFF);
                return (scratch.as_slice(), sb_gen, meta.host_id, meta.cwd_id, arts);
            }
        }
        let grid_row = target.saturating_sub(sb_len).min(self.rows() - 1);
        let row = self.primary.row(grid_row);
        let meta = self.primary.meta(grid_row);
        (row, meta.generation, meta.host_id, meta.cwd_id, arts)
    }

    /// Extract selected text cleanly (respecting soft-wrap joining and Block mode).
    pub fn extract_selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let (s, e) = sel.ordered();
        let evicted = self.scrollback.evicted_lines();
        let sb_len = self.scrollback.len() as u64;
        let screen_top = evicted + sb_len;
        let screen_end = screen_top + self.rows() as u64;

        let mut out = String::new();
        let mut scratch = Vec::new();
        let cols = self.cols();

        for abs_line in s.abs_line..=e.abs_line {
            if abs_line < evicted || abs_line >= screen_end {
                continue;
            }
            let (cells, wrapped): (&[Cell], bool) = if abs_line < screen_top {
                let sb_idx = (abs_line - evicted) as usize;
                scratch.resize(cols, Cell::BLANK);
                let meta = self.scrollback.copy_line_padded(sb_idx, &mut scratch)?;
                (scratch.as_slice(), meta.wrapped())
            } else {
                let r = (abs_line - screen_top) as usize;
                let g = self.active_grid();
                (g.row(r), g.meta(r).wrapped())
            };

            let (c_start, c_end) = match sel.mode {
                SelectionMode::Block => {
                    (sel.start.col.min(sel.end.col), sel.start.col.max(sel.end.col))
                }
                SelectionMode::Line => (0, cols.saturating_sub(1)),
                SelectionMode::Normal | SelectionMode::Word => {
                    let cs = if abs_line == s.abs_line { s.col } else { 0 };
                    let ce = if abs_line == e.abs_line {
                        e.col
                    } else {
                        cols.saturating_sub(1)
                    };
                    (cs, ce)
                }
            };

            let c_start = c_start.min(cols);
            let c_end = (c_end + 1).min(cols);
            let mut line_str = String::new();
            for cell in &cells[c_start..c_end] {
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                line_str.push(cell.ch);
            }
            if !wrapped || abs_line == e.abs_line {
                while line_str.ends_with(' ') {
                    line_str.pop();
                }
            }
            out.push_str(&line_str);
            if abs_line < e.abs_line && (!wrapped || sel.mode == SelectionMode::Block) {
                out.push('\n');
            }
        }
        Some(out)
    }

    /// Search across scrollback and active screen (case-insensitive substring or regex).
    pub fn search(&mut self, query: &str, regex_mode: bool, ignore_case: bool) -> usize {
        self.search_matches.clear();
        self.active_search_match = None;
        if query.is_empty() {
            return 0;
        }
        let re = if regex_mode {
            let pat = if ignore_case {
                format!("(?i){query}")
            } else {
                query.to_string()
            };
            match regex_lite::Regex::new(&pat) {
                Ok(r) => Some(r),
                Err(_) => return 0,
            }
        } else {
            None
        };
        let needle_lower = if ignore_case {
            query.to_lowercase()
        } else {
            query.to_string()
        };

        let evicted = self.scrollback.evicted_lines();
        let sb_len = self.scrollback.len();
        let mut matches = Vec::new();

        let mut scan_line = |abs_line: u64, text: &str| {
            if let Some(ref re) = re {
                for m in re.find_iter(text) {
                    let start_col = text[..m.start()].chars().count();
                    let len_chars = text[m.start()..m.end()].chars().count();
                    if len_chars > 0 {
                        matches.push(SearchMatch {
                            abs_line,
                            start_col,
                            end_col: start_col + len_chars,
                        });
                    }
                }
            } else {
                let hay = if ignore_case {
                    text.to_lowercase()
                } else {
                    text.to_string()
                };
                let mut offset = 0;
                while let Some(pos) = hay[offset..].find(&needle_lower) {
                    let byte_start = offset + pos;
                    let start_col = hay[..byte_start].chars().count();
                    let len_chars = needle_lower.chars().count();
                    matches.push(SearchMatch {
                        abs_line,
                        start_col,
                        end_col: start_col + len_chars,
                    });
                    offset = byte_start + needle_lower.len().max(1);
                }
            }
        };

        for idx in 0..sb_len {
            if let Some(line) = self.scrollback.line_text(idx) {
                scan_line(evicted + idx as u64, &line);
            }
        }
        let screen_top = evicted + sb_len as u64;
        for r in 0..self.rows() {
            let line = self.active_grid().row_text(r);
            scan_line(screen_top + r as u64, &line);
        }

        self.search_matches = matches;

        if !self.search_matches.is_empty() {
            self.active_search_match = Some(self.search_matches.len() - 1);
            self.scroll_to_active_match();
        }
        self.search_matches.len()
    }

    pub fn next_search_match(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let cur = self.active_search_match.unwrap_or(0);
        let len = self.search_matches.len();
        self.active_search_match = Some(if forward {
            (cur + 1) % len
        } else {
            (cur + len - 1) % len
        });
        self.scroll_to_active_match();
    }

    fn scroll_to_active_match(&mut self) {
        let Some(idx) = self.active_search_match else {
            return;
        };
        let Some(m) = self.search_matches.get(idx) else {
            return;
        };
        let screen_top = self.screen_top_abs_line();
        if m.abs_line < screen_top {
            let needed = (screen_top - m.abs_line) as usize + self.rows() / 3;
            self.viewport_offset = needed.min(self.scrollback.len());
        } else {
            self.viewport_offset = 0;
        }
    }

    /// Jump viewport to previous/next shell integration prompt mark (`OSC 133;A`).
    pub fn jump_to_prompt(&mut self, previous: bool) {
        let sb_len = self.scrollback.len();
        let cur_top = sb_len.saturating_sub(self.viewport_offset);
        if previous {
            for idx in (0..cur_top).rev() {
                if let Some((_, meta)) = self.scrollback.get(idx) {
                    if meta.prompt_mark() == Some(PromptMark::PromptStart) {
                        self.viewport_offset = sb_len - idx;
                        return;
                    }
                }
            }
        } else {
            for idx in (cur_top + 1)..sb_len {
                if let Some((_, meta)) = self.scrollback.get(idx) {
                    if meta.prompt_mark() == Some(PromptMark::PromptStart) {
                        self.viewport_offset = sb_len - idx;
                        return;
                    }
                }
            }
            self.viewport_offset = 0;
        }
    }

    /// Current Kitty keyboard enhancement flags (0 if disabled).
    #[inline]
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.kitty_kbd_stack.last().copied().unwrap_or(0)
    }

    // ---- Internal screen mutation helpers ----

    #[inline]
    fn linefeed(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up_internal(1);
        } else if self.cursor.row + 1 < self.rows() {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_down_internal(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn scroll_up_internal(&mut self, count: usize) {
        let is_alt = self.modes.contains(TermModes::ALT_SCREEN);
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let bg = self.pen.bg;
        let host = self.current_host_id;
        let cwd = self.current_cwd_id;
        let push_to_sb = !is_alt && top == 0 && bottom == self.primary.rows() - 1;

        if is_alt {
            self.alternate
                .scroll_region_up(top, bottom, count, bg, host, cwd, |_, _| {});
        } else {
            let sb = &mut self.scrollback;
            let was_scrolled = self.viewport_offset > 0;
            let mut evicted = 0usize;
            self.primary
                .scroll_region_up(top, bottom, count, bg, host, cwd, |cells, meta| {
                    if push_to_sb {
                        sb.push_row(cells, meta);
                        evicted += 1;
                    }
                });
            if was_scrolled && evicted > 0 {
                self.viewport_offset = (self.viewport_offset + evicted).min(self.scrollback.len());
            }
        }
    }

    fn scroll_down_internal(&mut self, count: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let bg = self.pen.bg;
        let host = self.current_host_id;
        let cwd = self.current_cwd_id;
        self.active_grid_mut()
            .scroll_region_down(top, bottom, count, bg, host, cwd);
    }

    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if save_cursor {
            self.saved_primary = SavedCursor {
                cursor: self.cursor,
                pen: self.pen,
                origin_mode: self.modes.contains(TermModes::ORIGIN),
                charsets: self.charsets,
                active_charset: self.active_charset,
            };
        }
        if !self.modes.contains(TermModes::ALT_SCREEN) {
            self.modes.insert(TermModes::ALT_SCREEN);
            self.viewport_offset = 0;
            let rows = self.alternate.rows();
            let host = self.current_host_id;
            let cwd = self.current_cwd_id;
            for r in 0..rows {
                self.alternate.clear_row(r, PackedColor::DEFAULT, host, cwd);
            }
            self.scroll_top = 0;
            self.scroll_bottom = rows - 1;
            self.cursor.col = 0;
            self.cursor.row = 0;
            self.cursor.pending_wrap = false;
        }
    }

    fn leave_alt_screen(&mut self, restore_cursor: bool) {
        if self.modes.contains(TermModes::ALT_SCREEN) {
            self.modes.remove(TermModes::ALT_SCREEN);
            let cols = self.primary.cols();
            let rows = self.primary.rows();
            self.scroll_top = 0;
            self.scroll_bottom = rows - 1;
            if restore_cursor {
                self.cursor = self.saved_primary.cursor;
                self.cursor.col = self.cursor.col.min(cols - 1);
                self.cursor.row = self.cursor.row.min(rows - 1);
                self.pen = self.saved_primary.pen;
                self.charsets = self.saved_primary.charsets;
                self.active_charset = self.saved_primary.active_charset;
                self.modes
                    .set(TermModes::ORIGIN, self.saved_primary.origin_mode);
            } else {
                self.cursor.col = self.cursor.col.min(cols - 1);
                self.cursor.row = self.cursor.row.min(rows - 1);
            }
            self.primary.mark_all_dirty();
        }
    }

    #[inline]
    fn apply_sgr(&mut self, params: &CsiParams) {
        let len = params.len();
        if len == 0 {
            self.pen.reset();
            if self.pen.hyperlink_id != 0 {
                self.sync_pen_extra();
            }
            return;
        }
        // Fast paths for the 3 hottest SGR sequences in real terminals (`\x1b[0m`, `\x1b[38;2;r;g;bm`, `\x1b[38;5;Nm`)!
        let p0 = params.get(0).unwrap_or(0);
        if len == 1 && p0 == 0 {
            self.pen.reset();
            if self.pen.hyperlink_id != 0 {
                self.sync_pen_extra();
            }
            return;
        }
        if len == 5 && p0 == 38 && params.get(1) == Some(2) {
            self.pen.fg = PackedColor::rgb(
                params.get(2).unwrap_or(0) as u8,
                params.get(3).unwrap_or(0) as u8,
                params.get(4).unwrap_or(0) as u8,
            );
            return;
        }
        if len == 3 && p0 == 38 && params.get(1) == Some(5) {
            self.pen.fg = PackedColor::indexed(params.get(2).unwrap_or(0) as u8);
            return;
        }

        let mut extra_dirty = false;
        let mut i = 0;
        while i < len {
            let p = params.get(i).unwrap_or(0);
            let grp = params.group_of(i);
            match p {
                0 => {
                    self.pen.reset();
                    if self.pen.hyperlink_id != 0 {
                        extra_dirty = true;
                    }
                }
                1 => self.pen.flags.insert(CellFlags::BOLD),
                2 => self.pen.flags.insert(CellFlags::FAINT),
                3 => self.pen.flags.insert(CellFlags::ITALIC),
                4 => {
                    self.pen.flags.remove(
                        CellFlags::UNDERLINE
                            | CellFlags::DOUBLE_UNDERLINE
                            | CellFlags::CURLY_UNDERLINE,
                    );
                    if i + 1 < len && params.group_of(i + 1) == grp {
                        let sub = params.get(i + 1).unwrap_or(1);
                        i += 1;
                        match sub {
                            0 => {}
                            2 => self.pen.flags.insert(CellFlags::DOUBLE_UNDERLINE),
                            3 | 4 | 5 => self.pen.flags.insert(CellFlags::CURLY_UNDERLINE),
                            _ => self.pen.flags.insert(CellFlags::UNDERLINE),
                        }
                    } else {
                        self.pen.flags.insert(CellFlags::UNDERLINE);
                    }
                }
                5 | 6 => self.pen.flags.insert(CellFlags::BLINK),
                7 => self.pen.flags.insert(CellFlags::INVERSE),
                8 => self.pen.flags.insert(CellFlags::HIDDEN),
                9 => self.pen.flags.insert(CellFlags::STRIKETHROUGH),
                21 => self.pen.flags.insert(CellFlags::DOUBLE_UNDERLINE),
                22 => self.pen.flags.remove(CellFlags::BOLD | CellFlags::FAINT),
                23 => self.pen.flags.remove(CellFlags::ITALIC),
                24 => self.pen.flags.remove(
                    CellFlags::UNDERLINE
                        | CellFlags::DOUBLE_UNDERLINE
                        | CellFlags::CURLY_UNDERLINE,
                ),
                25 => self.pen.flags.remove(CellFlags::BLINK),
                27 => self.pen.flags.remove(CellFlags::INVERSE),
                28 => self.pen.flags.remove(CellFlags::HIDDEN),
                29 => self.pen.flags.remove(CellFlags::STRIKETHROUGH),
                30..=37 => self.pen.fg = PackedColor::indexed((p - 30) as u8),
                38 => {
                    if let Some((color, consumed)) = parse_extended_color(params, i) {
                        self.pen.fg = color;
                        i += consumed;
                    }
                }
                39 => self.pen.fg = PackedColor::DEFAULT,
                40..=47 => self.pen.bg = PackedColor::indexed((p - 40) as u8),
                48 => {
                    if let Some((color, consumed)) = parse_extended_color(params, i) {
                        self.pen.bg = color;
                        i += consumed;
                    }
                }
                49 => self.pen.bg = PackedColor::DEFAULT,
                58 => {
                    if let Some((color, consumed)) = parse_extended_color(params, i) {
                        self.pen.underline_color = color;
                        extra_dirty = true;
                        i += consumed;
                    }
                }
                59 => {
                    self.pen.underline_color = PackedColor::DEFAULT;
                    extra_dirty = true;
                }
                90..=97 => self.pen.fg = PackedColor::indexed((p - 90 + 8) as u8),
                100..=107 => self.pen.bg = PackedColor::indexed((p - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
        if extra_dirty {
            self.sync_pen_extra();
        }
    }

    fn set_private_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            1 => self.modes.set(TermModes::APPLICATION_CURSOR, enable),
            5 => {
                self.modes.set(TermModes::REVERSE_VIDEO, enable);
                self.active_grid_mut().mark_all_dirty();
            }
            6 => {
                self.modes.set(TermModes::ORIGIN, enable);
                self.cursor.col = 0;
                self.cursor.row = if enable { self.scroll_top } else { 0 };
                self.cursor.pending_wrap = false;
            }
            7 => self.modes.set(TermModes::AUTO_WRAP, enable),
            9 => self.modes.set(TermModes::MOUSE_X10, enable),
            25 => self.modes.set(TermModes::CURSOR_VISIBLE, enable),
            47 | 1047 => {
                if enable {
                    self.enter_alt_screen(false);
                } else {
                    self.leave_alt_screen(false);
                }
            }
            1000 => self.modes.set(TermModes::MOUSE_VT200, enable),
            1002 => self.modes.set(TermModes::MOUSE_BUTTON, enable),
            1003 => self.modes.set(TermModes::MOUSE_ANY, enable),
            1004 => self.modes.set(TermModes::FOCUS_EVENTS, enable),
            1006 => self.modes.set(TermModes::MOUSE_SGR, enable),
            1007 => self.modes.set(TermModes::ALT_SCROLL, enable),
            1048 => {
                if enable {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1049 => {
                if enable {
                    self.enter_alt_screen(true);
                } else {
                    self.leave_alt_screen(true);
                }
            }
            2004 => self.modes.set(TermModes::BRACKETED_PASTE, enable),
            2026 => self.modes.set(TermModes::SYNC_OUTPUT, enable),
            _ => {}
        }
    }

    fn save_cursor(&mut self) {
        let saved = SavedCursor {
            cursor: self.cursor,
            pen: self.pen,
            origin_mode: self.modes.contains(TermModes::ORIGIN),
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
        if self.modes.contains(TermModes::ALT_SCREEN) {
            self.saved_alt = saved;
        } else {
            self.saved_primary = saved;
        }
    }

    fn restore_cursor(&mut self) {
        let saved = if self.modes.contains(TermModes::ALT_SCREEN) {
            self.saved_alt
        } else {
            self.saved_primary
        };
        let cols = self.cols();
        let rows = self.rows();
        self.cursor = saved.cursor;
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.pen = saved.pen;
        self.charsets = saved.charsets;
        self.active_charset = saved.active_charset;
        self.modes.set(TermModes::ORIGIN, saved.origin_mode);
    }

    fn place_inline_image(&mut self, img: InlineImage) {
        let img_id = img.id;
        let img_cols = img.cols as usize;
        let img_rows = img.rows as usize;
        if self.images.len() >= 64 {
            self.gc_side_tables();
        }
        self.images.insert(img_id, img);

        // Ensure cursor starts cleanly; scroll as needed to fit `img_rows` rows.
        let grid_cols = self.cols();
        for tile_r in 0..img_rows {
            if tile_r > 0 {
                self.linefeed();
            }
            let row_idx = self.cursor.row;
            let start_col = self.cursor.col;
            let host = self.current_host_id;
            let cwd = self.current_cwd_id;

            let max_c = (start_col + img_cols).min(grid_cols);
            let mut ids = Vec::with_capacity(max_c.saturating_sub(start_col));
            for c in start_col..max_c {
                let ref_id = self.image_cells.len() as u32;
                self.image_cells.push(ImageCellRef {
                    image_id: img_id,
                    tile_col: (c - start_col) as u16,
                    tile_row: tile_r as u16,
                    total_cols: img_cols as u16,
                    total_rows: img_rows as u16,
                });
                let extra_id = self.intern_cell_extra(CellExtra {
                    underline_color: PackedColor::DEFAULT,
                    hyperlink_id: 0,
                    image_ref_id: ref_id,
                    grapheme_tail_id: 0,
                });
                ids.push((c, extra_id));
            }
            let (row_cells, meta) = self.active_grid_mut().row_and_meta_mut(row_idx);
            meta.set_has_wide_or_image(true);
            meta.used_cols = meta.used_cols.max(max_c as u16);
            meta.host_id = host;
            meta.cwd_id = cwd;
            for (c, extra_id) in ids {
                row_cells[c] = Cell {
                    ch: ' ',
                    fg: PackedColor::DEFAULT,
                    bg: PackedColor::DEFAULT,
                    flags: CellFlags::IMAGE_CELL,
                    extra_id,
                };
            }
        }
        self.cursor.col = (self.cursor.col + img_cols).min(grid_cols - 1);
    }

    fn handle_osc_1337(&mut self, rest: &str) {
        if let Some(host) = rest.strip_prefix("RemoteHost=") {
            self.current_host_id = self.strings.intern(host);
            let r = self.cursor.row;
            self.active_grid_mut().meta_mut(r).host_id = self.current_host_id;
            self.events
                .push(TerminalEvent::RemoteHostChanged(host.to_string()));
        } else if let Some(cwd) = rest.strip_prefix("CurrentDir=") {
            self.current_cwd_id = self.strings.intern(cwd);
            let r = self.cursor.row;
            self.active_grid_mut().meta_mut(r).cwd_id = self.current_cwd_id;
            self.events
                .push(TerminalEvent::CurrentDirChanged(cwd.to_string()));
        } else if let Some(ver) = rest.strip_prefix("ShellIntegrationVersion=") {
            self.shell_integration_version = Some(ver.to_string());
        } else if let Some(kv) = rest.strip_prefix("SetUserVar=") {
            if let Some((k, b64)) = kv.split_once('=') {
                let val = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_else(|| b64.to_string());
                self.user_vars.insert(k.to_string(), val.clone());
                self.events.push(TerminalEvent::UserVarChanged {
                    key: k.to_string(),
                    value: val,
                });
            }
        } else if let Some(art_str) = rest.strip_prefix("Artifact=") {
            if let Some(art) = parse_remote_artifact(art_str, self.cursor.col) {
                let abs_line = self.screen_top_abs_line() + self.cursor.row as u64;
                self.artifacts.entry(abs_line).or_default().push(art);
            }
        } else if let Some(file_spec) = rest.strip_prefix("File=") {
            if let Some((params_str, b64)) = file_spec.split_once(':') {
                let params = FileTransferParams::parse(params_str);
                let id = self.next_image_id;
                self.next_image_id += 1;
                if let Some(decoded) = decode_transfer(
                    id,
                    params,
                    b64.as_bytes(),
                    self.cell_width_px,
                    self.cell_height_px,
                    self.cols(),
                    self.rows(),
                ) {
                    match decoded {
                        DecodedTransfer::InlineImage(img) => self.place_inline_image(img),
                        DecodedTransfer::DownloadFile { name, data } => {
                            self.events.push(TerminalEvent::FileDownload { name, data });
                        }
                    }
                }
            }
        } else if let Some(mp_spec) = rest.strip_prefix("MultipartFile=") {
            let params = FileTransferParams::parse(mp_spec);
            let cap = params.size.map(|s| (s * 4) / 3 + 16).unwrap_or(4096).min(16 * 1024 * 1024);
            self.multipart = Some(MultipartTransfer {
                params,
                b64_buf: Vec::with_capacity(cap),
            });
        } else if let Some(part) = rest.strip_prefix("FilePart=") {
            if let Some(mp) = &mut self.multipart {
                mp.b64_buf.extend_from_slice(part.as_bytes());
            }
        } else if rest == "FileEnd" {
            if let Some(mp) = self.multipart.take() {
                let id = self.next_image_id;
                self.next_image_id += 1;
                if let Some(decoded) = decode_transfer(
                    id,
                    mp.params,
                    &mp.b64_buf,
                    self.cell_width_px,
                    self.cell_height_px,
                    self.cols(),
                    self.rows(),
                ) {
                    match decoded {
                        DecodedTransfer::InlineImage(img) => self.place_inline_image(img),
                        DecodedTransfer::DownloadFile { name, data } => {
                            self.events.push(TerminalEvent::FileDownload { name, data });
                        }
                    }
                }
            }
        } else if let Some(shape) = rest.strip_prefix("CursorShape=") {
            self.cursor.style = match shape.trim() {
                "1" => CursorStyle::VerticalBar,
                "2" => CursorStyle::Underline,
                _ => CursorStyle::Block,
            };
        } else if let Some(profile) = rest.strip_prefix("SetProfile=") {
            self.events
                .push(TerminalEvent::ProfileChangeRequested(profile.to_string()));
        } else if rest.starts_with("RequestAttention=") {
            self.events.push(TerminalEvent::RequestAttention);
        }
    }

    fn handle_osc_6(&mut self, rest: &str) {
        // iTerm2 tab color:
        //   6;1;bg;red;brightness;255
        //   6;1;bg;green;brightness;145
        //   6;1;bg;blue;brightness;117
        //   6;1;bg;*;default
        let parts: Vec<&str> = rest.split(';').collect();
        if parts.len() >= 4 && parts[0] == "1" && parts[1] == "bg" {
            if parts[2] == "*" && parts[3] == "default" {
                self.tab_r = None;
                self.tab_g = None;
                self.tab_b = None;
                self.tab_color = None;
                self.events.push(TerminalEvent::TabColorChanged(None));
            } else if parts.len() >= 5 && parts[3] == "brightness" {
                if let Ok(v) = parts[4].trim().parse::<u8>() {
                    match parts[2] {
                        "red" => self.tab_r = Some(v),
                        "green" => self.tab_g = Some(v),
                        "blue" => self.tab_b = Some(v),
                        _ => return,
                    }
                    let color = Rgba::from_u8(
                        self.tab_r.unwrap_or(0),
                        self.tab_g.unwrap_or(0),
                        self.tab_b.unwrap_or(0),
                        255,
                    );
                    self.tab_color = Some(color);
                    self.events.push(TerminalEvent::TabColorChanged(Some(color)));
                }
            }
        }
    }
}

/// Parse SGR 38/48/58 extended color in either `;` or `:` syntax.
/// Returns `(PackedColor, extra_params_consumed)`.
fn parse_extended_color(params: &CsiParams, idx: usize) -> Option<(PackedColor, usize)> {
    let kind = params.get(idx + 1)?;
    match kind {
        5 => {
            let c = params.get(idx + 2)? as u8;
            Some((PackedColor::indexed(c), 2))
        }
        2 => {
            let grp = params.group_of(idx);
            // Colon syntax `38:2:<colorspace>:r:g:b` has 4+ subparams in same group!
            if idx + 5 < params.len() && params.group_of(idx + 5) == grp {
                let r = params.get(idx + 3)? as u8;
                let g = params.get(idx + 4)? as u8;
                let b = params.get(idx + 5)? as u8;
                Some((PackedColor::rgb(r, g, b), 5))
            } else {
                let r = params.get(idx + 2)? as u8;
                let g = params.get(idx + 3)? as u8;
                let b = params.get(idx + 4)? as u8;
                Some((PackedColor::rgb(r, g, b), 4))
            }
        }
        _ => None,
    }
}

impl VtSink for TerminalScreen {
    /// Bulk multi-line plain-ASCII fast path: archives lines guaranteed to scroll off-screen
    /// directly into packed 1-byte/char scrollback slabs (ZERO 16-byte `Cell` expansion,
    /// ZERO `ActiveGrid` writes/clears, ZERO `row_order` rotations for scrolled-past lines!).
    /// 100% bit-for-bit identical to line-by-line VT100 execution.
    #[inline]
    fn try_bulk_ascii_lines(&mut self, bytes: &[u8]) -> usize {
        // 1-instruction reject for escape-prefixed lines (e.g. `\x1b[38;2;...m`) before any work!
        if bytes.len() < 2048
            || !(0x20..=0x7E).contains(&bytes[0])
            || self.pending_zwj
            || !self.modes.contains(TermModes::AUTO_WRAP)
            || self.modes.contains(TermModes::INSERT_MODE)
            || self.cursor.col != 0
            || self.cursor.pending_wrap
            || self.charsets[self.active_charset as usize] != Charset::Ascii
            || !self.pen.fg.is_default()
            || !self.pen.bg.is_default()
            || !self.pen.flags.is_empty()
            || self.pen.cached_extra_id != 0
        {
            return 0;
        }
        let is_alt = self.modes.contains(TermModes::ALT_SCREEN);
        let rows = self.rows();
        let cols = self.cols();
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        if bottom < top || bottom >= rows || self.cursor.row != bottom {
            return 0;
        }
        let push_to_sb = !is_alt && top == 0 && bottom == rows - 1;
        if self.active_grid().meta(bottom).used_cols != 0 {
            return 0;
        }

        let lf_is_crlf = self.modes.contains(TermModes::LINEFEED_NEWLINE);
        let len = bytes.len();
        let mut pos = 0usize;
        let mut vlines = std::mem::take(&mut self.bulk_lines_scratch);
        vlines.clear();

        while pos < len {
            let line_start = pos;
            let ascii_len = crate::parser::VtParser::scan_printable_ascii(&bytes[pos..]);
            let line_end = pos + ascii_len;
            if line_end >= len {
                break;
            }
            let term = bytes[line_end];
            let next_pos = if term == b'\r' && line_end + 1 < len && bytes[line_end + 1] == b'\n' {
                line_end + 2
            } else if term == b'\n' && lf_is_crlf {
                line_end + 1
            } else {
                break;
            };

            let mut s = line_start;
            while line_end - s > cols {
                vlines.push((s, s + cols, true));
                s += cols;
            }
            vlines.push((s, line_end, false));
            pos = next_pos;
        }

        let region_len = bottom - top + 1;
        let keep_in_region = region_len - 1;
        let total_vlines = vlines.len();
        if total_vlines <= keep_in_region {
            self.bulk_lines_scratch = vlines;
            return 0;
        }

        let host_id = self.current_host_id;
        let cwd_id = self.current_cwd_id;
        let direct_drop_end = total_vlines - keep_in_region;

        if push_to_sb {
            for r in 0..keep_in_region {
                let row_slice = self.primary.row(r);
                let meta = *self.primary.meta(r);
                self.scrollback.push_row(row_slice, &meta);
            }
            for &(s, e, wrapped) in &vlines[..direct_drop_end] {
                self.scrollback
                    .push_raw_ascii_line(&bytes[s..e], wrapped, host_id, cwd_id);
            }
        }

        let grid = self.active_grid_mut();
        for (i, &(s, e, wrapped)) in vlines[direct_drop_end..].iter().enumerate() {
            let dst_r = top + i;
            grid.clear_row(dst_r, PackedColor::DEFAULT, host_id, cwd_id);
            let chunk = &bytes[s..e];
            if !chunk.is_empty() || wrapped {
                let (dst_cells, meta) = grid.row_and_meta_mut(dst_r);
                for (cell, &b) in dst_cells[..chunk.len()].iter_mut().zip(chunk.iter()) {
                    *cell = Cell {
                        ch: b as char,
                        ..Cell::BLANK
                    };
                }
                meta.used_cols = chunk.len() as u16;
                meta.set_wrapped(wrapped);
                meta.host_id = host_id;
                meta.cwd_id = cwd_id;
            }
        }
        grid.clear_row(bottom, PackedColor::DEFAULT, host_id, cwd_id);
        self.cursor.row = bottom;
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
        if push_to_sb && self.viewport_offset > 0 {
            self.viewport_offset = (self.viewport_offset + total_vlines).min(self.scrollback.len());
        }
        self.bulk_lines_scratch = vlines;
        pos
    }

    /// Ultra-fast bulk ASCII run writer: writes directly into contiguous `&mut [Cell]`
    /// with ONE generation bump per line instead of per character!
    #[inline]
    fn print_ascii_run(&mut self, mut bytes: &[u8]) {
        if self.pending_zwj {
            self.pending_zwj = false;
        }
        let cols = self.cols();
        let insert_mode = self.modes.contains(TermModes::INSERT_MODE);
        let auto_wrap = self.modes.contains(TermModes::AUTO_WRAP);
        let template = self.pen.make_cell(' ');
        let is_styled = !template.fg.is_default()
            || !template.bg.is_default()
            || !template.flags.is_empty()
            || template.extra_id != 0;
        let host_id = self.current_host_id;
        let cwd_id = self.current_cwd_id;
        let dec_gfx = self.charsets[self.active_charset as usize] == Charset::DecSpecialGraphics;

        while !bytes.is_empty() {
            if self.cursor.pending_wrap {
                if auto_wrap {
                    let r = self.cursor.row;
                    self.active_grid_mut().meta_mut(r).set_wrapped(true);
                    self.cursor.col = 0;
                    self.linefeed();
                } else {
                    self.cursor.col = cols - 1;
                    self.cursor.pending_wrap = false;
                }
            }

            let row_idx = self.cursor.row;
            let col = self.cursor.col;
            let avail = cols - col;
            let chunk_len = bytes.len().min(avail);
            let chunk = &bytes[..chunk_len];

            let (row_cells, meta) = self.active_grid_mut().row_and_meta_mut(row_idx);
            if is_styled {
                meta.set_styled(true);
            }
            if meta.host_id == 0 && host_id != 0 {
                meta.host_id = host_id;
            }
            if meta.cwd_id == 0 && cwd_id != 0 {
                meta.cwd_id = cwd_id;
            }

            if insert_mode {
                let shift_end = cols - chunk_len;
                if col < shift_end {
                    row_cells.copy_within(col..shift_end, col + chunk_len);
                }
                meta.used_cols = cols as u16;
            } else {
                meta.used_cols = meta.used_cols.max((col + chunk_len) as u16);
            }

            // Tight inner loop: LLVM auto-vectorizes plain ASCII; translates DEC Special Graphics when active (htop/ncurses!)
            let dst = &mut row_cells[col..col + chunk_len];
            if dec_gfx {
                meta.set_has_non_ascii(true);
                for (cell, &b) in dst.iter_mut().zip(chunk.iter()) {
                    *cell = Cell {
                        ch: translate_dec_graphics(b),
                        ..template
                    };
                }
            } else {
                for (cell, &b) in dst.iter_mut().zip(chunk.iter()) {
                    *cell = Cell {
                        ch: b as char,
                        ..template
                    };
                }
            }

            bytes = &bytes[chunk_len..];
            if col + chunk_len >= cols {
                self.cursor.col = cols - 1;
                self.cursor.pending_wrap = true;
            } else {
                self.cursor.col = col + chunk_len;
            }
        }
    }

    /// Ultra-fast bulk UTF-8 writer: ONE row/meta lookup & ONE generation bump per line,
    /// hoisted slice pointer inside inner char loop, and O(1) BMP width fast path!
    fn print_utf8_run(&mut self, s: &str) {
        let cols = self.cols();
        let auto_wrap = self.modes.contains(TermModes::AUTO_WRAP);
        let pen = self.pen;
        let host_id = self.current_host_id;
        let cwd_id = self.current_cwd_id;

        let mut chars = s.chars();
        let mut pending_ch: Option<char> = None;

        loop {
            let first = match pending_ch.take().or_else(|| chars.next()) {
                Some(c) => c,
                None => break,
            };
            if self.pending_zwj || is_combining_or_joiner(first) {
                self.pending_zwj = first == '\u{200D}';
                self.attach_combining_char(first);
                continue;
            }
            let first_w = fast_char_width(first);
            if first_w == 0 {
                continue;
            }

            let mut row_idx = self.cursor.row.min(self.rows() - 1);
            let mut col = self.cursor.col.min(cols - 1);
            let mut pending_wrap = self.cursor.pending_wrap;
            let (mut row_cells, mut meta) = self.active_grid_mut().row_and_meta_mut(row_idx);
            meta.set_has_non_ascii(true);
            if meta.host_id == 0 && host_id != 0 {
                meta.host_id = host_id;
            }
            if meta.cwd_id == 0 && cwd_id != 0 {
                meta.cwd_id = cwd_id;
            }
            let mut used_cols = meta.used_cols as usize;
            let mut has_wide = false;
            let tpl = pen.make_cell(' ');
            let spacer_tpl = Cell {
                ch: ' ',
                fg: pen.fg,
                bg: pen.bg,
                flags: CellFlags::WIDE_SPACER,
                extra_id: 0,
            };

            let mut cur_opt = Some((first, first_w));
            while let Some((ch, width)) = cur_opt {
                if pending_wrap || (width == 2 && col + 1 >= cols) {
                    meta.used_cols = used_cols as u16;
                    if has_wide {
                        meta.set_has_wide_or_image(true);
                    }
                    if auto_wrap {
                        meta.set_wrapped(true);
                        self.cursor.col = 0;
                        self.cursor.pending_wrap = false;
                        self.linefeed();
                        row_idx = self.cursor.row.min(self.rows() - 1);
                        col = 0;
                        pending_wrap = false;
                        let pair = self.active_grid_mut().row_and_meta_mut(row_idx);
                        row_cells = pair.0;
                        meta = pair.1;
                        meta.set_has_non_ascii(true);
                        if meta.host_id == 0 && host_id != 0 {
                            meta.host_id = host_id;
                        }
                        if meta.cwd_id == 0 && cwd_id != 0 {
                            meta.cwd_id = cwd_id;
                        }
                        used_cols = meta.used_cols as usize;
                        has_wide = false;
                    } else {
                        col = cols.saturating_sub(width);
                        pending_wrap = false;
                    }
                }

                // Safety: `col < cols` is guaranteed above (and `col + 1 < cols` when `width == 2`).
                unsafe {
                    if col > 0 && row_cells.get_unchecked(col).flags.contains(CellFlags::WIDE_SPACER) {
                        *row_cells.get_unchecked_mut(col - 1) = Cell::blank_with_bg(pen.bg);
                    }
                    let mut cell = Cell { ch, ..tpl };
                    if width == 2 {
                        cell.flags.insert(CellFlags::WIDE_CHAR);
                        has_wide = true;
                        *row_cells.get_unchecked_mut(col) = cell;
                        if col + 1 < cols {
                            *row_cells.get_unchecked_mut(col + 1) = spacer_tpl;
                        }
                    } else {
                        *row_cells.get_unchecked_mut(col) = cell;
                    }
                }
                let end_col = (col + width).min(cols);
                if end_col > used_cols {
                    used_cols = end_col;
                }

                if col + width >= cols {
                    col = cols - 1;
                    pending_wrap = true;
                } else {
                    col += width;
                }

                cur_opt = match chars.next() {
                    Some(nch) => {
                        if is_combining_or_joiner(nch) {
                            pending_ch = Some(nch);
                            None
                        } else {
                            let nw = fast_char_width(nch);
                            if nw == 0 {
                                pending_ch = Some(nch);
                                None
                            } else {
                                Some((nch, nw))
                            }
                        }
                    }
                    None => None,
                };
            }

            meta.used_cols = used_cols as u16;
            if has_wide {
                meta.set_has_wide_or_image(true);
            }
            self.cursor.row = row_idx;
            self.cursor.col = col;
            self.cursor.pending_wrap = pending_wrap;
        }
    }

    fn print_char(&mut self, ch: char) {
        if is_combining_or_joiner(ch) || self.pending_zwj {
            self.pending_zwj = ch == '\u{200D}';
            self.attach_combining_char(ch);
            return;
        }
        let width = fast_char_width(ch);
        if width == 0 {
            return;
        }
        self.pending_zwj = false;
        let cols = self.cols();
        let auto_wrap = self.modes.contains(TermModes::AUTO_WRAP);
        if self.cursor.pending_wrap || (width == 2 && self.cursor.col + 1 >= cols) {
            if auto_wrap {
                let r = self.cursor.row;
                self.active_grid_mut().meta_mut(r).set_wrapped(true);
                self.cursor.col = 0;
                self.linefeed();
            } else {
                self.cursor.col = cols.saturating_sub(width);
                self.cursor.pending_wrap = false;
            }
        }

        let row_idx = self.cursor.row.min(self.rows() - 1);
        let col = self.cursor.col.min(cols - 1);
        let pen = self.pen;
        let mut cell = pen.make_cell(ch);
        if width == 2 {
            cell.flags.insert(CellFlags::WIDE_CHAR);
        }
        let host_id = self.current_host_id;
        let cwd_id = self.current_cwd_id;

        let (row_cells, meta) = self.active_grid_mut().row_and_meta_mut(row_idx);
        meta.set_has_non_ascii(true);
        meta.used_cols = meta.used_cols.max((col + width).min(cols) as u16);
        if width == 2 {
            meta.set_has_wide_or_image(true);
        }
        if meta.host_id == 0 && host_id != 0 {
            meta.host_id = host_id;
        }
        if meta.cwd_id == 0 && cwd_id != 0 {
            meta.cwd_id = cwd_id;
        }

        // Clean up any overwritten wide char pair
        if row_cells[col].flags.contains(CellFlags::WIDE_SPACER) && col > 0 {
            row_cells[col - 1] = Cell::blank_with_bg(pen.bg);
        }
        row_cells[col] = cell;
        if width == 2 && col + 1 < cols {
            row_cells[col + 1] = Cell {
                ch: ' ',
                fg: pen.fg,
                bg: pen.bg,
                flags: CellFlags::WIDE_SPACER,
                extra_id: 0,
            };
        }

        if col + width >= cols {
            self.cursor.col = cols - 1;
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.col = col + width;
        }
    }

    #[inline]
    fn execute(&mut self, byte: u8) {
        self.pending_zwj = false;
        match byte {
            0x07 => self.events.push(TerminalEvent::Bell),
            0x08 => {
                self.cursor.pending_wrap = false;
                self.cursor.col = self.cursor.col.saturating_sub(1);
            }
            0x09 => {
                self.cursor.pending_wrap = false;
                let cols = self.cols();
                let mut c = self.cursor.col + 1;
                while c < cols && !self.tab_stops.get(c).copied().unwrap_or(false) {
                    c += 1;
                }
                self.cursor.col = c.min(cols - 1);
            }
            0x0A | 0x0B | 0x0C => {
                if self.modes.contains(TermModes::LINEFEED_NEWLINE) {
                    self.cursor.col = 0;
                }
                self.linefeed();
            }
            0x0D => {
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            0x0E => {
                // SO (Shift Out): activate G1 character set
                self.active_charset = 1;
            }
            0x0F => {
                // SI (Shift In): activate G0 character set
                self.active_charset = 0;
            }
            _ => {}
        }
    }

    #[inline(always)]
    fn csi_sgr_reset(&mut self) {
        self.pending_zwj = false;
        self.pen.reset();
        if self.pen.hyperlink_id != 0 {
            self.sync_pen_extra();
        }
    }

    #[inline(always)]
    fn csi_sgr_single(&mut self, code: u16) {
        self.pending_zwj = false;
        match code {
            0 => {
                self.pen.reset();
                if self.pen.hyperlink_id != 0 {
                    self.sync_pen_extra();
                }
            }
            1 => self.pen.flags.insert(CellFlags::BOLD),
            2 => self.pen.flags.insert(CellFlags::FAINT),
            3 => self.pen.flags.insert(CellFlags::ITALIC),
            4 => {
                self.pen
                    .flags
                    .remove(CellFlags::DOUBLE_UNDERLINE | CellFlags::CURLY_UNDERLINE);
                self.pen.flags.insert(CellFlags::UNDERLINE);
            }
            5 | 6 => self.pen.flags.insert(CellFlags::BLINK),
            7 => self.pen.flags.insert(CellFlags::INVERSE),
            8 => self.pen.flags.insert(CellFlags::HIDDEN),
            9 => self.pen.flags.insert(CellFlags::STRIKETHROUGH),
            21 => {
                self.pen
                    .flags
                    .remove(CellFlags::UNDERLINE | CellFlags::CURLY_UNDERLINE);
                self.pen.flags.insert(CellFlags::DOUBLE_UNDERLINE);
            }
            22 => self.pen.flags.remove(CellFlags::BOLD | CellFlags::FAINT),
            23 => self.pen.flags.remove(CellFlags::ITALIC),
            24 => self.pen.flags.remove(
                CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDERLINE | CellFlags::CURLY_UNDERLINE,
            ),
            25 => self.pen.flags.remove(CellFlags::BLINK),
            27 => self.pen.flags.remove(CellFlags::INVERSE),
            28 => self.pen.flags.remove(CellFlags::HIDDEN),
            29 => self.pen.flags.remove(CellFlags::STRIKETHROUGH),
            30..=37 => self.pen.fg = PackedColor::indexed((code - 30) as u8),
            39 => self.pen.fg = PackedColor::DEFAULT,
            40..=47 => self.pen.bg = PackedColor::indexed((code - 40) as u8),
            49 => self.pen.bg = PackedColor::DEFAULT,
            90..=97 => self.pen.fg = PackedColor::indexed((code - 90 + 8) as u8),
            100..=107 => self.pen.bg = PackedColor::indexed((code - 100 + 8) as u8),
            _ => {
                let mut p = CsiParams::new();
                p.push_raw(code, true);
                self.apply_sgr(&p);
            }
        }
    }

    #[inline(always)]
    fn csi_sgr_rgb(&mut self, is_bg: bool, r: u8, g: u8, b: u8) {
        self.pending_zwj = false;
        let c = PackedColor::rgb(r, g, b);
        if is_bg {
            self.pen.bg = c;
        } else {
            self.pen.fg = c;
        }
    }

    #[inline(always)]
    fn csi_sgr_indexed(&mut self, is_bg: bool, idx: u8) {
        self.pending_zwj = false;
        let c = PackedColor::indexed(idx);
        if is_bg {
            self.pen.bg = c;
        } else {
            self.pen.fg = c;
        }
    }

    #[inline(always)]
    fn csi_sgr_slice(&mut self, params: &[u16]) {
        self.pending_zwj = false;
        let len = params.len();
        let mut i = 0;
        let mut extra_dirty = false;
        while i < len {
            let p = params[i];
            match p {
                0 => {
                    self.pen.reset();
                    if self.pen.hyperlink_id != 0 {
                        extra_dirty = true;
                    }
                }
                1 => self.pen.flags.insert(CellFlags::BOLD),
                2 => self.pen.flags.insert(CellFlags::FAINT),
                3 => self.pen.flags.insert(CellFlags::ITALIC),
                4 => {
                    self.pen
                        .flags
                        .remove(CellFlags::DOUBLE_UNDERLINE | CellFlags::CURLY_UNDERLINE);
                    self.pen.flags.insert(CellFlags::UNDERLINE);
                }
                5 | 6 => self.pen.flags.insert(CellFlags::BLINK),
                7 => self.pen.flags.insert(CellFlags::INVERSE),
                8 => self.pen.flags.insert(CellFlags::HIDDEN),
                9 => self.pen.flags.insert(CellFlags::STRIKETHROUGH),
                21 => {
                    self.pen
                        .flags
                        .remove(CellFlags::UNDERLINE | CellFlags::CURLY_UNDERLINE);
                    self.pen.flags.insert(CellFlags::DOUBLE_UNDERLINE);
                }
                22 => self.pen.flags.remove(CellFlags::BOLD | CellFlags::FAINT),
                23 => self.pen.flags.remove(CellFlags::ITALIC),
                24 => self.pen.flags.remove(
                    CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDERLINE | CellFlags::CURLY_UNDERLINE,
                ),
                25 => self.pen.flags.remove(CellFlags::BLINK),
                27 => self.pen.flags.remove(CellFlags::INVERSE),
                28 => self.pen.flags.remove(CellFlags::HIDDEN),
                29 => self.pen.flags.remove(CellFlags::STRIKETHROUGH),
                30..=37 => self.pen.fg = PackedColor::indexed((p - 30) as u8),
                38 => {
                    if i + 2 < len && params[i + 1] == 5 {
                        self.pen.fg = PackedColor::indexed(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < len && params[i + 1] == 2 {
                        self.pen.fg = PackedColor::rgb(
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        );
                        i += 4;
                    }
                }
                39 => self.pen.fg = PackedColor::DEFAULT,
                40..=47 => self.pen.bg = PackedColor::indexed((p - 40) as u8),
                48 => {
                    if i + 2 < len && params[i + 1] == 5 {
                        self.pen.bg = PackedColor::indexed(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < len && params[i + 1] == 2 {
                        self.pen.bg = PackedColor::rgb(
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        );
                        i += 4;
                    }
                }
                49 => self.pen.bg = PackedColor::DEFAULT,
                58 => {
                    if i + 2 < len && params[i + 1] == 5 {
                        self.pen.underline_color = PackedColor::indexed(params[i + 2] as u8);
                        extra_dirty = true;
                        i += 2;
                    } else if i + 4 < len && params[i + 1] == 2 {
                        self.pen.underline_color = PackedColor::rgb(
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        );
                        extra_dirty = true;
                        i += 4;
                    }
                }
                59 => {
                    self.pen.underline_color = PackedColor::DEFAULT;
                    extra_dirty = true;
                }
                90..=97 => self.pen.fg = PackedColor::indexed((p - 90 + 8) as u8),
                100..=107 => self.pen.bg = PackedColor::indexed((p - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
        if extra_dirty {
            self.sync_pen_extra();
        }
    }

    #[inline(always)]
    fn csi_el(&mut self, mode: u16) {
        self.pending_zwj = false;
        let r = self.cursor.row;
        let c = self.cursor.col;
        let cols = self.cols();
        let bg = self.pen.bg;
        let grid = self.active_grid_mut();
        match mode {
            0 => grid.clear_cols(r, c, cols, bg),
            1 => grid.clear_cols(r, 0, c + 1, bg),
            2 => grid.clear_cols(r, 0, cols, bg),
            _ => {}
        }
    }

    #[inline(always)]
    fn csi_cup(&mut self, row1: u16, col1: u16) {
        self.pending_zwj = false;
        let r = (if row1 == 0 { 1 } else { row1 } as usize) - 1;
        let c = (if col1 == 0 { 1 } else { col1 } as usize) - 1;
        let rows = self.rows();
        let cols = self.cols();
        let (base_r, max_r) = if self.modes.contains(TermModes::ORIGIN) {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, rows - 1)
        };
        self.cursor.row = (base_r + r).min(max_r);
        self.cursor.col = c.min(cols - 1);
        self.cursor.pending_wrap = false;
    }

    fn csi_dispatch(&mut self, params: &CsiParams, intermediates: &[u8], final_byte: u8) {
        self.pending_zwj = false;

        // Handle multi-intermediate CSI sequences before single-private dispatch:
        // DECRQM (CSI ? Ps $ p) — queried by neovim, tmux, helix, zellij for mode support!
        if intermediates == [b'?', b'$'] && final_byte == b'p' {
            let mode = params.get(0).unwrap_or(0);
            let pm: u8 = match mode {
                1 => if self.modes.contains(TermModes::APPLICATION_CURSOR) { 1 } else { 2 },
                5 => if self.modes.contains(TermModes::REVERSE_VIDEO) { 1 } else { 2 },
                6 => if self.modes.contains(TermModes::ORIGIN) { 1 } else { 2 },
                7 => if self.modes.contains(TermModes::AUTO_WRAP) { 1 } else { 2 },
                25 => if self.modes.contains(TermModes::CURSOR_VISIBLE) { 1 } else { 2 },
                1000 => if self.modes.contains(TermModes::MOUSE_VT200) { 1 } else { 2 },
                1002 => if self.modes.contains(TermModes::MOUSE_BUTTON) { 1 } else { 2 },
                1003 => if self.modes.contains(TermModes::MOUSE_ANY) { 1 } else { 2 },
                1004 => if self.modes.contains(TermModes::FOCUS_EVENTS) { 1 } else { 2 },
                1006 => if self.modes.contains(TermModes::MOUSE_SGR) { 1 } else { 2 },
                1007 => if self.modes.contains(TermModes::ALT_SCROLL) { 1 } else { 2 },
                1049 => if self.modes.contains(TermModes::ALT_SCREEN) { 1 } else { 2 },
                2004 => if self.modes.contains(TermModes::BRACKETED_PASTE) { 1 } else { 2 },
                2026 => if self.modes.contains(TermModes::SYNC_OUTPUT) { 1 } else { 2 },
                _ => 0,
            };
            self.events.push(TerminalEvent::WriteBack(
                format!("\x1b[?{mode};{pm}$y").into_bytes(),
            ));
            return;
        }
        // DECSTR (CSI ! p) — Soft Terminal Reset
        if intermediates == [b'!'] && final_byte == b'p' {
            self.pen.reset();
            self.charsets = [Charset::Ascii; 2];
            self.active_charset = 0;
            self.scroll_top = 0;
            self.scroll_bottom = self.rows().saturating_sub(1);
            self.modes = TermModes::AUTO_WRAP | TermModes::CURSOR_VISIBLE;
            self.cursor.pending_wrap = false;
            return;
        }

        let private = intermediates.first().copied();
        let cols = self.cols();
        let rows = self.rows();
        let bg = self.pen.bg;
        let host = self.current_host_id;
        let cwd = self.current_cwd_id;

        match (private, final_byte) {
            (None, b'm') => self.apply_sgr(params),

            // Cursor movement
            (None, b'A') => {
                let n = params.get_or(0, 1) as usize;
                let min_r = if self.cursor.row >= self.scroll_top {
                    self.scroll_top
                } else {
                    0
                };
                self.cursor.row = self.cursor.row.saturating_sub(n).max(min_r);
                self.cursor.pending_wrap = false;
            }
            (None, b'B') | (None, b'e') => {
                let n = params.get_or(0, 1) as usize;
                let max_r = if self.cursor.row <= self.scroll_bottom {
                    self.scroll_bottom
                } else {
                    rows - 1
                };
                self.cursor.row = (self.cursor.row + n).min(max_r);
                self.cursor.pending_wrap = false;
            }
            (None, b'C') | (None, b'a') => {
                let n = params.get_or(0, 1) as usize;
                self.cursor.col = (self.cursor.col + n).min(cols - 1);
                self.cursor.pending_wrap = false;
            }
            (None, b'D') => {
                let n = params.get_or(0, 1) as usize;
                self.cursor.col = self.cursor.col.saturating_sub(n);
                self.cursor.pending_wrap = false;
            }
            (None, b'E') => {
                let n = params.get_or(0, 1) as usize;
                self.cursor.row = (self.cursor.row + n).min(self.scroll_bottom);
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            (None, b'F') => {
                let n = params.get_or(0, 1) as usize;
                self.cursor.row = self.cursor.row.saturating_sub(n).max(self.scroll_top);
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            (None, b'G') | (None, b'`') => {
                let c = (params.get_or(0, 1) as usize).saturating_sub(1);
                self.cursor.col = c.min(cols - 1);
                self.cursor.pending_wrap = false;
            }
            (None, b'd') => {
                let r = (params.get_or(0, 1) as usize).saturating_sub(1);
                let base = if self.modes.contains(TermModes::ORIGIN) {
                    self.scroll_top
                } else {
                    0
                };
                self.cursor.row = (base + r).min(rows - 1);
                self.cursor.pending_wrap = false;
            }
            (None, b'H') | (None, b'f') => {
                let r = (params.get_or(0, 1) as usize).saturating_sub(1);
                let c = (params.get_or(1, 1) as usize).saturating_sub(1);
                let (base_r, max_r) = if self.modes.contains(TermModes::ORIGIN) {
                    (self.scroll_top, self.scroll_bottom)
                } else {
                    (0, rows - 1)
                };
                self.cursor.row = (base_r + r).min(max_r);
                self.cursor.col = c.min(cols - 1);
                self.cursor.pending_wrap = false;
            }

            // Erase in display (ED)
            (None, b'J') | (Some(b'?'), b'J') => {
                let mode = params.get(0).unwrap_or(0);
                let r = self.cursor.row;
                let c = self.cursor.col;
                match mode {
                    0 => {
                        let grid = self.active_grid_mut();
                        grid.clear_cols(r, c, cols, bg);
                        for row in (r + 1)..rows {
                            grid.clear_row(row, bg, host, cwd);
                        }
                    }
                    1 => {
                        let grid = self.active_grid_mut();
                        for row in 0..r {
                            grid.clear_row(row, bg, host, cwd);
                        }
                        grid.clear_cols(r, 0, c + 1, bg);
                    }
                    2 => {
                        let grid = self.active_grid_mut();
                        for row in 0..rows {
                            grid.clear_row(row, bg, host, cwd);
                        }
                    }
                    3 => {
                        self.scrollback.clear();
                        self.viewport_offset = 0;
                    }
                    _ => {}
                }
            }

            // Erase in line (EL)
            (None, b'K') | (Some(b'?'), b'K') => {
                let mode = params.get(0).unwrap_or(0);
                let r = self.cursor.row;
                let c = self.cursor.col;
                let grid = self.active_grid_mut();
                match mode {
                    0 => grid.clear_cols(r, c, cols, bg),
                    1 => grid.clear_cols(r, 0, c + 1, bg),
                    2 => grid.clear_cols(r, 0, cols, bg),
                    _ => {}
                }
            }

            // Erase characters (ECH)
            (None, b'X') => {
                let n = params.get_or(0, 1) as usize;
                let r = self.cursor.row;
                let c = self.cursor.col;
                self.active_grid_mut().clear_cols(r, c, c + n, bg);
            }

            // Insert / Delete characters (ICH / DCH)
            (None, b'@') => {
                let r = self.cursor.row.min(rows - 1);
                let c = self.cursor.col.min(cols - 1);
                let n = (params.get_or(0, 1) as usize).min(cols.saturating_sub(c));
                if n > 0 {
                    let row_cells = self.active_grid_mut().row_mut(r);
                    row_cells.copy_within(c..cols - n, c + n);
                    row_cells[c..c + n].fill(Cell::blank_with_bg(bg));
                }
            }
            (None, b'P') => {
                let r = self.cursor.row.min(rows - 1);
                let c = self.cursor.col.min(cols - 1);
                let n = (params.get_or(0, 1) as usize).min(cols.saturating_sub(c));
                if n > 0 {
                    let row_cells = self.active_grid_mut().row_mut(r);
                    row_cells.copy_within(c + n..cols, c);
                    row_cells[cols - n..cols].fill(Cell::blank_with_bg(bg));
                }
            }

            // Insert / Delete lines (IL / DL)
            (None, b'L') => {
                if self.cursor.row >= self.scroll_top && self.cursor.row <= self.scroll_bottom {
                    let n = params.get_or(0, 1) as usize;
                    let r = self.cursor.row;
                    let bottom = self.scroll_bottom;
                    self.active_grid_mut()
                        .scroll_region_down(r, bottom, n, bg, host, cwd);
                    self.cursor.col = 0;
                    self.cursor.pending_wrap = false;
                }
            }
            (None, b'M') => {
                if self.cursor.row >= self.scroll_top && self.cursor.row <= self.scroll_bottom {
                    let n = params.get_or(0, 1) as usize;
                    let r = self.cursor.row;
                    let bottom = self.scroll_bottom;
                    self.active_grid_mut()
                        .scroll_region_up(r, bottom, n, bg, host, cwd, |_, _| {});
                    self.cursor.col = 0;
                    self.cursor.pending_wrap = false;
                }
            }

            // Scroll up / down (SU / SD)
            (None, b'S') => {
                let n = params.get_or(0, 1) as usize;
                self.scroll_up_internal(n);
            }
            (None, b'T') => {
                let n = params.get_or(0, 1) as usize;
                self.scroll_down_internal(n);
            }

            // Set scrolling region (DECSTBM)
            (None, b'r') => {
                let top = (params.get_or(0, 1) as usize).saturating_sub(1);
                let bottom = (params.get_or(1, rows as u16) as usize)
                    .saturating_sub(1)
                    .min(rows - 1);
                if top < bottom && bottom < rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = rows - 1;
                }
                self.cursor.col = 0;
                self.cursor.row = if self.modes.contains(TermModes::ORIGIN) {
                    self.scroll_top
                } else {
                    0
                };
                self.cursor.pending_wrap = false;
            }

            // Set / Reset modes
            (Some(b'?'), b'h') => {
                for &m in params.slice() {
                    self.set_private_mode(m, true);
                }
            }
            (Some(b'?'), b'l') => {
                for &m in params.slice() {
                    self.set_private_mode(m, false);
                }
            }
            (None, b'h') => {
                for &m in params.slice() {
                    match m {
                        4 => self.modes.insert(TermModes::INSERT_MODE),
                        20 => self.modes.insert(TermModes::LINEFEED_NEWLINE),
                        _ => {}
                    }
                }
            }
            (None, b'l') => {
                for &m in params.slice() {
                    match m {
                        4 => self.modes.remove(TermModes::INSERT_MODE),
                        20 => self.modes.remove(TermModes::LINEFEED_NEWLINE),
                        _ => {}
                    }
                }
            }

            // Cursor style (DECSCUSR: CSI Ps SP q)
            (Some(b' '), b'q') => {
                match params.get(0).unwrap_or(0) {
                    0 | 2 => {
                        self.cursor.style = CursorStyle::Block;
                        self.cursor.blinking = false;
                    }
                    1 => {
                        self.cursor.style = CursorStyle::Block;
                        self.cursor.blinking = true;
                    }
                    3 => {
                        self.cursor.style = CursorStyle::Underline;
                        self.cursor.blinking = true;
                    }
                    4 => {
                        self.cursor.style = CursorStyle::Underline;
                        self.cursor.blinking = false;
                    }
                    5 => {
                        self.cursor.style = CursorStyle::VerticalBar;
                        self.cursor.blinking = true;
                    }
                    6 => {
                        self.cursor.style = CursorStyle::VerticalBar;
                        self.cursor.blinking = false;
                    }
                    _ => {}
                }
            }

            // Save / Restore cursor (SCOSC / SCORC)
            (None, b's') => self.save_cursor(),
            (None, b'u') => self.restore_cursor(),

            // Kitty keyboard protocol push/pop/query
            (Some(b'>'), b'u') => {
                let flags = params.get(0).unwrap_or(0) as u8;
                if self.kitty_kbd_stack.len() < 16 {
                    self.kitty_kbd_stack.push(flags);
                }
            }
            (Some(b'<'), b'u') => {
                let n = params.get_or(0, 1) as usize;
                for _ in 0..n {
                    self.kitty_kbd_stack.pop();
                }
            }
            (Some(b'?'), b'u') => {
                let flags = self.kitty_keyboard_flags();
                self.events.push(TerminalEvent::WriteBack(
                    format!("\x1b[?{flags}u").into_bytes(),
                ));
            }

            // Device Status Report (DSR)
            (None, b'n') => match params.get(0).unwrap_or(0) {
                5 => self.events.push(TerminalEvent::WriteBack(b"\x1b[0n".to_vec())),
                6 => {
                    let resp = format!("\x1b[{};{}R", self.cursor.row + 1, self.cursor.col + 1);
                    self.events
                        .push(TerminalEvent::WriteBack(resp.into_bytes()));
                }
                _ => {}
            },

            // Primary / Secondary Device Attributes (DA1 / DA2)
            (None, b'c') => {
                // VT220 with ANSI color + clipboard + SGR
                self.events
                    .push(TerminalEvent::WriteBack(b"\x1b[?62;4;22c".to_vec()));
            }
            (Some(b'>'), b'c') => {
                // Report xterm-compatible version
                self.events
                    .push(TerminalEvent::WriteBack(b"\x1b[>0;3500;0c".to_vec()));
            }

            // Tab clear (TBC)
            (None, b'g') => match params.get(0).unwrap_or(0) {
                0 => {
                    if let Some(slot) = self.tab_stops.get_mut(self.cursor.col) {
                        *slot = false;
                    }
                }
                3 => self.tab_stops.fill(false),
                _ => {}
            },

            // Repeat preceding graphic character (REP)
            (None, b'b') => {
                let n = (params.get_or(0, 1) as usize).min(cols * rows);
                let r = self.cursor.row.min(rows - 1);
                if self.cursor.col > 0 && self.cursor.col <= cols {
                    let ch = self.active_grid().row(r)[self.cursor.col - 1].ch;
                    if ch.is_ascii() && (0x20..=0x7E).contains(&(ch as u32)) {
                        let buf = vec![ch as u8; n];
                        self.print_ascii_run(&buf);
                    } else {
                        for _ in 0..n {
                            self.print_char(ch);
                        }
                    }
                }
            }

            // Window manipulation queries & title stack (XTWINOPS: CSI 14/18/22/23 t)
            (None, b't') => match params.get(0).unwrap_or(0) {
                18 => {
                    let resp = format!("\x1b[8;{};{}t", rows, cols);
                    self.events.push(TerminalEvent::WriteBack(resp.into_bytes()));
                }
                14 => {
                    let w = (cols as f32 * self.cell_width_px) as u32;
                    let h = (rows as f32 * self.cell_height_px) as u32;
                    let resp = format!("\x1b[4;{};{}t", h, w);
                    self.events.push(TerminalEvent::WriteBack(resp.into_bytes()));
                }
                22 => {
                    // Push window/tab title onto stack (used by vim/neovim/ssh)
                    if self.title_stack.len() < 32 {
                        self.title_stack.push(self.title.clone());
                    }
                }
                23 => {
                    // Pop window/tab title from stack
                    if let Some(prev) = self.title_stack.pop() {
                        if self.title != prev {
                            self.title = prev.clone();
                            self.events.push(TerminalEvent::TitleChanged(prev));
                        }
                    }
                }
                _ => {}
            },

            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
        // VT100 G0/G1 charset designation (`ESC ( 0` = DEC Special Graphics used by htop/ncurses!)
        if intermediates.len() == 1 {
            match (intermediates[0], final_byte) {
                (b'(', b'0') => self.charsets[0] = Charset::DecSpecialGraphics,
                (b'(', b'B' | b'A' | b'U') => self.charsets[0] = Charset::Ascii,
                (b')', b'0') => self.charsets[1] = Charset::DecSpecialGraphics,
                (b')', b'B' | b'A' | b'U') => self.charsets[1] = Charset::Ascii,
                _ => {}
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match final_byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(),
            b'E' => {
                self.cursor.col = 0;
                self.linefeed();
            }
            b'H' => {
                if let Some(slot) = self.tab_stops.get_mut(self.cursor.col) {
                    *slot = true;
                }
            }
            b'M' => self.reverse_index(),
            b'=' => self.modes.insert(TermModes::APPLICATION_KEYPAD),
            b'>' => self.modes.remove(TermModes::APPLICATION_KEYPAD),
            b'c' => {
                // Full terminal reset (RIS)
                let cols = self.cols();
                let rows = self.rows();
                let sb_max = self.scrollback.len().max(100_000);
                *self = TerminalScreen::new(cols, rows, sb_max);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, payload: &[u8]) {
        let Ok(s) = std::str::from_utf8(payload) else {
            return;
        };
        let Some((code_str, rest)) = s.split_once(';') else {
            return;
        };
        match code_str {
            "0" | "1" | "2" => {
                if self.title != rest {
                    self.title.clear();
                    self.title.push_str(rest);
                    self.events.push(TerminalEvent::TitleChanged(self.title.clone()));
                }
            }
            "4" => {
                // OSC 4;<index>;? — Indexed 256-color palette query (used by vim/neovim/fzf)
                if let Some((idx_s, spec)) = rest.split_once(';') {
                    if spec.trim() == "?" {
                        if let Ok(idx) = idx_s.trim().parse::<u8>() {
                            let c = iterm_profile::ColorScheme::default().indexed_color(idx);
                            let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u8;
                            let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u8;
                            let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u8;
                            let resp = format!(
                                "\x1b]4;{idx};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
                            );
                            self.events.push(TerminalEvent::WriteBack(resp.into_bytes()));
                        }
                    }
                }
            }
            "6" => self.handle_osc_6(rest),
            "7" => {
                // OSC 7;file://host/path
                if let Some(after_scheme) = rest.strip_prefix("file://") {
                    if let Some((host, path)) = after_scheme.split_once('/') {
                        let decoded_path = percent_decode(&format!("/{path}"));
                        if !host.is_empty() {
                            self.current_host_id = self.strings.intern(host);
                        }
                        self.current_cwd_id = self.strings.intern(&decoded_path);
                        let r = self.cursor.row;
                        let host_id = self.current_host_id;
                        let cwd_id = self.current_cwd_id;
                        let meta = self.active_grid_mut().meta_mut(r);
                        meta.host_id = host_id;
                        meta.cwd_id = cwd_id;
                        self.events.push(TerminalEvent::CurrentDirChanged(decoded_path));
                    }
                }
            }
            "8" => {
                // OSC 8;params;URI (cap URI length & reject control characters for safety)
                if let Some((_, uri)) = rest.split_once(';') {
                    if uri.is_empty() {
                        self.pen.hyperlink_id = 0;
                    } else if uri.len() <= 4096 && !uri.bytes().any(|b| b < 0x20 || b == 0x7F) {
                        if let Some(&id) = self.hyperlink_lookup.get(uri) {
                            self.pen.hyperlink_id = id;
                        } else {
                            let id = self.hyperlinks.len() as u32;
                            self.hyperlinks.push(uri.to_string());
                            self.hyperlink_lookup.insert(uri.to_string(), id);
                            self.pen.hyperlink_id = id;
                        }
                    }
                    self.sync_pen_extra();
                }
            }
            "9" => {
                // OSC 9;<message> — iTerm2 desktop notification (skip ConEmu OSC 9;4;... progress codes)
                if !rest.starts_with("4;") && !rest.is_empty() {
                    self.events.push(TerminalEvent::Notification {
                        title: "Notification".into(),
                        body: rest.chars().take(200).collect(),
                    });
                }
            }
            "777" => {
                // OSC 777;notify;<title>;<body>
                if let Some(after) = rest.strip_prefix("notify;") {
                    let (t, b) = after.split_once(';').unwrap_or(("Notification", after));
                    self.events.push(TerminalEvent::Notification {
                        title: t.chars().take(80).collect(),
                        body: b.chars().take(200).collect(),
                    });
                }
            }
            "10" | "11" | "12" => {
                // OSC 10/11/12;? — Dynamic color query (neovim, bat, delta, fzf, bubbletea dark/light detection!)
                if rest.trim() == "?" {
                    let (r, g, b) = match code_str {
                        "10" => self.default_fg_rgb,
                        "11" => self.default_bg_rgb,
                        _ => self.default_cursor_rgb,
                    };
                    let resp = format!(
                        "\x1b]{code_str};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
                    );
                    self.events.push(TerminalEvent::WriteBack(resp.into_bytes()));
                }
            }
            "52" => {
                // OSC 52;<selection>;<base64 or ?>
                if let Some((_, b64)) = rest.split_once(';') {
                    if b64.trim() == "?" {
                        self.events.push(TerminalEvent::ClipboardRequest);
                    } else if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                        if let Ok(text) = String::from_utf8(raw) {
                            self.events.push(TerminalEvent::ClipboardSet(text));
                        }
                    }
                }
            }
            "133" => {
                // FinalTerm / iTerm2 Shell Integration prompt marks
                let r = self.cursor.row;
                let mark = if rest.starts_with('A') {
                    Some(PromptMark::PromptStart)
                } else if rest.starts_with('B') {
                    Some(PromptMark::CommandStart)
                } else if rest.starts_with('C') {
                    Some(PromptMark::CommandExecuted)
                } else if let Some(after_d) = rest.strip_prefix("D;") {
                    let exit = after_d.split(';').next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    Some(PromptMark::CommandFinished(exit))
                } else if rest == "D" {
                    Some(PromptMark::CommandFinished(0))
                } else {
                    None
                };
                if let Some(m) = mark {
                    self.active_grid_mut().meta_mut(r).set_prompt_mark(Some(m));
                }
            }
            "1337" => self.handle_osc_1337(rest),
            _ => {}
        }
    }

    fn dcs_dispatch(&mut self, _params: &CsiParams, intermediates: &[u8], final_byte: u8, data: &[u8]) {
        // XTGETTCAP (`DCS + q <hex-cap-1>;<hex-cap-2> ST`): queried by neovim/tmux/helix
        // to auto-detect truecolor (`RGB`/`Tc`), curly underlines (`Smulx`), underline color (`Setulc`),
        // synchronized output (`sync`), OSC 52 (`Ms`), and terminal name (`TN`)!
        if intermediates == [b'+'] && final_byte == b'q' {
            if let Ok(req) = std::str::from_utf8(data) {
                for hex_name in req.split(';').take(32) {
                    let Some(name) = decode_hex_str(hex_name.trim()) else {
                        continue;
                    };
                    let val: Option<&str> = match name.as_str() {
                        "TN" | "name" => Some("xterm-256color"),
                        "RGB" | "Tc" => Some(""),
                        "Co" | "colors" => Some("256"),
                        "Smulx" => Some("\x1b[4:%p1%dm"),
                        "Setulc" => Some("\x1b[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%dm"),
                        "Su" | "sync" => Some(""),
                        "Ms" => Some("\x1b]52;%p1%s;%p2%s\x07"),
                        _ => None,
                    };
                    let resp = if let Some(v) = val {
                        if v.is_empty() {
                            format!("\x1bP1+r{hex_name}\x1b\\")
                        } else {
                            let hex_v = encode_hex_str(v);
                            format!("\x1bP1+r{hex_name}={hex_v}\x1b\\")
                        }
                    } else {
                        format!("\x1bP0+r{hex_name}\x1b\\")
                    };
                    self.events.push(TerminalEvent::WriteBack(resp.into_bytes()));
                }
            }
        }
    }
}

fn decode_hex_str(hex: &str) -> Option<String> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) || hex.len() > 256 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let s = std::str::from_utf8(&bytes[i..i + 2]).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    String::from_utf8(out).ok()
}

fn encode_hex_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02X}");
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fast inline Unicode display width: O(1) branch checks for 99.9% of common BMP ranges
/// (ASCII, Greek, arrows, math, box-drawing, Hiragana/Katakana/CJK/Hangul), falling back to
/// `unicode_width` for combining marks & exotic planes.
#[inline(always)]
fn fast_char_width(ch: char) -> usize {
    let cp = ch as u32;
    if cp < 0x7F {
        return if cp >= 0x20 { 1 } else { 0 };
    }
    // Common single-width ranges: Latin-1..Greek..Cyrillic, punctuation/arrows/math/box-drawing
    if (0x00A0..=0x02FF).contains(&cp)
        || (0x0370..=0x0482).contains(&cp)
        || (0x2010..=0x2027).contains(&cp)
        || (0x2190..=0x26FF).contains(&cp)
    {
        // Note: a few emoji inside 0x2600..0x26FF have width 2; let unicode_width check 0x2600+
        if cp < 0x2600 {
            return 1;
        }
    }
    // Common East Asian double-width ranges: Hiragana, Katakana, CJK Unified Ideographs, Hangul
    if (0x3041..=0x3096).contains(&cp)
        || (0x30A0..=0x30FF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xFF01..=0xFF60).contains(&cp)
    {
        return 2;
    }
    ch.width().unwrap_or(0)
}

/// Detect zero-width combining marks, variation selectors (U+FE0E/FE0F), ZWJ (U+200D),
/// and emoji Fitzpatrick skin-tone modifiers (U+1F3FB..=U+1F3FF) that attach to the preceding cell.
#[inline(always)]
fn is_combining_or_joiner(ch: char) -> bool {
    let cp = ch as u32;
    if cp < 0x0300 {
        return false;
    }
    // Common CJK/Hiragana/Katakana/Hangul/Box-Drawing fast skip (1 branch!)
    if (0x2100..=0xFDFF).contains(&cp) {
        return false;
    }
    cp == 0x200D
        || cp == 0xFE0E
        || cp == 0xFE0F
        || (0x0300..=0x036F).contains(&cp)
        || (0x1AB0..=0x1AFF).contains(&cp)
        || (0x1DC0..=0x1DFF).contains(&cp)
        || (0x20D0..=0x20FF).contains(&cp)
        || (0xFE20..=0xFE2F).contains(&cp)
        || (0x1F3FB..=0x1F3FF).contains(&cp)
}
