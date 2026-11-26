use crate::cell::{Cell, CellFlags};
use crate::grid::{row_cells_to_string, PromptMark, RowMeta};
use std::collections::VecDeque;

/// Fixed 64 KiB packed-ASCII slab (`1 byte/char` — 16x smaller than 16-byte `Cell`s!).
const SLAB_ASCII_BYTES: usize = 64 * 1024;
/// Fixed 256 KiB styled-cell slab (`16_384 * 16B`), lazily allocated ONLY when styled/Unicode lines arrive!
const SLAB_CELLS: usize = 16_384;
/// Maximum archived lines per slab.
const SLAB_MAX_LINES: usize = 512;

const LINE_FLAG_WRAPPED: u8 = 1 << 0;
const LINE_FLAG_PACKED_ASCII: u8 = 1 << 2;

/// Pure `Copy` 16-byte archived line descriptor (ZERO heap pointers!).
#[derive(Debug, Clone, Copy)]
pub struct ScrollbackLineMeta {
    pub data_offset: u32,
    pub data_len: u16,
    pub flags: u8,
    pub prompt_kind: u8,
    pub host_id: u32,
    pub cwd_id: u32,
}

impl ScrollbackLineMeta {
    #[inline]
    pub fn wrapped(&self) -> bool {
        (self.flags & LINE_FLAG_WRAPPED) != 0
    }

    #[inline]
    pub fn is_packed_ascii(&self) -> bool {
        (self.flags & LINE_FLAG_PACKED_ASCII) != 0
    }

    #[inline]
    pub fn prompt_mark(&self) -> Option<PromptMark> {
        match self.prompt_kind {
            1 => Some(PromptMark::PromptStart),
            2 => Some(PromptMark::CommandStart),
            3 => Some(PromptMark::CommandExecuted),
            4 => Some(PromptMark::CommandFinished(0)),
            _ => None,
        }
    }
}

/// Dual-tier arena slab: packs plain unstyled ASCII rows at 1 byte/char (16x RAM & bandwidth savings!)
/// and stores styled/Unicode rows losslessly in a lazily-allocated 16-byte `Cell` buffer!
#[derive(Debug, Clone)]
struct ScrollbackSlab {
    ascii: Vec<u8>,
    cells: Vec<Cell>,
    lines: Vec<ScrollbackLineMeta>,
    first_line_global: u64,
}

impl ScrollbackSlab {
    fn new(first_line_global: u64) -> Self {
        Self {
            ascii: Vec::with_capacity(SLAB_ASCII_BYTES),
            cells: Vec::new(),
            lines: Vec::with_capacity(SLAB_MAX_LINES),
            first_line_global,
        }
    }

    #[inline]
    fn reset_for_reuse(&mut self, first_line_global: u64) {
        self.ascii.clear();
        self.cells.clear();
        self.lines.clear();
        self.first_line_global = first_line_global;
    }

    #[inline]
    fn can_fit_ascii(&self, n_bytes: usize) -> bool {
        self.lines.len() < SLAB_MAX_LINES && self.ascii.len() + n_bytes <= self.ascii.capacity()
    }

    #[inline]
    fn can_fit_cells(&self, n_cells: usize) -> bool {
        if self.lines.len() >= SLAB_MAX_LINES {
            return false;
        }
        if self.cells.capacity() == 0 {
            return true;
        }
        self.cells.len() + n_cells <= self.cells.capacity()
    }

    #[inline]
    fn push_raw_ascii(&mut self, bytes: &[u8], wrapped: bool, host_id: u32, cwd_id: u32) {
        let offset = self.ascii.len() as u32;
        self.ascii.extend_from_slice(bytes);
        let flags = u8::from(wrapped) | LINE_FLAG_PACKED_ASCII;
        self.lines.push(ScrollbackLineMeta {
            data_offset: offset,
            data_len: bytes.len() as u16,
            flags,
            prompt_kind: 0,
            host_id,
            cwd_id,
        });
    }

    #[inline]
    fn push_packed_ascii(&mut self, row: &[Cell], meta: &RowMeta) {
        let offset = self.ascii.len() as u32;
        let len = row.len();
        // Fast unpack char -> u8 into pre-reserved 64 KiB buffer (zero reallocation!)
        let start = self.ascii.len();
        unsafe {
            let dst = self.ascii.as_mut_ptr().add(start);
            for (k, cell) in row.iter().enumerate() {
                *dst.add(k) = cell.ch as u8;
            }
            self.ascii.set_len(start + len);
        }
        let prompt_kind = encode_prompt_kind(meta.prompt_mark());
        let flags = u8::from(meta.wrapped()) | LINE_FLAG_PACKED_ASCII;
        self.lines.push(ScrollbackLineMeta {
            data_offset: offset,
            data_len: len as u16,
            flags,
            prompt_kind,
            host_id: meta.host_id,
            cwd_id: meta.cwd_id,
        });
    }

    #[inline]
    fn push_cells(&mut self, cells_slice: &[Cell], meta: &RowMeta) {
        if self.cells.capacity() == 0 {
            self.cells.reserve(SLAB_CELLS.max(cells_slice.len()));
        }
        let offset = self.cells.len() as u32;
        self.cells.extend_from_slice(cells_slice);
        let prompt_kind = encode_prompt_kind(meta.prompt_mark());
        let flags = u8::from(meta.wrapped()) | (u8::from(meta.has_non_ascii()) << 1);
        self.lines.push(ScrollbackLineMeta {
            data_offset: offset,
            data_len: cells_slice.len() as u16,
            flags,
            prompt_kind,
            host_id: meta.host_id,
            cwd_id: meta.cwd_id,
        });
    }
}

#[inline]
fn encode_prompt_kind(m: Option<PromptMark>) -> u8 {
    match m {
        None => 0,
        Some(PromptMark::PromptStart) => 1,
        Some(PromptMark::CommandStart) => 2,
        Some(PromptMark::CommandExecuted) => 3,
        Some(PromptMark::CommandFinished(_)) => 4,
    }
}

/// High-throughput dual-tier recycled-slab scrollback buffer.
///
/// - Plain ASCII lines store at **1 byte/char** (16x smaller RAM & memory traffic!).
/// - Styled/truecolor/Unicode lines store losslessly in recycled 256 KiB `Cell` slabs.
#[derive(Debug, Clone)]
pub struct Scrollback {
    slabs: VecDeque<ScrollbackSlab>,
    free_slabs: Vec<ScrollbackSlab>,
    max_lines: usize,
    total_lines: usize,
    evicted_lines: u64,
    first_slab_start: usize,
    epoch: u64,
}

impl Scrollback {
    pub fn new(max_lines: usize) -> Self {
        Self {
            slabs: VecDeque::new(),
            free_slabs: Vec::with_capacity(8),
            max_lines,
            total_lines: 0,
            evicted_lines: 0,
            first_slab_start: 0,
            epoch: 1,
        }
    }

    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.total_lines
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total_lines == 0
    }

    #[inline]
    pub fn evicted_lines(&self) -> u64 {
        self.evicted_lines
    }

    pub fn set_max_lines(&mut self, max_lines: usize) {
        self.max_lines = max_lines;
        self.enforce_limit();
    }

    pub fn clear(&mut self) {
        self.evicted_lines = self.evicted_lines.saturating_add(self.total_lines as u64);
        self.epoch = self.epoch.wrapping_add(1).max(1);
        while let Some(slab) = self.slabs.pop_front() {
            if self.free_slabs.len() < 8 {
                self.free_slabs.push(slab);
            }
        }
        self.total_lines = 0;
        self.first_slab_start = 0;
    }

    /// Pop the most recently archived line from the back of scrollback (used for lossless resize reflow pull-back!).
    pub fn pop_back_line(&mut self) -> Option<(Vec<Cell>, ScrollbackLineMeta)> {
        if self.total_lines == 0 {
            return None;
        }
        let (cells, meta, remove_back) = {
            let min_lines = if self.slabs.len() == 1 {
                self.first_slab_start
            } else {
                0
            };
            let slab = self.slabs.back_mut()?;
            if slab.lines.len() <= min_lines {
                return None;
            }
            let meta = slab.lines.pop()?;
            let off = meta.data_offset as usize;
            let len = meta.data_len as usize;
            let cells = if meta.is_packed_ascii() {
                let slice = &slab.ascii[off..off + len];
                let mut v = Vec::with_capacity(len);
                for &b in slice {
                    v.push(Cell {
                        ch: b as char,
                        ..Cell::BLANK
                    });
                }
                slab.ascii.truncate(off);
                v
            } else {
                let v = slab.cells[off..off + len].to_vec();
                slab.cells.truncate(off);
                v
            };
            let remove_back = slab.lines.len() <= min_lines;
            (cells, meta, remove_back)
        };

        self.total_lines -= 1;
        // Bump epoch so any subsequently pushed scrollback line at this global index gets a fresh cache key!
        self.epoch = self.epoch.wrapping_add(1).max(1);
        if remove_back {
            if let Some(slab) = self.slabs.pop_back() {
                if self.free_slabs.len() < 8 {
                    self.free_slabs.push(slab);
                }
            }
            if self.slabs.is_empty() {
                self.first_slab_start = 0;
            }
        }
        Some((cells, meta))
    }

    /// Append a raw plain-ASCII line directly into a packed 1-byte/char slab (ZERO `Cell` expansion!).
    #[inline]
    pub fn push_raw_ascii_line(&mut self, mut bytes: &[u8], wrapped: bool, host_id: u32, cwd_id: u32) {
        if self.max_lines == 0 {
            self.evicted_lines += 1;
            return;
        }
        if !wrapped {
            while !bytes.is_empty() && bytes[bytes.len() - 1] == b' ' {
                bytes = &bytes[..bytes.len() - 1];
            }
        }
        let need_new = self
            .slabs
            .back()
            .is_none_or(|s| !s.can_fit_ascii(bytes.len()));
        if need_new {
            let next_global = self.evicted_lines + self.total_lines as u64;
            let mut slab = if let Some(mut recycled) = self.free_slabs.pop() {
                recycled.reset_for_reuse(next_global);
                recycled
            } else {
                ScrollbackSlab::new(next_global)
            };
            if slab.ascii.capacity() < bytes.len() {
                slab.ascii.reserve(bytes.len());
            }
            self.slabs.push_back(slab);
        }
        self.slabs
            .back_mut()
            .unwrap()
            .push_raw_ascii(bytes, wrapped, host_id, cwd_id);
        self.total_lines += 1;
        if self.total_lines > self.max_lines {
            self.enforce_limit();
        }
    }

    /// Append an evicted screen row in O(1) — packs plain ASCII at 1 byte/char!
    #[inline]
    pub fn push_row(&mut self, row: &[Cell], meta: &RowMeta) {
        if self.max_lines == 0 {
            self.evicted_lines += 1;
            return;
        }
        let mut trimmed_len = if meta.wrapped() {
            row.len()
        } else {
            (meta.used_cols as usize).min(row.len())
        };
        if !meta.wrapped() && trimmed_len > 0 && row[trimmed_len - 1].ch == ' ' {
            while trimmed_len > 0 && row[trimmed_len - 1].is_empty() {
                trimmed_len -= 1;
            }
        }

        let plain_ascii = meta.is_plain_ascii();
        let need_new = self.slabs.back().is_none_or(|s| {
            if plain_ascii {
                !s.can_fit_ascii(trimmed_len)
            } else {
                !s.can_fit_cells(trimmed_len)
            }
        });
        if need_new {
            let next_global = self.evicted_lines + self.total_lines as u64;
            let mut slab = if let Some(mut recycled) = self.free_slabs.pop() {
                recycled.reset_for_reuse(next_global);
                recycled
            } else {
                ScrollbackSlab::new(next_global)
            };
            if plain_ascii && slab.ascii.capacity() < trimmed_len {
                slab.ascii.reserve(trimmed_len);
            }
            self.slabs.push_back(slab);
        }

        let slab = self.slabs.back_mut().unwrap();
        if plain_ascii {
            slab.push_packed_ascii(&row[..trimmed_len], meta);
        } else {
            slab.push_cells(&row[..trimmed_len], meta);
        }
        self.total_lines += 1;
        if self.total_lines > self.max_lines {
            self.enforce_limit();
        }
    }

    fn enforce_limit(&mut self) {
        while self.total_lines > self.max_lines {
            let excess = self.total_lines - self.max_lines;
            let Some(front) = self.slabs.front() else {
                break;
            };
            let avail_in_front = front.lines.len() - self.first_slab_start;
            if excess >= avail_in_front {
                if let Some(slab) = self.slabs.pop_front() {
                    if self.free_slabs.len() < 8 {
                        self.free_slabs.push(slab);
                    }
                }
                self.total_lines -= avail_in_front;
                self.evicted_lines += avail_in_front as u64;
                self.first_slab_start = 0;
            } else {
                self.first_slab_start += excess;
                self.total_lines -= excess;
                self.evicted_lines += excess as u64;
            }
        }
    }

    #[inline]
    fn locate(&self, idx: usize) -> Option<(&ScrollbackSlab, usize)> {
        if idx >= self.total_lines || self.slabs.is_empty() {
            return None;
        }
        let target_global = self.evicted_lines + idx as u64;
        let mut lo = 0usize;
        let mut hi = self.slabs.len();
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.slabs[mid].first_line_global <= target_global {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let slab = &self.slabs[lo];
        let in_slab = if lo == 0 {
            let rel = (target_global - self.evicted_lines) as usize;
            self.first_slab_start + rel
        } else {
            (target_global - slab.first_line_global) as usize
        };
        if in_slab >= slab.lines.len() {
            return None;
        }
        Some((slab, in_slab))
    }

    /// Retrieve line metadata by 0-based scrollback index (`0` = oldest retained line).
    pub fn get(&self, idx: usize) -> Option<(Option<&[Cell]>, &ScrollbackLineMeta)> {
        let (slab, in_slab) = self.locate(idx)?;
        let m = &slab.lines[in_slab];
        if m.is_packed_ascii() {
            Some((None, m))
        } else {
            let s = m.data_offset as usize;
            Some((Some(&slab.cells[s..s + m.data_len as usize]), m))
        }
    }

    pub fn line_text(&self, idx: usize) -> Option<String> {
        let (slab, in_slab) = self.locate(idx)?;
        let m = &slab.lines[in_slab];
        let s = m.data_offset as usize;
        let len = m.data_len as usize;
        if m.is_packed_ascii() {
            let bytes = &slab.ascii[s..s + len];
            let mut end = bytes.len();
            if !m.wrapped() {
                while end > 0 && bytes[end - 1] == b' ' {
                    end -= 1;
                }
            }
            // All bytes in packed ASCII are 0x20..=0x7E
            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
        } else {
            Some(row_cells_to_string(&slab.cells[s..s + len], m.wrapped()))
        }
    }

    pub fn copy_line_padded(&self, idx: usize, dst: &mut [Cell]) -> Option<&ScrollbackLineMeta> {
        let (slab, in_slab) = self.locate(idx)?;
        let m = &slab.lines[in_slab];
        let s = m.data_offset as usize;
        let len = m.data_len as usize;
        let n = len.min(dst.len());
        if m.is_packed_ascii() {
            let bytes = &slab.ascii[s..s + n];
            for (cell, &b) in dst[..n].iter_mut().zip(bytes.iter()) {
                *cell = Cell {
                    ch: b as char,
                    ..Cell::BLANK
                };
            }
        } else {
            dst[..n].copy_from_slice(&slab.cells[s..s + n]);
        }
        if n < dst.len() {
            dst[n..].fill(Cell::BLANK);
        }
        if n == dst.len() && n > 0 && dst[n - 1].flags.contains(CellFlags::WIDE_CHAR) {
            dst[n - 1] = Cell::BLANK;
        }
        Some(m)
    }

    /// Visit every non-packed styled cell across all retained scrollback slabs (used by side-table GC).
    pub fn for_each_styled_cell_mut<F: FnMut(&mut Cell)>(&mut self, mut f: F) {
        for slab in &mut self.slabs {
            for cell in &mut slab.cells {
                if cell.extra_id != 0 {
                    f(cell);
                }
            }
        }
    }

    /// Approximate heap memory currently consumed by retained scrollback slabs in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.slabs
            .iter()
            .map(|s| {
                s.ascii.capacity()
                    + s.cells.capacity() * std::mem::size_of::<Cell>()
                    + s.lines.capacity() * std::mem::size_of::<ScrollbackLineMeta>()
            })
            .sum()
    }
}
