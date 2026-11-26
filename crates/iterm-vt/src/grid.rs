use crate::cell::{Cell, CellFlags, PackedColor};
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide monotonic ID assigned once per `ActiveGrid` allocation.
/// Packed into bits 38..61 of `RowMeta.generation` so `primary`, `alternate`, `resize_simple`,
/// `reflow_primary`, and distinct panes/tabs NEVER collide in `PaneRenderCache` (zero hot-path atomics!).
static NEXT_GRID_ID: AtomicU64 = AtomicU64::new(1);

#[inline(always)]
fn pack_row_gen(grid_id: u64, local_gen: u64) -> u64 {
    ((grid_id & 0x00FF_FFFF) << 38) | (local_gen & 0x003F_FFFF_FFFF)
}

/// Shell integration prompt mark attached to a terminal row (`OSC 133;A/B/C/D`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    PromptStart,
    CommandStart,
    CommandExecuted,
    CommandFinished(i32),
}

const META_WRAPPED: u8 = 1 << 0;
const META_NON_ASCII: u8 = 1 << 1;
const META_WIDE_OR_IMAGE: u8 = 1 << 2;
const META_STYLED: u8 = 1 << 3;

/// Pure `Copy` 24-byte POD per-row metadata (ZERO heap allocations, ZERO Drop!).
#[derive(Debug, Clone, Copy)]
pub struct RowMeta {
    pub generation: u64,
    pub host_id: u32,
    pub cwd_id: u32,
    pub used_cols: u16,
    flags: u8,
    prompt_kind: u8,
    prompt_exit: i16,
}

impl Default for RowMeta {
    fn default() -> Self {
        Self {
            generation: 1,
            host_id: 0,
            cwd_id: 0,
            used_cols: 0,
            flags: 0,
            prompt_kind: 0,
            prompt_exit: 0,
        }
    }
}

impl RowMeta {
    #[inline]
    pub fn wrapped(&self) -> bool {
        (self.flags & META_WRAPPED) != 0
    }

    #[inline]
    pub fn set_wrapped(&mut self, v: bool) {
        if v {
            self.flags |= META_WRAPPED;
        } else {
            self.flags &= !META_WRAPPED;
        }
    }

    #[inline]
    pub fn has_non_ascii(&self) -> bool {
        (self.flags & META_NON_ASCII) != 0
    }

    #[inline]
    pub fn set_has_non_ascii(&mut self, v: bool) {
        if v {
            self.flags |= META_NON_ASCII;
        } else {
            self.flags &= !META_NON_ASCII;
        }
    }

    #[inline]
    pub fn set_has_wide_or_image(&mut self, v: bool) {
        if v {
            self.flags |= META_WIDE_OR_IMAGE;
        } else {
            self.flags &= !META_WIDE_OR_IMAGE;
        }
    }

    #[inline]
    pub fn is_plain_ascii(&self) -> bool {
        (self.flags & (META_NON_ASCII | META_WIDE_OR_IMAGE | META_STYLED)) == 0
    }

    #[inline]
    pub fn set_styled(&mut self, v: bool) {
        if v {
            self.flags |= META_STYLED;
        } else {
            self.flags &= !META_STYLED;
        }
    }

    #[inline]
    pub fn prompt_mark(&self) -> Option<PromptMark> {
        match self.prompt_kind {
            1 => Some(PromptMark::PromptStart),
            2 => Some(PromptMark::CommandStart),
            3 => Some(PromptMark::CommandExecuted),
            4 => Some(PromptMark::CommandFinished(self.prompt_exit as i32)),
            _ => None,
        }
    }

    #[inline]
    pub fn set_prompt_mark(&mut self, m: Option<PromptMark>) {
        match m {
            None => {
                self.prompt_kind = 0;
                self.prompt_exit = 0;
            }
            Some(PromptMark::PromptStart) => self.prompt_kind = 1,
            Some(PromptMark::CommandStart) => self.prompt_kind = 2,
            Some(PromptMark::CommandExecuted) => self.prompt_kind = 3,
            Some(PromptMark::CommandFinished(code)) => {
                self.prompt_kind = 4;
                self.prompt_exit = code.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
    }
}

/// String interner for per-row `RemoteHost` and `CurrentDir` metadata.
#[derive(Debug, Clone, Default)]
pub struct StringPool {
    strings: Vec<String>,
    index: FxHashMap<String, u32>,
}

impl StringPool {
    pub fn new() -> Self {
        Self {
            strings: vec![String::new()], // ID 0 = empty/unset
            index: FxHashMap::default(),
        }
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), id);
        id
    }

    #[inline]
    pub fn get(&self, id: u32) -> Option<&str> {
        if id == 0 {
            return None;
        }
        self.strings.get(id as usize).map(|s| s.as_str())
    }
}

/// Flat contiguous active grid with **O(1) `row_order` indirection**!
///
/// Scrolling ANY region `[top_row..=bottom_row]` — including tmux's `DECSTBM 1..rows-1`
/// (which leaves the bottom row for tmux's status bar!) — rotates `row_order[top..=bottom]`
/// in L1 cache (~80 bytes!) with ZERO cell `memmove` and ZERO `RowMeta` copies!
#[derive(Debug, Clone)]
pub struct ActiveGrid {
    grid_id: u64,
    cols: usize,
    rows: usize,
    row_order: Vec<u16>,
    cells: Vec<Cell>,
    meta: Vec<RowMeta>,
    next_gen: u64,
}

impl ActiveGrid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let grid_id = NEXT_GRID_ID.fetch_add(1, Ordering::Relaxed);
        let cols = cols.max(1);
        let rows = rows.clamp(1, u16::MAX as usize);
        let mut meta = Vec::with_capacity(rows);
        let mut row_order = Vec::with_capacity(rows);
        for i in 0..rows {
            row_order.push(i as u16);
            meta.push(RowMeta {
                generation: pack_row_gen(grid_id, (i as u64) + 1),
                ..RowMeta::default()
            });
        }
        Self {
            grid_id,
            cols,
            rows,
            row_order,
            cells: vec![Cell::BLANK; cols * rows],
            meta,
            next_gen: (rows as u64) + 2,
        }
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline]
    pub fn current_generation(&self) -> u64 {
        pack_row_gen(self.grid_id, self.next_gen)
    }

    #[inline(always)]
    fn phys_row(&self, logical_row: usize) -> usize {
        let lr = if logical_row < self.rows {
            logical_row
        } else {
            self.rows - 1
        };
        unsafe { *self.row_order.get_unchecked(lr) as usize }
    }

    #[inline(always)]
    pub fn bump_generation(&mut self) -> u64 {
        let g = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1).max(1);
        pack_row_gen(self.grid_id, g)
    }

    #[inline(always)]
    pub fn cells_mut(&mut self) -> &mut [Cell] {
        &mut self.cells
    }

    #[inline(always)]
    pub fn row(&self, logical_row: usize) -> &[Cell] {
        let p = self.phys_row(logical_row);
        let start = p * self.cols;
        unsafe { self.cells.get_unchecked(start..start + self.cols) }
    }

    #[inline(always)]
    pub fn row_mut(&mut self, logical_row: usize) -> &mut [Cell] {
        let p = self.phys_row(logical_row);
        let g = self.bump_generation();
        self.meta[p].generation = g;
        self.meta[p].used_cols = self.cols as u16;
        self.meta[p].set_styled(true);
        let start = p * self.cols;
        &mut self.cells[start..start + self.cols]
    }

    #[inline(always)]
    pub fn row_and_meta_mut(&mut self, logical_row: usize) -> (&mut [Cell], &mut RowMeta) {
        let p = self.phys_row(logical_row);
        let g = self.bump_generation();
        let meta = &mut self.meta[p];
        meta.generation = g;
        let start = p * self.cols;
        (&mut self.cells[start..start + self.cols], meta)
    }

    #[inline(always)]
    pub fn meta(&self, logical_row: usize) -> &RowMeta {
        let p = self.phys_row(logical_row);
        unsafe { self.meta.get_unchecked(p) }
    }

    #[inline(always)]
    pub fn meta_mut(&mut self, logical_row: usize) -> &mut RowMeta {
        let p = self.phys_row(logical_row);
        &mut self.meta[p]
    }

    #[inline]
    pub fn mark_row_dirty(&mut self, logical_row: usize) {
        let p = self.phys_row(logical_row);
        let g = self.bump_generation();
        self.meta[p].generation = g;
    }

    pub fn mark_all_dirty(&mut self) {
        for r in 0..self.rows {
            self.mark_row_dirty(r);
        }
    }

    #[inline]
    fn clear_phys_row(&mut self, p: usize, bg: PackedColor, host_id: u32, cwd_id: u32) {
        let g = self.bump_generation();
        let start = p * self.cols;
        let blank = Cell::blank_with_bg(bg);
        let clear_len = if bg.is_default() {
            (self.meta[p].used_cols as usize).min(self.cols)
        } else {
            self.cols
        };
        if clear_len > 0 {
            self.cells[start..start + clear_len].fill(blank);
        }
        let m = &mut self.meta[p];
        m.generation = g;
        m.flags = if bg.is_default() { 0 } else { META_STYLED };
        m.used_cols = if bg.is_default() { 0 } else { self.cols as u16 };
        m.host_id = host_id;
        m.cwd_id = cwd_id;
        m.prompt_kind = 0;
        m.prompt_exit = 0;
    }

    /// Clear a single row to blank cells with `bg`.
    #[inline]
    pub fn clear_row(&mut self, logical_row: usize, bg: PackedColor, host_id: u32, cwd_id: u32) {
        let p = self.phys_row(logical_row);
        self.clear_phys_row(p, bg, host_id, cwd_id);
    }

    /// Clear range `[start_col..end_col)` on `logical_row`.
    pub fn clear_cols(&mut self, logical_row: usize, start_col: usize, end_col: usize, bg: PackedColor) {
        let end_col = end_col.min(self.cols);
        if start_col >= end_col {
            return;
        }
        let p = self.phys_row(logical_row);
        let g = self.bump_generation();
        let base = p * self.cols;
        let blank = Cell::blank_with_bg(bg);
        if start_col > 0 && self.cells[base + start_col].flags.contains(CellFlags::WIDE_SPACER) {
            self.cells[base + start_col - 1] = blank;
        }
        if end_col < self.cols && self.cells[base + end_col - 1].flags.contains(CellFlags::WIDE_CHAR) {
            self.cells[base + end_col] = blank;
        }
        let used = self.meta[p].used_cols as usize;
        let actual_end = if bg.is_default() {
            end_col.min(used.max(start_col))
        } else {
            end_col
        };
        if actual_end > start_col {
            self.cells[base + start_col..base + actual_end].fill(blank);
        }
        let m = &mut self.meta[p];
        m.generation = g;
        if !bg.is_default() {
            m.used_cols = m.used_cols.max(end_col as u16);
            m.set_styled(true);
        } else if end_col >= used {
            m.used_cols = (start_col as u16).min(m.used_cols);
        }
        if end_col == self.cols {
            m.set_wrapped(false);
        }
    }

    /// O(1) region scroll up (BOTH full-screen AND tmux `DECSTBM` sub-regions!) via `row_order.rotate_left`!
    #[inline]
    pub fn scroll_region_up<F>(
        &mut self,
        top_row: usize,
        bottom_row: usize,
        count: usize,
        bg: PackedColor,
        host_id: u32,
        cwd_id: u32,
        mut on_evict: F,
    ) where
        F: FnMut(&[Cell], &RowMeta),
    {
        if count == 0 || top_row > bottom_row || bottom_row >= self.rows {
            return;
        }
        let region_len = bottom_row - top_row + 1;
        let count = count.min(region_len);

        if top_row == 0 && bottom_row == self.rows - 1 {
            for r in 0..count {
                let p = self.row_order[r] as usize;
                let start = p * self.cols;
                on_evict(&self.cells[start..start + self.cols], &self.meta[p]);
            }
        }

        if count < region_len {
            self.row_order[top_row..=bottom_row].rotate_left(count);
        }

        let clear_start = bottom_row + 1 - count;
        for r in clear_start..=bottom_row {
            let p = self.row_order[r] as usize;
            self.clear_phys_row(p, bg, host_id, cwd_id);
        }
    }

    /// O(1) region scroll down (BOTH full-screen AND tmux `DECSTBM` sub-regions!) via `row_order.rotate_right`!
    #[inline]
    pub fn scroll_region_down(
        &mut self,
        top_row: usize,
        bottom_row: usize,
        count: usize,
        bg: PackedColor,
        host_id: u32,
        cwd_id: u32,
    ) {
        if count == 0 || top_row > bottom_row || bottom_row >= self.rows {
            return;
        }
        let region_len = bottom_row - top_row + 1;
        let count = count.min(region_len);

        if count < region_len {
            self.row_order[top_row..=bottom_row].rotate_right(count);
        }

        for r in top_row..(top_row + count) {
            let p = self.row_order[r] as usize;
            self.clear_phys_row(p, bg, host_id, cwd_id);
        }
    }

    /// Resize grid dimensions cleanly.
    pub fn resize_simple(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.clamp(1, u16::MAX as usize);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }
        let mut next = ActiveGrid::new(new_cols, new_rows);
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for r in 0..copy_rows {
            let src_row = self.row(r);
            let src_meta = *self.meta(r);
            let (dst_row, dst_meta) = next.row_and_meta_mut(r);
            dst_row[..copy_cols].copy_from_slice(&src_row[..copy_cols]);
            let g = dst_meta.generation;
            *dst_meta = src_meta;
            dst_meta.generation = g;
            dst_meta.used_cols = dst_meta.used_cols.min(copy_cols as u16);
            if copy_cols < self.cols {
                dst_meta.set_wrapped(false);
            }
        }
        *self = next;
    }

    /// Extract plain UTF-8 text for a logical row (trimming trailing spaces unless wrapped).
    pub fn row_text(&self, logical_row: usize) -> String {
        row_cells_to_string(self.row(logical_row), self.meta(logical_row).wrapped())
    }
}

/// Convert a row's cells into a UTF-8 string (skipping wide-char spacers and trimming trailing spaces).
pub fn row_cells_to_string(cells: &[Cell], keep_trailing: bool) -> String {
    let mut end = cells.len();
    if !keep_trailing {
        while end > 0 && cells[end - 1].ch == ' ' && !cells[end - 1].flags.contains(CellFlags::WIDE_SPACER) {
            end -= 1;
        }
    }
    let mut out = String::with_capacity(end);
    for cell in &cells[..end] {
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        out.push(cell.ch);
    }
    out
}
