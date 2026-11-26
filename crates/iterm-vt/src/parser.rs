/// Maximum number of CSI numeric parameters stored inline without heap allocation.
pub const MAX_CSI_PARAMS: usize = 32;

/// Zero-allocation CSI parameter container supporting both `;` parameters and `:` subparameters
/// (required for `38:2::r:g:b`, `4:3` curly underline, etc.).
#[derive(Debug, Clone)]
pub struct CsiParams {
    params: [u16; MAX_CSI_PARAMS],
    /// Subparameter group index for each entry (allows grouping `38:2::r:g:b`).
    group: [u8; MAX_CSI_PARAMS],
    len: usize,
    cur_group: u8,
    has_current: bool,
    current: u16,
}

impl Default for CsiParams {
    fn default() -> Self {
        Self::new()
    }
}

impl CsiParams {
    #[inline]
    pub const fn new() -> Self {
        Self {
            params: [0; MAX_CSI_PARAMS],
            group: [0; MAX_CSI_PARAMS],
            len: 0,
            cur_group: 0,
            has_current: false,
            current: 0,
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
        self.cur_group = 0;
        self.has_current = false;
        self.current = 0;
    }

    #[inline]
    pub fn push_digit(&mut self, b: u8) {
        self.has_current = true;
        self.current = self.current.saturating_mul(10).saturating_add((b - b'0') as u16);
    }

    #[inline]
    pub fn push_raw(&mut self, val: u16, advance_group: bool) {
        if self.len < MAX_CSI_PARAMS {
            self.params[self.len] = val;
            self.group[self.len] = self.cur_group;
            self.len += 1;
        }
        if advance_group {
            self.cur_group = self.cur_group.saturating_add(1);
        }
    }

    #[inline]
    pub fn push_param_sep(&mut self) {
        if self.len < MAX_CSI_PARAMS {
            self.params[self.len] = if self.has_current { self.current } else { 0 };
            self.group[self.len] = self.cur_group;
            self.len += 1;
        }
        self.cur_group = self.cur_group.saturating_add(1);
        self.current = 0;
        self.has_current = false;
    }

    #[inline]
    pub fn push_subparam_sep(&mut self) {
        if self.len < MAX_CSI_PARAMS {
            self.params[self.len] = if self.has_current { self.current } else { 0 };
            self.group[self.len] = self.cur_group;
            self.len += 1;
        }
        self.current = 0;
        self.has_current = false;
    }

    #[inline]
    pub fn finish(&mut self) {
        if self.has_current || self.len > 0 {
            if self.len < MAX_CSI_PARAMS {
                self.params[self.len] = if self.has_current { self.current } else { 0 };
                self.group[self.len] = self.cur_group;
                self.len += 1;
            }
            self.has_current = false;
            self.current = 0;
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<u16> {
        if idx < self.len {
            Some(self.params[idx])
        } else {
            None
        }
    }

    #[inline]
    pub fn get_or(&self, idx: usize, default: u16) -> u16 {
        self.get(idx).map(|v| if v == 0 { default } else { v }).unwrap_or(default)
    }

    #[inline]
    pub fn group_of(&self, idx: usize) -> u8 {
        if idx < self.len {
            self.group[idx]
        } else {
            255
        }
    }

    #[inline]
    pub fn slice(&self) -> &[u16] {
        &self.params[..self.len]
    }
}

/// Callback sink invoked by `VtParser`.
pub trait VtSink {
    /// Optional bulk multi-line plain-ASCII fast path (bypasses 16-byte `Cell` expansion for lines
    /// guaranteed to scroll directly into 1-byte/char scrollback!). Returns bytes consumed (`0` = fallback).
    #[inline]
    fn try_bulk_ascii_lines(&mut self, _bytes: &[u8]) -> usize {
        0
    }
    /// Ultra-fast bulk printable ASCII run (`0x20..=0x7E` only).
    fn print_ascii_run(&mut self, bytes: &[u8]);
    /// Fast bulk UTF-8 multi-byte run (non-ASCII Unicode/CJK/box-drawing).
    fn print_utf8_run(&mut self, s: &str) {
        for ch in s.chars() {
            self.print_char(ch);
        }
    }
    /// Single Unicode scalar value (typically non-ASCII).
    fn print_char(&mut self, ch: char);
    /// C0/C1 control byte (`\r`, `\n`, `\t`, `\x08`, `\x07`, etc.).
    fn execute(&mut self, byte: u8);
    /// Fast direct SGR reset (`\x1b[m` / `\x1b[0m`) bypassing `CsiParams`.
    #[inline]
    fn csi_sgr_reset(&mut self) {
        let p = CsiParams::new();
        self.csi_dispatch(&p, &[], b'm');
    }
    /// Fast direct SGR single basic attribute (`\x1b[31m`, `\x1b[1m`, `\x1b[49m`, etc.).
    #[inline]
    fn csi_sgr_single(&mut self, code: u16) {
        let mut p = CsiParams::new();
        p.push_raw(code, true);
        self.csi_dispatch(&p, &[], b'm');
    }
    /// Fast direct SGR 24-bit truecolor (`is_bg == false` -> 38;2;r;g;b, `true` -> 48;2;r;g;b).
    #[inline]
    fn csi_sgr_rgb(&mut self, is_bg: bool, r: u8, g: u8, b: u8) {
        let mut p = CsiParams::new();
        p.push_raw(if is_bg { 48 } else { 38 }, true);
        p.push_raw(2, true);
        p.push_raw(r as u16, true);
        p.push_raw(g as u16, true);
        p.push_raw(b as u16, true);
        self.csi_dispatch(&p, &[], b'm');
    }
    /// Fast direct SGR 256-palette (`is_bg == false` -> 38;5;idx, `true` -> 48;5;idx).
    #[inline]
    fn csi_sgr_indexed(&mut self, is_bg: bool, idx: u8) {
        let mut p = CsiParams::new();
        p.push_raw(if is_bg { 48 } else { 38 }, true);
        p.push_raw(5, true);
        p.push_raw(idx as u16, true);
        self.csi_dispatch(&p, &[], b'm');
    }
    /// Fast direct multi-parameter `;`-separated SGR slice (e.g. `1;38;2;r;g;b;48;5;236m`, `38;5;c1;48;5;c2m`).
    #[inline]
    fn csi_sgr_slice(&mut self, params: &[u16]) {
        let mut p = CsiParams::new();
        for &v in params {
            p.push_raw(v, true);
        }
        self.csi_dispatch(&p, &[], b'm');
    }
    /// Fast direct EL (`\x1b[K` / `\x1b[0K` / `\x1b[1K` / `\x1b[2K`).
    #[inline]
    fn csi_el(&mut self, mode: u16) {
        let mut p = CsiParams::new();
        p.push_raw(mode, true);
        self.csi_dispatch(&p, &[], b'K');
    }
    /// Fast direct CUP (`\x1b[row;colH`) bypassing `CsiParams`.
    #[inline]
    fn csi_cup(&mut self, row1: u16, col1: u16) {
        let mut p = CsiParams::new();
        p.push_raw(row1, true);
        p.push_raw(col1, true);
        self.csi_dispatch(&p, &[], b'H');
    }
    /// CSI sequence dispatched (`intermediates`, `private` prefix e.g. `?` / `>` / `=`, `params`, `final_byte`).
    fn csi_dispatch(&mut self, params: &CsiParams, intermediates: &[u8], final_byte: u8);
    /// ESC sequence dispatched (`intermediates`, `final_byte`).
    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8);
    /// OSC sequence dispatched (raw payload bytes between `ESC ]` and `BEL`/`ST`).
    fn osc_dispatch(&mut self, payload: &[u8]);
    /// DCS sequence dispatched (non-tmux DCS sequences; `tmux;` passthrough is automatically unwrapped!).
    fn dcs_dispatch(&mut self, params: &CsiParams, intermediates: &[u8], final_byte: u8, data: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIgnore,
    OscString,
    OscEsc,
    DcsEntry,
    DcsParam,
    DcsPassthrough,
    DcsEsc,
    IgnoreString,
    IgnoreStringEsc,
}

/// Maximum OSC/DCS payload buffer (64 MiB cap to support high-res `imgcat` / `it2dl` payloads safely).
const MAX_STRING_PAYLOAD: usize = 64 * 1024 * 1024;

/// High-throughput zero-allocation VT100/xterm/iTerm2 parser with SWAR ASCII fast path
/// and native tmux DCS passthrough unwrapping.
pub struct VtParser {
    state: State,
    params: CsiParams,
    intermediates: [u8; 4],
    inter_len: usize,
    osc_buf: Vec<u8>,
    dcs_buf: Vec<u8>,
    dcs_final: u8,
    utf8_buf: [u8; 4],
    utf8_len: u8,
    utf8_expected: u8,
    /// Recursion guard for tmux DCS passthrough unwrapping.
    passthrough_depth: u8,
}

impl Default for VtParser {
    fn default() -> Self {
        Self::new()
    }
}

impl VtParser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            params: CsiParams::new(),
            intermediates: [0; 4],
            inter_len: 0,
            osc_buf: Vec::with_capacity(256),
            dcs_buf: Vec::with_capacity(256),
            dcs_final: 0,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
            passthrough_depth: 0,
        }
    }

    #[inline]
    fn push_intermediate(&mut self, b: u8) {
        if self.inter_len < self.intermediates.len() {
            self.intermediates[self.inter_len] = b;
            self.inter_len += 1;
        }
    }

    /// Scan contiguous printable ASCII (`0x20..=0x7E`) starting at `bytes[0]` using hardware SIMD
    /// (32-byte unrolled SSE2 on x86_64, 16-byte NEON nibblemask on Apple Silicon aarch64, 8-byte SWAR fallback).
    #[inline]
    pub(crate) fn scan_printable_ascii(bytes: &[u8]) -> usize {
        let len = bytes.len();
        let mut i = 0;

        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            let low = _mm_set1_epi8(0x20u8 as i8);
            let high = _mm_set1_epi8(0x7Eu8 as i8);
            // 32-byte unrolled SSE2 loop (2x instruction-level parallelism on modern CPUs!)
            while i + 32 <= len {
                let p = bytes.as_ptr().add(i);
                let v0 = _mm_loadu_si128(p as *const __m128i);
                let v1 = _mm_loadu_si128(p.add(16) as *const __m128i);
                let b0 = _mm_or_si128(_mm_cmplt_epi8(v0, low), _mm_cmpgt_epi8(v0, high));
                let b1 = _mm_or_si128(_mm_cmplt_epi8(v1, low), _mm_cmpgt_epi8(v1, high));
                let m0 = _mm_movemask_epi8(b0);
                if m0 != 0 {
                    return i + m0.trailing_zeros() as usize;
                }
                let m1 = _mm_movemask_epi8(b1);
                if m1 != 0 {
                    return i + 16 + m1.trailing_zeros() as usize;
                }
                i += 32;
            }
            while i + 16 <= len {
                let v = _mm_loadu_si128(bytes.as_ptr().add(i) as *const __m128i);
                let bad = _mm_or_si128(_mm_cmplt_epi8(v, low), _mm_cmpgt_epi8(v, high));
                let mask = _mm_movemask_epi8(bad);
                if mask != 0 {
                    return i + mask.trailing_zeros() as usize;
                }
                i += 16;
            }
        }

        #[cfg(target_arch = "aarch64")]
        unsafe {
            use std::arch::aarch64::*;
            let low = vdupq_n_u8(0x20);
            let high = vdupq_n_u8(0x7E);
            while i + 16 <= len {
                let v = vld1q_u8(bytes.as_ptr().add(i));
                let bad = vorrq_u8(vcltq_u8(v, low), vcgtq_u8(v, high));
                // Pack 16 lane masks into 64-bit nibblemask (4 bits/lane) for single-instruction lane index!
                let nibbles = vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(bad), 4)), 0);
                if nibbles != 0 {
                    return i + (nibbles.trailing_zeros() as usize >> 2);
                }
                i += 16;
            }
        }

        while i + 8 <= len {
            let word = u64::from_ne_bytes(bytes[i..i + 8].try_into().unwrap());
            let high_bit = word & 0x8080_8080_8080_8080;
            let is_del = (word.wrapping_add(0x0101_0101_0101_0101)) & 0x8080_8080_8080_8080;
            let below_space = (word.wrapping_sub(0x2020_2020_2020_2020)) & 0x8080_8080_8080_8080;
            if (high_bit | is_del | below_space) != 0 {
                break;
            }
            i += 8;
        }

        while i < len {
            let b = bytes[i];
            if !(0x20..=0x7E).contains(&b) {
                break;
            }
            i += 1;
        }
        i
    }

    /// Fast inline CSI parser for complete `\x1b[...<final>` sequences in `Ground` state.
    /// Specializes the hottest SGR (`m`) and CUP (`H`) forms directly without touching `CsiParams`!
    #[inline]
    fn try_fast_csi<S: VtSink>(&mut self, tail: &[u8], sink: &mut S) -> Option<usize> {
        let len = tail.len();
        if len == 0 {
            return None;
        }
        let b0 = tail[0];

        // Ultra-fast direct path for non-private SGR (`m`), CUP (`H`), and EL (`K`) — ~98% of real-world CSI!
        if b0 == b'm' {
            sink.csi_sgr_reset();
            return Some(1);
        }
        if b0 == b'H' {
            sink.csi_cup(1, 1);
            return Some(1);
        }
        if b0 == b'K' {
            sink.csi_el(0);
            return Some(1);
        }
        if b0.is_ascii_digit() {
            let mut p = [0u16; 12];
            let mut pc = 0usize;
            let mut cur = (b0 - b'0') as u16;
            let mut idx = 1usize;
            let mut simple = true;
            while idx < len {
                let b = tail[idx];
                if b.is_ascii_digit() {
                    cur = cur.saturating_mul(10).saturating_add((b - b'0') as u16);
                } else if b == b';' {
                    if pc < 11 {
                        p[pc] = cur;
                        pc += 1;
                        cur = 0;
                    } else {
                        simple = false;
                        break;
                    }
                } else if b == b'm' {
                    p[pc] = cur;
                    pc += 1;
                    match pc {
                        1 => {
                            if p[0] == 0 {
                                sink.csi_sgr_reset();
                            } else {
                                sink.csi_sgr_single(p[0]);
                            }
                        }
                        3 if (p[0] == 38 || p[0] == 48) && p[1] == 5 => {
                            sink.csi_sgr_indexed(p[0] == 48, p[2] as u8);
                        }
                        5 if (p[0] == 38 || p[0] == 48) && p[1] == 2 => {
                            sink.csi_sgr_rgb(p[0] == 48, p[2] as u8, p[3] as u8, p[4] as u8);
                        }
                        _ => sink.csi_sgr_slice(&p[..pc]),
                    }
                    return Some(idx + 1);
                } else if b == b'H' {
                    p[pc] = cur;
                    pc += 1;
                    if pc == 1 {
                        sink.csi_cup(p[0], 1);
                        return Some(idx + 1);
                    } else if pc == 2 {
                        sink.csi_cup(p[0], p[1]);
                        return Some(idx + 1);
                    } else {
                        simple = false;
                        break;
                    }
                } else if b == b'K' && pc == 0 {
                    sink.csi_el(cur);
                    return Some(idx + 1);
                } else {
                    simple = false;
                    break;
                }
                idx += 1;
            }
            if simple {
                // Reached end of slice without final byte
                return None;
            }
        }

        let mut idx = 0usize;
        self.params.clear();
        self.inter_len = 0;

        // Optional private prefix (`?`, `>`, `<`, `=`)
        if (b'<'..=b'?').contains(&b0) {
            self.intermediates[0] = b0;
            self.inter_len = 1;
            idx = 1;
        }

        let mut cur: u16 = 0;
        let mut has_cur = false;
        while idx < len {
            let b = tail[idx];
            match b {
                b'0'..=b'9' => {
                    has_cur = true;
                    cur = cur.saturating_mul(10).saturating_add((b - b'0') as u16);
                }
                b';' => {
                    self.params.push_raw(if has_cur { cur } else { 0 }, true);
                    cur = 0;
                    has_cur = false;
                }
                b':' => {
                    self.params.push_raw(if has_cur { cur } else { 0 }, false);
                    cur = 0;
                    has_cur = false;
                }
                0x40..=0x7E => {
                    if has_cur || !self.params.is_empty() {
                        self.params.push_raw(if has_cur { cur } else { 0 }, true);
                    }
                    let inter = self.intermediates;
                    let ilen = self.inter_len;
                    sink.csi_dispatch(&self.params, &inter[..ilen], b);
                    return Some(idx + 1);
                }
                0x20..=0x2F => {
                    if self.inter_len < self.intermediates.len() {
                        self.intermediates[self.inter_len] = b;
                        self.inter_len += 1;
                    }
                }
                _ => return None,
            }
            idx += 1;
        }
        None
    }

    /// Scan contiguous valid multi-byte UTF-8 printable text (bytes >= 0x80 or printable ASCII)
    /// stopping at any control byte (< 0x20, 0x7F) or invalid/truncated UTF-8.
    #[inline]
    fn scan_valid_utf8_printable(bytes: &[u8]) -> usize {
        let len = bytes.len();
        let mut i = 0;
        while i < len {
            let b = bytes[i];
            if b < 0x80 {
                if (0x20..=0x7E).contains(&b) {
                    i += 1;
                } else {
                    break;
                }
            } else if (0xC2..=0xDF).contains(&b) {
                if i + 1 < len && (bytes[i + 1] & 0xC0) == 0x80 {
                    i += 2;
                } else {
                    break;
                }
            } else if (0xE0..=0xEF).contains(&b) {
                if i + 2 < len
                    && (bytes[i + 1] & 0xC0) == 0x80
                    && (bytes[i + 2] & 0xC0) == 0x80
                    && !(b == 0xE0 && bytes[i + 1] < 0xA0)
                    && !(b == 0xED && bytes[i + 1] >= 0xA0)
                {
                    i += 3;
                } else {
                    break;
                }
            } else if (0xF0..=0xF4).contains(&b) {
                if i + 3 < len
                    && (bytes[i + 1] & 0xC0) == 0x80
                    && (bytes[i + 2] & 0xC0) == 0x80
                    && (bytes[i + 3] & 0xC0) == 0x80
                    && !(b == 0xF0 && bytes[i + 1] < 0x90)
                    && !(b == 0xF4 && bytes[i + 1] >= 0x90)
                {
                    i += 4;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        i
    }

    /// Feed a chunk of PTY bytes into the parser, driving `sink`.
    pub fn advance<S: VtSink>(&mut self, bytes: &[u8], sink: &mut S) {
        let mut i = 0;
        let len = bytes.len();

        while i < len {
            if self.state == State::Ground && self.utf8_expected == 0 {
                // Bulk plain-ASCII line bypass: archives lines guaranteed to scroll off-screen
                // directly at 1 byte/char without 16x Cell expansion!
                if len - i >= 2048 {
                    let consumed = sink.try_bulk_ascii_lines(&bytes[i..]);
                    if consumed > 0 {
                        i += consumed;
                        if i >= len {
                            return;
                        }
                    }
                }
                // Tight Ground fast loop: handles ASCII runs, \r/\n/\t/\x08, inline CSI, and bulk UTF-8!
                loop {
                    if i >= len {
                        return;
                    }
                    let b = bytes[i];
                    if (0x20..=0x7E).contains(&b) {
                        let ascii_len = Self::scan_printable_ascii(&bytes[i..]);
                        sink.print_ascii_run(&bytes[i..i + ascii_len]);
                        i += ascii_len;
                    } else if b == 0x1B {
                        if i + 1 < len && bytes[i + 1] == b'[' {
                            if let Some(consumed) = self.try_fast_csi(&bytes[i + 2..], sink) {
                                i += 2 + consumed;
                                continue;
                            }
                        }
                        break;
                    } else if b == b'\r' || b == b'\n' || b == b'\t' || b == 0x08 {
                        sink.execute(b);
                        i += 1;
                        // Re-enter bulk plain-ASCII line fast path right after cursor hits bottom col 0!
                        if b == b'\n' && len - i >= 2048 {
                            let consumed = sink.try_bulk_ascii_lines(&bytes[i..]);
                            if consumed > 0 {
                                i += consumed;
                            }
                        }
                    } else if b >= 0xC2 {
                        let ulen = Self::scan_valid_utf8_printable(&bytes[i..]);
                        if ulen > 0 {
                            // Safety: scan_valid_utf8_printable validated UTF-8 bounds & overlong/surrogate rules!
                            let s = unsafe { std::str::from_utf8_unchecked(&bytes[i..i + ulen]) };
                            sink.print_utf8_run(s);
                            i += ulen;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }

            // Fast SIMD memchr3 bulk scan inside OSC string until BEL (0x07), ESC (0x1B), or CAN/SUB
            if self.state == State::OscString {
                let start = i;
                if let Some(pos) = memchr::memchr3(0x07, 0x1B, 0x18, &bytes[i..]) {
                    i += pos;
                } else {
                    i = len;
                }
                // Check rare SUB (0x1A) inside slice only if present
                if let Some(sub_pos) = memchr::memchr(0x1A, &bytes[start..i]) {
                    i = start + sub_pos;
                }
                if i > start {
                    let avail = MAX_STRING_PAYLOAD.saturating_sub(self.osc_buf.len());
                    let take = (i - start).min(avail);
                    self.osc_buf.extend_from_slice(&bytes[start..start + take]);
                }
                if i >= len {
                    break;
                }
            }

            // Fast SIMD memchr3 bulk scan inside DCS passthrough until ESC (0x1B) or CAN/SUB
            if self.state == State::DcsPassthrough {
                let start = i;
                if let Some(pos) = memchr::memchr3(0x1B, 0x18, 0x1A, &bytes[i..]) {
                    i += pos;
                } else {
                    i = len;
                }
                if i > start {
                    let avail = MAX_STRING_PAYLOAD.saturating_sub(self.dcs_buf.len());
                    let take = (i - start).min(avail);
                    self.dcs_buf.extend_from_slice(&bytes[start..start + take]);
                }
                if i >= len {
                    break;
                }
            }

            let b = bytes[i];
            i += 1;
            self.step(b, sink);
        }
    }

    #[inline]
    fn step<S: VtSink>(&mut self, b: u8, sink: &mut S) {
        // Handle in-progress multi-byte UTF-8 sequence in Ground state
        if self.state == State::Ground && self.utf8_expected > 0 {
            if (b & 0xC0) == 0x80 {
                self.utf8_buf[self.utf8_len as usize] = b;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_expected {
                    let slice = &self.utf8_buf[..self.utf8_len as usize];
                    if let Ok(s) = std::str::from_utf8(slice) {
                        if let Some(ch) = s.chars().next() {
                            sink.print_char(ch);
                        }
                    } else {
                        sink.print_char('\u{FFFD}');
                    }
                    self.utf8_len = 0;
                    self.utf8_expected = 0;
                }
                return;
            } else {
                // Broken UTF-8 sequence: emit replacement and reprocess current byte
                sink.print_char('\u{FFFD}');
                self.utf8_len = 0;
                self.utf8_expected = 0;
            }
        }

        // Global transitions: CAN / SUB abort any escape sequence immediately
        if b == 0x18 || b == 0x1A {
            self.state = State::Ground;
            self.osc_buf.clear();
            self.dcs_buf.clear();
            return;
        }

        match self.state {
            State::Ground => match b {
                0x20..=0x7E => sink.print_ascii_run(&[b]),
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(b),
                0x7F => {} // DEL ignored
                0xC2..=0xDF => {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_expected = 2;
                }
                0xE0..=0xEF => {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_expected = 3;
                }
                0xF0..=0xF4 => {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_expected = 4;
                }
                _ => sink.print_char('\u{FFFD}'),
            },

            State::Escape => match b {
                b'[' => {
                    self.params.clear();
                    self.inter_len = 0;
                    self.state = State::CsiEntry;
                }
                b']' => {
                    self.osc_buf.clear();
                    self.state = State::OscString;
                }
                b'P' => {
                    self.params.clear();
                    self.inter_len = 0;
                    self.dcs_buf.clear();
                    self.dcs_final = 0;
                    self.state = State::DcsEntry;
                }
                b'_' | b'^' | b'X' => {
                    self.state = State::IgnoreString;
                }
                0x20..=0x2F => {
                    self.push_intermediate(b);
                    self.state = State::EscapeIntermediate;
                }
                0x30..=0x7E => {
                    let inter = self.intermediates;
                    let ilen = self.inter_len;
                    self.state = State::Ground;
                    sink.esc_dispatch(&inter[..ilen], b);
                }
                0x1B => {
                    self.inter_len = 0;
                }
                0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(b),
                _ => self.state = State::Ground,
            },

            State::EscapeIntermediate => match b {
                0x20..=0x2F => self.push_intermediate(b),
                0x30..=0x7E => {
                    let inter = self.intermediates;
                    let ilen = self.inter_len;
                    self.state = State::Ground;
                    sink.esc_dispatch(&inter[..ilen], b);
                }
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(b),
                _ => self.state = State::Ground,
            },

            State::CsiEntry => match b {
                b'0'..=b'9' => {
                    self.params.push_digit(b);
                    self.state = State::CsiParam;
                }
                b';' => {
                    self.params.push_param_sep();
                    self.state = State::CsiParam;
                }
                b':' => {
                    self.params.push_subparam_sep();
                    self.state = State::CsiParam;
                }
                b'<'..=b'?' | 0x20..=0x2F => {
                    self.push_intermediate(b);
                    self.state = State::CsiParam;
                }
                0x40..=0x7E => {
                    self.params.finish();
                    let inter = self.intermediates;
                    let ilen = self.inter_len;
                    self.state = State::Ground;
                    sink.csi_dispatch(&self.params, &inter[..ilen], b);
                }
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(b),
                _ => self.state = State::CsiIgnore,
            },

            State::CsiParam => match b {
                b'0'..=b'9' => self.params.push_digit(b),
                b';' => self.params.push_param_sep(),
                b':' => self.params.push_subparam_sep(),
                0x20..=0x2F => self.push_intermediate(b),
                0x40..=0x7E => {
                    self.params.finish();
                    let inter = self.intermediates;
                    let ilen = self.inter_len;
                    self.state = State::Ground;
                    sink.csi_dispatch(&self.params, &inter[..ilen], b);
                }
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(b),
                _ => self.state = State::CsiIgnore,
            },

            State::CsiIgnore => match b {
                0x40..=0x7E => self.state = State::Ground,
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(b),
                _ => {}
            },

            State::OscString => match b {
                0x07 => {
                    self.state = State::Ground;
                    let payload = std::mem::take(&mut self.osc_buf);
                    sink.osc_dispatch(&payload);
                    self.osc_buf = payload;
                    self.osc_buf.clear();
                }
                0x1B => self.state = State::OscEsc,
                _ => {
                    if self.osc_buf.len() < MAX_STRING_PAYLOAD {
                        self.osc_buf.push(b);
                    }
                }
            },

            State::OscEsc => {
                if b == b'\\' {
                    self.state = State::Ground;
                    let payload = std::mem::take(&mut self.osc_buf);
                    sink.osc_dispatch(&payload);
                    self.osc_buf = payload;
                    self.osc_buf.clear();
                } else {
                    self.state = State::Escape;
                    self.inter_len = 0;
                    self.step(b, sink);
                }
            }

            State::DcsEntry => match b {
                b'0'..=b'9' => {
                    self.params.push_digit(b);
                    self.state = State::DcsParam;
                }
                b';' => {
                    self.params.push_param_sep();
                    self.state = State::DcsParam;
                }
                0x20..=0x2F | b'<'..=b'?' => {
                    self.push_intermediate(b);
                    self.state = State::DcsParam;
                }
                0x40..=0x7E => {
                    self.params.finish();
                    self.dcs_final = b;
                    self.state = State::DcsPassthrough;
                }
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                _ => self.state = State::IgnoreString,
            },

            State::DcsParam => match b {
                b'0'..=b'9' => self.params.push_digit(b),
                b';' => self.params.push_param_sep(),
                0x20..=0x2F => self.push_intermediate(b),
                0x40..=0x7E => {
                    self.params.finish();
                    self.dcs_final = b;
                    self.state = State::DcsPassthrough;
                }
                0x1B => {
                    self.inter_len = 0;
                    self.state = State::Escape;
                }
                _ => self.state = State::IgnoreString,
            },

            State::DcsPassthrough => {
                if b == 0x1B {
                    self.state = State::DcsEsc;
                } else if self.dcs_buf.len() < MAX_STRING_PAYLOAD {
                    self.dcs_buf.push(b);
                }
            }

            State::DcsEsc => {
                if b == b'\\' {
                    // ST terminates DCS!
                    self.state = State::Ground;
                    let data = std::mem::take(&mut self.dcs_buf);
                    let final_b = self.dcs_final;
                    let inter = self.intermediates;
                    let ilen = self.inter_len;
                    self.dispatch_dcs(&inter[..ilen], final_b, &data, sink);
                    self.dcs_buf = data;
                    self.dcs_buf.clear();
                } else if b == 0x1B && self.dcs_final == b't' && self.dcs_buf.starts_with(b"mux;") {
                    // tmux doubles ESC bytes inside DCS tmux;...ST! Preserve the pair so unwrapper decodes it.
                    if self.dcs_buf.len() + 2 <= MAX_STRING_PAYLOAD {
                        self.dcs_buf.push(0x1B);
                        self.dcs_buf.push(0x1B);
                    }
                    self.state = State::DcsPassthrough;
                } else {
                    // Single ESC inside DCS payload (not ST)
                    if self.dcs_buf.len() + 2 <= MAX_STRING_PAYLOAD {
                        self.dcs_buf.push(0x1B);
                        self.dcs_buf.push(b);
                    }
                    self.state = State::DcsPassthrough;
                }
            }

            State::IgnoreString => {
                if b == 0x1B {
                    self.state = State::IgnoreStringEsc;
                }
            }

            State::IgnoreStringEsc => {
                if b == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::IgnoreString;
                }
            }
        }
    }

    fn dispatch_dcs<S: VtSink>(
        &mut self,
        intermediates: &[u8],
        final_byte: u8,
        data: &[u8],
        sink: &mut S,
    ) {
        // Native tmux passthrough unwrapper: `ESC P tmux; <doubled-ESC payload> ESC \`
        // Because `t` is the DCS final byte in `ESC P tmux;...`, `data` begins with `mux;`.
        if intermediates.is_empty()
            && self.params.is_empty()
            && final_byte == b't'
            && data.starts_with(b"mux;")
            && self.passthrough_depth < 4
        {
            let inner = &data[4..];
            let mut unescaped = Vec::with_capacity(inner.len());
            let mut j = 0;
            while j < inner.len() {
                if inner[j] == 0x1B && j + 1 < inner.len() && inner[j + 1] == 0x1B {
                    unescaped.push(0x1B);
                    j += 2;
                } else {
                    unescaped.push(inner[j]);
                    j += 1;
                }
            }
            let mut sub = VtParser::new();
            sub.passthrough_depth = self.passthrough_depth + 1;
            sub.advance(&unescaped, sink);
            return;
        }

        sink.dcs_dispatch(&self.params, intermediates, final_byte, data);
    }
}
