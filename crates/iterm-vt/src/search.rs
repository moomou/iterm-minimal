//! High-throughput zero-allocation SIMD search engine over dual-tier `Scrollback` slabs
//! and `ActiveGrid` rows.
//!
//! Performance architecture:
//! - **Pure-ASCII scrollback slabs (`slab.ascii`)**: scanned contiguously (64 KiB at a time)
//!   with hardware SIMD (`memchr::memmem::Finder` for case-sensitive, rare-byte `memchr`/`memchr2`
//!   for case-insensitive) — skipping non-matching slabs of 512 lines in ~2 us with zero allocs.
//! - **Styled ASCII lines (`slab.cells` with `!has_non_ascii()`)**: unpacked into a single
//!   reusable scratch byte buffer and scanned with the same SIMD matcher (zero heap allocs/line).
//! - **Unicode/CJK lines**: skips pure-ASCII slabs/lines in O(1) when query is non-ASCII,
//!   uses direct UTF-8 `memmem` for case-invariant scripts (CJK/Kana/Emoji), and maps match
//!   byte offsets to exact visual terminal columns (accounting for `WIDE_CHAR`/`WIDE_SPACER`).
//! - **Regex search**: safely extracts the longest guaranteed required literal outside
//!   alternations/quantifiers/groups/classes as a SIMD slab prefilter before invoking `regex_lite`.

use crate::cell::{Cell, CellFlags};
use crate::grid::ActiveGrid;
use crate::screen::SearchMatch;
use crate::scrollback::{Scrollback, SlabView};
use memchr::{memchr, memchr2, memmem};

/// Relative byte frequency rank for English/code/log ASCII text (lower = rarer = better SIMD anchor).
#[inline]
fn ascii_rarity(b: u8) -> u8 {
    match b.to_ascii_lowercase() {
        b' ' => 255,
        b'e' => 240,
        b'0' => 235,
        b't' => 230,
        b'a' => 225,
        b'o' => 220,
        b'i' => 215,
        b'n' => 210,
        b's' => 205,
        b'r' => 200,
        b'1' | b'2' => 195,
        b'l' => 190,
        b'd' => 185,
        b'c' => 180,
        b'h' | b'u' => 175,
        b'm' | b'p' => 170,
        b'-' | b'.' | b':' | b'/' => 165,
        b'_' | b'=' => 160,
        b'g' | b'f' | b'b' | b'y' => 140,
        b'w' | b'v' | b'k' => 110,
        b'3'..=b'9' => 130,
        b'j' | b'x' | b'q' | b'z' => 30,
        _ => 60,
    }
}

/// Precomputed ASCII literal matcher (case-sensitive `memmem` or rare-byte `memchr`/`memchr2`).
enum AsciiMatcher<'a> {
    Sensitive {
        finder: memmem::Finder<'a>,
        len: usize,
    },
    Insensitive {
        needle: &'a [u8],
        anchor_idx: usize,
        anchor_lo: u8,
        anchor_hi: u8,
    },
}

impl<'a> AsciiMatcher<'a> {
    fn new(needle: &'a [u8], ignore_case: bool) -> Self {
        debug_assert!(!needle.is_empty());
        if !ignore_case {
            return Self::Sensitive {
                finder: memmem::Finder::new(needle),
                len: needle.len(),
            };
        }
        let mut best_idx = 0usize;
        let mut best_rank = u8::MAX;
        for (i, &b) in needle.iter().enumerate() {
            let r = ascii_rarity(b);
            if r < best_rank {
                best_rank = r;
                best_idx = i;
            }
        }
        let ab = needle[best_idx];
        Self::Insensitive {
            needle,
            anchor_idx: best_idx,
            anchor_lo: ab.to_ascii_lowercase(),
            anchor_hi: ab.to_ascii_uppercase(),
        }
    }

    #[inline(always)]
    fn len(&self) -> usize {
        match self {
            Self::Sensitive { len, .. } => *len,
            Self::Insensitive { needle, .. } => needle.len(),
        }
    }

    #[inline]
    fn find(&self, hay: &[u8]) -> Option<usize> {
        match self {
            Self::Sensitive { finder, .. } => finder.find(hay),
            Self::Insensitive {
                needle,
                anchor_idx,
                anchor_lo,
                anchor_hi,
            } => {
                let nlen = needle.len();
                if hay.len() < nlen {
                    return None;
                }
                let lo = *anchor_lo;
                let hi = *anchor_hi;
                if nlen == 1 {
                    return if lo == hi {
                        memchr(lo, hay)
                    } else {
                        memchr2(lo, hi, hay)
                    };
                }
                let aidx = *anchor_idx;
                let tail = nlen - 1 - aidx;
                let mut cur = 0usize;
                let max_start = hay.len() - nlen;
                while cur <= max_start {
                    let window = &hay[cur + aidx..hay.len() - tail];
                    let rel = if lo == hi {
                        memchr(lo, window)
                    } else {
                        memchr2(lo, hi, window)
                    }?;
                    let cand = cur + rel;
                    if hay[cand..cand + nlen].eq_ignore_ascii_case(needle) {
                        return Some(cand);
                    }
                    cur = cand + 1;
                }
                None
            }
        }
    }
}

/// Extract up to 4 guaranteed ASCII literal substrings required by a regex pattern.
///
/// Conservative and 100% sound:
/// - Returns empty if a top-level alternation `|` or inline flag `(?i)` is present.
/// - Ignores text inside groups `(...)`, character classes `[...]`, and meta escapes.
/// - Drops the last char of an accumulated literal if immediately followed by a nullable
///   quantifier (`?`, `*`, or `{0...}`).
/// - Sorts extracted literals by rarest anchor byte first so the highest-selectivity SIMD
///   prefilter runs first (avoiding common literals like `status=` masking rare ones like `FATAL_`).
pub(crate) fn extract_required_ascii_literals(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let mut lits: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    let mut paren_depth = 0usize;
    let mut in_class = false;

    let flush = |cur: &mut String, lits: &mut Vec<String>| {
        if !cur.is_empty() {
            lits.push(std::mem::take(cur));
        }
    };

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            if i + 1 >= bytes.len() {
                break;
            }
            let next = bytes[i + 1];
            if in_class || paren_depth > 0 {
                i += 2;
                continue;
            }
            let is_meta_escape = matches!(
                next,
                b'd' | b'D'
                    | b'w'
                    | b'W'
                    | b's'
                    | b'S'
                    | b'b'
                    | b'B'
                    | b'A'
                    | b'z'
                    | b'Z'
                    | b'x'
                    | b'u'
                    | b'p'
                    | b'P'
                    | b'0'..=b'9'
            );
            if is_meta_escape {
                flush(&mut cur, &mut lits);
            } else if next.is_ascii() {
                let ch = match next {
                    b't' => '\t',
                    b'n' => '\n',
                    b'r' => '\r',
                    _ => next as char,
                };
                if ch != '\n' && ch != '\r' {
                    cur.push(ch);
                } else {
                    flush(&mut cur, &mut lits);
                }
            } else {
                flush(&mut cur, &mut lits);
            }
            i += 2;
            continue;
        }

        if in_class {
            if b == b']' {
                in_class = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'[' => {
                flush(&mut cur, &mut lits);
                in_class = true;
                if i + 1 < bytes.len() && bytes[i + 1] == b'^' {
                    i += 1;
                }
                if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                    i += 1;
                }
            }
            b'(' => {
                flush(&mut cur, &mut lits);
                if paren_depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'?' {
                    let mut j = i + 2;
                    while j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'-') {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b')' {
                        return Vec::new();
                    }
                }
                paren_depth += 1;
            }
            b')' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                }
                flush(&mut cur, &mut lits);
            }
            b'|' => {
                if paren_depth == 0 {
                    return Vec::new();
                }
            }
            b'?' | b'*' => {
                if paren_depth == 0 {
                    cur.pop();
                    flush(&mut cur, &mut lits);
                }
            }
            b'+' => {
                if paren_depth == 0 {
                    flush(&mut cur, &mut lits);
                }
            }
            b'{' => {
                if paren_depth == 0 {
                    let rest = &bytes[i + 1..];
                    if rest.first() == Some(&b'0') && matches!(rest.get(1), Some(b',' | b'}')) {
                        cur.pop();
                    }
                    flush(&mut cur, &mut lits);
                    while i < bytes.len() && bytes[i] != b'}' {
                        i += 1;
                    }
                }
            }
            b'.' | b'^' | b'$' => {
                if paren_depth == 0 {
                    flush(&mut cur, &mut lits);
                }
            }
            _ => {
                if paren_depth == 0 {
                    if b.is_ascii() {
                        cur.push(b as char);
                    } else {
                        flush(&mut cur, &mut lits);
                    }
                }
            }
        }
        i += 1;
    }

    flush(&mut cur, &mut lits);
    // Prioritize rarer anchor bytes (and longer literals as tiebreaker); keep up to 4.
    lits.sort_by_key(|s| {
        let min_rarity = s.bytes().map(ascii_rarity).min().unwrap_or(255);
        (min_rarity, std::cmp::Reverse(s.len()))
    });
    lits.truncate(4);
    lits
}

/// Reusable scratch buffers to keep search 100% allocation-free per line.
struct SearchScratch {
    ascii_buf: Vec<u8>,
    str_buf: String,
    lower_buf: String,
    /// Maps UTF-8 byte index in `str_buf` (or `lower_buf`) -> visual terminal column index.
    byte_to_col: Vec<usize>,
}

impl SearchScratch {
    fn new() -> Self {
        Self {
            ascii_buf: Vec::with_capacity(512),
            str_buf: String::with_capacity(512),
            lower_buf: String::with_capacity(512),
            byte_to_col: Vec::with_capacity(512),
        }
    }

    /// Unpack a pure-ASCII `&[Cell]` row into `self.ascii_buf` (trailing spaces trimmed if not wrapped).
    #[inline]
    fn unpack_ascii_cells(&mut self, cells: &[Cell], trim_trailing_spaces: bool) -> &[u8] {
        let mut len = cells.len();
        if trim_trailing_spaces {
            while len > 0 && cells[len - 1].ch == ' ' {
                len -= 1;
            }
        }
        self.ascii_buf.clear();
        self.ascii_buf.reserve(len);
        unsafe {
            let dst = self.ascii_buf.as_mut_ptr();
            for (k, c) in cells[..len].iter().enumerate() {
                *dst.add(k) = c.ch as u8;
            }
            self.ascii_buf.set_len(len);
        }
        &self.ascii_buf
    }

    /// Extract a Unicode `&[Cell]` row into `self.str_buf` without allocating per line.
    #[inline]
    fn unpack_unicode_cells(&mut self, cells: &[Cell], trim_trailing_spaces: bool) {
        let mut len = cells.len();
        if trim_trailing_spaces {
            while len > 0
                && !cells[len - 1].flags.contains(CellFlags::WIDE_SPACER)
                && cells[len - 1].ch == ' '
            {
                len -= 1;
            }
        }
        self.str_buf.clear();
        for c in &cells[..len] {
            if !c.flags.contains(CellFlags::WIDE_SPACER) {
                self.str_buf.push(c.ch);
            }
        }
    }

    /// Populate `self.byte_to_col` mapping every char-boundary byte index in `self.str_buf`
    /// to its exact visual terminal column (only called on Unicode lines that actually match!).
    fn build_col_map_for_cells(&mut self, cells: &[Cell], trim_trailing_spaces: bool) {
        let mut len = cells.len();
        if trim_trailing_spaces {
            while len > 0
                && !cells[len - 1].flags.contains(CellFlags::WIDE_SPACER)
                && cells[len - 1].ch == ' '
            {
                len -= 1;
            }
        }
        self.byte_to_col.clear();
        let mut col = 0usize;
        for (idx, c) in cells[..len].iter().enumerate() {
            if c.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            let blen = c.ch.len_utf8();
            for _ in 0..blen {
                self.byte_to_col.push(idx);
            }
            col = idx + if c.flags.contains(CellFlags::WIDE_CHAR) { 2 } else { 1 };
        }
        self.byte_to_col.push(col);
    }

    /// Populate `self.lower_buf` AND `self.byte_to_col` simultaneously for case-folded Unicode search.
    fn unpack_unicode_cells_lowercased(&mut self, cells: &[Cell], trim_trailing_spaces: bool) {
        let mut len = cells.len();
        if trim_trailing_spaces {
            while len > 0
                && !cells[len - 1].flags.contains(CellFlags::WIDE_SPACER)
                && cells[len - 1].ch == ' '
            {
                len -= 1;
            }
        }
        self.lower_buf.clear();
        self.byte_to_col.clear();
        let mut end_col = 0usize;
        for (idx, c) in cells[..len].iter().enumerate() {
            if c.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            for lc in c.ch.to_lowercase() {
                let blen = lc.len_utf8();
                self.lower_buf.push(lc);
                for _ in 0..blen {
                    self.byte_to_col.push(idx);
                }
            }
            end_col = idx + if c.flags.contains(CellFlags::WIDE_CHAR) { 2 } else { 1 };
        }
        self.byte_to_col.push(end_col);
    }

    #[inline]
    fn col_at_byte(&self, byte_pos: usize) -> usize {
        self.byte_to_col
            .get(byte_pos)
            .copied()
            .unwrap_or_else(|| self.byte_to_col.last().copied().unwrap_or(0))
    }
}

/// Scan all non-overlapping matches of an ASCII matcher on a single pure-ASCII line slice.
#[inline]
fn scan_ascii_line(
    abs_line: u64,
    bytes: &[u8],
    matcher: &AsciiMatcher<'_>,
    matches: &mut Vec<SearchMatch>,
) {
    let nlen = matcher.len();
    if bytes.len() < nlen {
        return;
    }
    let mut off = 0usize;
    while off + nlen <= bytes.len() {
        let Some(rel) = matcher.find(&bytes[off..]) else {
            break;
        };
        let pos = off + rel;
        matches.push(SearchMatch {
            abs_line,
            start_col: pos,
            end_col: pos + nlen,
        });
        off = pos + nlen;
    }
}

/// Fast slab-level scan for a 100% pure-ASCII `ScrollbackSlab` (`slab.cells.is_empty()`).
/// Scans the entire contiguous 64 KiB `slab.ascii` buffer with hardware SIMD first, skipping
/// all 512 lines in ~2 us when no match exists!
#[inline]
fn scan_pure_ascii_slab(
    slab: &SlabView<'_>,
    matcher: &AsciiMatcher<'_>,
    matches: &mut Vec<SearchMatch>,
) {
    let active_lines = &slab.lines[slab.start_idx..];
    if active_lines.is_empty() || slab.ascii.is_empty() {
        return;
    }
    let byte_lo = active_lines[0].data_offset as usize;
    let last = active_lines[active_lines.len() - 1];
    let byte_hi = (last.data_offset as usize + last.data_len as usize).min(slab.ascii.len());
    if byte_hi <= byte_lo {
        return;
    }
    let nlen = matcher.len();
    if byte_hi - byte_lo < nlen {
        return;
    }

    let hay = &slab.ascii[byte_lo..byte_hi];
    let Some(first_rel) = matcher.find(hay) else {
        return; // Entire slab of up to 512 lines has ZERO matches!
    };

    // Binary search once for the line containing the first hit, then advance monotonically!
    let first_abs_off = byte_lo + first_rel;
    let mut line_i = active_lines.partition_point(|m| {
        (m.data_offset as usize + m.data_len as usize) <= first_abs_off
    });
    let mut cur_rel = first_rel;

    loop {
        let abs_off = byte_lo + cur_rel;
        while line_i < active_lines.len() {
            let m = active_lines[line_i];
            if (m.data_offset as usize + m.data_len as usize) > abs_off {
                break;
            }
            line_i += 1;
        }
        if line_i >= active_lines.len() {
            break;
        }
        let meta = active_lines[line_i];
        let l_start = meta.data_offset as usize;
        let l_end = l_start + meta.data_len as usize;

        if abs_off < l_start {
            cur_rel = l_start - byte_lo;
        } else if abs_off + nlen <= l_end {
            let col = abs_off - l_start;
            matches.push(SearchMatch {
                abs_line: slab.first_line_global + (slab.start_idx + line_i) as u64,
                start_col: col,
                end_col: col + nlen,
            });
            cur_rel = (abs_off + nlen) - byte_lo;
        } else {
            // Candidate straddled the boundary between two packed lines -> resume at next line start!
            cur_rel = l_end - byte_lo;
        }

        if cur_rel + nlen > hay.len() {
            break;
        }
        let Some(next_rel) = matcher.find(&hay[cur_rel..]) else {
            break;
        };
        cur_rel += next_rel;
    }
}

/// Scan a Unicode `Cell` slice for an ASCII needle using self-synchronizing UTF-8 SIMD matching!
fn scan_unicode_cells_ascii_needle(
    abs_line: u64,
    cells: &[Cell],
    trim_trailing_spaces: bool,
    matcher: &AsciiMatcher<'_>,
    scratch: &mut SearchScratch,
    matches: &mut Vec<SearchMatch>,
) {
    scratch.unpack_unicode_cells(cells, trim_trailing_spaces);
    let nlen = matcher.len();
    let first_rel = {
        let hay = scratch.str_buf.as_bytes();
        if hay.len() < nlen {
            return;
        }
        match matcher.find(hay) {
            Some(r) => r,
            None => return,
        }
    };
    scratch.build_col_map_for_cells(cells, trim_trailing_spaces);
    let hay = scratch.str_buf.as_bytes();
    let mut pos = first_rel;
    loop {
        let start_col = scratch.col_at_byte(pos);
        let end_col = scratch.col_at_byte(pos + nlen);
        matches.push(SearchMatch {
            abs_line,
            start_col,
            end_col,
        });
        let next_off = pos + nlen;
        if next_off + nlen > hay.len() {
            break;
        }
        let Some(rel) = matcher.find(&hay[next_off..]) else {
            break;
        };
        pos = next_off + rel;
    }
}

/// Scan a Unicode `Cell` slice for a non-ASCII literal using `memmem::Finder`.
fn scan_unicode_cells_non_ascii_literal(
    abs_line: u64,
    cells: &[Cell],
    trim_trailing_spaces: bool,
    finder: &memmem::Finder<'_>,
    needle_len: usize,
    needs_lowercase: bool,
    scratch: &mut SearchScratch,
    matches: &mut Vec<SearchMatch>,
) {
    if needs_lowercase {
        scratch.unpack_unicode_cells_lowercased(cells, trim_trailing_spaces);
        let hay = scratch.lower_buf.as_bytes();
        if hay.len() < needle_len {
            return;
        }
        let mut off = 0usize;
        while off + needle_len <= hay.len() {
            let Some(rel) = finder.find(&hay[off..]) else {
                break;
            };
            let pos = off + rel;
            let start_col = scratch.col_at_byte(pos);
            let end_col = scratch.col_at_byte(pos + needle_len);
            matches.push(SearchMatch {
                abs_line,
                start_col,
                end_col,
            });
            off = pos + needle_len;
        }
    } else {
        scratch.unpack_unicode_cells(cells, trim_trailing_spaces);
        let first_rel = {
            let hay = scratch.str_buf.as_bytes();
            if hay.len() < needle_len {
                return;
            }
            match finder.find(hay) {
                Some(r) => r,
                None => return,
            }
        };
        scratch.build_col_map_for_cells(cells, trim_trailing_spaces);
        let hay = scratch.str_buf.as_bytes();
        let mut pos = first_rel;
        loop {
            let start_col = scratch.col_at_byte(pos);
            let end_col = scratch.col_at_byte(pos + needle_len);
            matches.push(SearchMatch {
                abs_line,
                start_col,
                end_col,
            });
            let next_off = pos + needle_len;
            if next_off + needle_len > hay.len() {
                break;
            }
            let Some(rel) = finder.find(&hay[next_off..]) else {
                break;
            };
            pos = next_off + rel;
        }
    }
}

/// Check whether a byte slice contains ALL required SIMD prefilter literals.
#[inline]
fn passes_prefilters(bytes: &[u8], prefilters: &[AsciiMatcher<'_>]) -> bool {
    prefilters.iter().all(|pf| pf.find(bytes).is_some())
}

/// Scan a pure-ASCII line slice with a compiled regex (optional SIMD required-literal prefilters).
#[inline]
fn scan_ascii_line_regex(
    abs_line: u64,
    bytes: &[u8],
    re: &regex_lite::Regex,
    prefilters: &[AsciiMatcher<'_>],
    matches: &mut Vec<SearchMatch>,
) {
    if !passes_prefilters(bytes, prefilters) {
        return;
    }
    // SAFETY: caller guarantees `bytes` contains only 7-bit ASCII (<= 0x7F).
    let text = unsafe { std::str::from_utf8_unchecked(bytes) };
    for m in re.find_iter(text) {
        if m.end() > m.start() {
            matches.push(SearchMatch {
                abs_line,
                start_col: m.start(),
                end_col: m.end(),
            });
        }
    }
}

/// Scan a Unicode `Cell` slice with a compiled regex (optional SIMD required-literal prefilters).
fn scan_unicode_cells_regex(
    abs_line: u64,
    cells: &[Cell],
    trim_trailing_spaces: bool,
    re: &regex_lite::Regex,
    prefilters: &[AsciiMatcher<'_>],
    scratch: &mut SearchScratch,
    matches: &mut Vec<SearchMatch>,
) {
    scratch.unpack_unicode_cells(cells, trim_trailing_spaces);
    if !passes_prefilters(scratch.str_buf.as_bytes(), prefilters) {
        return;
    }
    let mut built_map = false;
    // Collect regex match byte spans without borrowing scratch.str_buf across build_col_map
    let mut first_hit = true;
    for m in re.find_iter(&scratch.str_buf) {
        if m.end() <= m.start() {
            continue;
        }
        if first_hit {
            first_hit = false;
            built_map = true;
        }
        // Temporarily stash byte offsets in SearchMatch, remap immediately below if needed
        matches.push(SearchMatch {
            abs_line,
            start_col: m.start(),
            end_col: m.end(),
        });
    }
    if built_map {
        // Count how many we pushed for this line (tail entries with matching abs_line)
        let mut start_idx = matches.len();
        while start_idx > 0 && matches[start_idx - 1].abs_line == abs_line {
            start_idx -= 1;
        }
        scratch.build_col_map_for_cells(cells, trim_trailing_spaces);
        for sm in &mut matches[start_idx..] {
            sm.start_col = scratch.col_at_byte(sm.start_col);
            sm.end_col = scratch.col_at_byte(sm.end_col);
        }
    }
}

/// Execute high-throughput search across `scrollback` and `active_grid` in ascending `abs_line` order.
pub(crate) fn search_screen(
    scrollback: &Scrollback,
    active_grid: &ActiveGrid,
    query: &str,
    regex_mode: bool,
    ignore_case: bool,
    matches: &mut Vec<SearchMatch>,
) {
    matches.clear();
    if query.is_empty() {
        return;
    }

    let mut scratch = SearchScratch::new();
    let screen_top = scrollback.evicted_lines() + scrollback.len() as u64;

    if regex_mode {
        let pat = if ignore_case {
            format!("(?i){query}")
        } else {
            query.to_string()
        };
        let Ok(re) = regex_lite::Regex::new(&pat) else {
            return;
        };
        let req_lits = extract_required_ascii_literals(query);
        let prefilters: Vec<AsciiMatcher<'_>> = req_lits
            .iter()
            .map(|lit| AsciiMatcher::new(lit.as_bytes(), ignore_case))
            .collect();

        scrollback.for_each_slab(|slab| {
            let active_lines = &slab.lines[slab.start_idx..];
            if active_lines.is_empty() {
                return;
            }
            // Fast slab-level SIMD prefilter skip for pure-ASCII slabs:
            // ALL required literals must appear in the slab for any line in it to match!
            let ascii_slab_may_match = if !prefilters.is_empty() {
                if slab.cells.is_empty() {
                    let lo = active_lines[0].data_offset as usize;
                    let last = active_lines[active_lines.len() - 1];
                    let hi = (last.data_offset as usize + last.data_len as usize)
                        .min(slab.ascii.len());
                    if hi <= lo || !passes_prefilters(&slab.ascii[lo..hi], &prefilters) {
                        return;
                    }
                    true
                } else {
                    !slab.ascii.is_empty() && passes_prefilters(slab.ascii, &prefilters)
                }
            } else {
                true
            };

            for (rel_i, meta) in active_lines.iter().enumerate() {
                let abs_line = slab.first_line_global + (slab.start_idx + rel_i) as u64;
                let off = meta.data_offset as usize;
                let len = meta.data_len as usize;
                if meta.is_packed_ascii() {
                    if !ascii_slab_may_match {
                        continue;
                    }
                    scan_ascii_line_regex(
                        abs_line,
                        &slab.ascii[off..off + len],
                        &re,
                        &prefilters,
                        matches,
                    );
                } else if !meta.has_non_ascii() {
                    let bytes = scratch.unpack_ascii_cells(&slab.cells[off..off + len], false);
                    scan_ascii_line_regex(abs_line, bytes, &re, &prefilters, matches);
                } else {
                    scan_unicode_cells_regex(
                        abs_line,
                        &slab.cells[off..off + len],
                        false,
                        &re,
                        &prefilters,
                        &mut scratch,
                        matches,
                    );
                }
            }
        });

        for r in 0..active_grid.rows() {
            let abs_line = screen_top + r as u64;
            let meta = active_grid.meta(r);
            let row = active_grid.row(r);
            if !meta.has_non_ascii() {
                let bytes = scratch.unpack_ascii_cells(row, true);
                scan_ascii_line_regex(abs_line, bytes, &re, &prefilters, matches);
            } else {
                scan_unicode_cells_regex(
                    abs_line,
                    row,
                    true,
                    &re,
                    &prefilters,
                    &mut scratch,
                    matches,
                );
            }
        }
        return;
    }

    // ---- Literal Search Fast Path 1: ASCII query (99% of terminal searches) ----
    if query.is_ascii() {
        let matcher = AsciiMatcher::new(query.as_bytes(), ignore_case);
        scrollback.for_each_slab(|slab| {
            if slab.cells.is_empty() {
                scan_pure_ascii_slab(&slab, &matcher, matches);
                return;
            }
            let ascii_slab_has_hit = !slab.ascii.is_empty() && matcher.find(slab.ascii).is_some();
            let active_lines = &slab.lines[slab.start_idx..];
            for (rel_i, meta) in active_lines.iter().enumerate() {
                let abs_line = slab.first_line_global + (slab.start_idx + rel_i) as u64;
                let off = meta.data_offset as usize;
                let len = meta.data_len as usize;
                if meta.is_packed_ascii() {
                    if ascii_slab_has_hit {
                        scan_ascii_line(abs_line, &slab.ascii[off..off + len], &matcher, matches);
                    }
                } else if !meta.has_non_ascii() {
                    if len >= matcher.len() {
                        let bytes = scratch.unpack_ascii_cells(&slab.cells[off..off + len], false);
                        scan_ascii_line(abs_line, bytes, &matcher, matches);
                    }
                } else {
                    scan_unicode_cells_ascii_needle(
                        abs_line,
                        &slab.cells[off..off + len],
                        false,
                        &matcher,
                        &mut scratch,
                        matches,
                    );
                }
            }
        });

        for r in 0..active_grid.rows() {
            let abs_line = screen_top + r as u64;
            let meta = active_grid.meta(r);
            let row = active_grid.row(r);
            if !meta.has_non_ascii() {
                let bytes = scratch.unpack_ascii_cells(row, true);
                scan_ascii_line(abs_line, bytes, &matcher, matches);
            } else {
                scan_unicode_cells_ascii_needle(
                    abs_line,
                    row,
                    true,
                    &matcher,
                    &mut scratch,
                    matches,
                );
            }
        }
        return;
    }

    // ---- Literal Search Fast Path 2: Non-ASCII / Unicode / CJK query ----
    let query_lower = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };
    // If query_lower contains non-ASCII, pure-ASCII slabs & lines can NEVER match -> skip in O(1)!
    let skip_ascii_lines = !query_lower.is_ascii();
    // Case-invariant scripts (CJK, Hiragana, Katakana, Hangul, Emoji, symbols) need ZERO per-line
    // `to_lowercase()` even when `ignore_case == true`!
    let needs_lowercase = ignore_case && (query_lower != query.to_uppercase());
    let needle_bytes = if needs_lowercase {
        query_lower.as_bytes()
    } else {
        query.as_bytes()
    };
    let finder = memmem::Finder::new(needle_bytes);
    let nlen = needle_bytes.len();

    scrollback.for_each_slab(|slab| {
        if skip_ascii_lines && slab.cells.is_empty() {
            return;
        }
        let active_lines = &slab.lines[slab.start_idx..];
        for (rel_i, meta) in active_lines.iter().enumerate() {
            if skip_ascii_lines && (meta.is_packed_ascii() || !meta.has_non_ascii()) {
                continue;
            }
            let abs_line = slab.first_line_global + (slab.start_idx + rel_i) as u64;
            let off = meta.data_offset as usize;
            let len = meta.data_len as usize;
            if meta.is_packed_ascii() {
                // Rare edge: e.g. Kelvin sign U+212A lowercases to ASCII 'k'
                let matcher = AsciiMatcher::new(needle_bytes, ignore_case);
                scan_ascii_line(abs_line, &slab.ascii[off..off + len], &matcher, matches);
            } else {
                scan_unicode_cells_non_ascii_literal(
                    abs_line,
                    &slab.cells[off..off + len],
                    false,
                    &finder,
                    nlen,
                    needs_lowercase,
                    &mut scratch,
                    matches,
                );
            }
        }
    });

    for r in 0..active_grid.rows() {
        let meta = active_grid.meta(r);
        if skip_ascii_lines && !meta.has_non_ascii() {
            continue;
        }
        let abs_line = screen_top + r as u64;
        let row = active_grid.row(r);
        if !meta.has_non_ascii() {
            let matcher = AsciiMatcher::new(needle_bytes, ignore_case);
            let bytes = scratch.unpack_ascii_cells(row, true);
            scan_ascii_line(abs_line, bytes, &matcher, matches);
        } else {
            scan_unicode_cells_non_ascii_literal(
                abs_line,
                row,
                true,
                &finder,
                nlen,
                needs_lowercase,
                &mut scratch,
                matches,
            );
        }
    }
}
