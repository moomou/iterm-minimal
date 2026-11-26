use crate::atlas::{AtlasEntry, FontStyle, GlyphAtlas, GlyphKey, RasterizedGlyph};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Font metrics in physical pixels (already scaled by backing scale factor).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub cell_width: f32,
    pub cell_height: f32,
    pub ascent: f32,
    pub descent: f32,
    pub baseline_y: f32,
    pub underline_offset: f32,
    pub underline_thickness: f32,
    pub strikeout_offset: f32,
    pub scale_factor: f32,
    pub size_qpt: u16,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            cell_width: 12.0,
            cell_height: 24.0,
            ascent: 18.0,
            descent: 6.0,
            baseline_y: 18.0,
            underline_offset: 20.0,
            underline_thickness: 1.5,
            strikeout_offset: 11.0,
            scale_factor: 2.0,
            size_qpt: 64,
        }
    }
}

/// One shaped glyph emitted by CoreText (supports multi-cell FiraCode ligatures & fallback fonts!).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// Column index relative to the start of the shaped run.
    pub cluster_col: u16,
    /// Platform font ID (`0` = primary font, `1+` = fallback / Non-ASCII font).
    pub font_id: u32,
    /// Platform glyph ID (`CGGlyph`).
    pub glyph_id: u32,
    /// Subpixel offset from cell origin in pixels.
    pub x_offset: f32,
    pub y_offset: f32,
}

/// Platform text shaper & glyph rasterizer interface.
/// Implemented by `CoreTextShaper` on macOS and `MockShaper` for Linux unit/perf tests.
pub trait TextShaper: Send + Sync {
    fn metrics(&self) -> CellMetrics;

    /// Fast O(1) lookup for a printable ASCII character (`0x20..=0x7E`) when ligatures are not active.
    fn ascii_glyph(&self, style: FontStyle, ascii_byte: u8) -> Option<(u32, u32)>;

    /// Shape a contiguous run of UTF-8 text (`run_cols` cells wide) with optional ligatures.
    fn shape_run(&self, text: &str, style: FontStyle, ligatures: bool) -> Vec<ShapedGlyph>;

    /// Rasterize a single glyph ID into an RGBA8 bitmap for packing into `GlyphAtlas`.
    fn rasterize_glyph(&self, key: GlyphKey) -> Option<RasterizedGlyph>;
}

/// Cache key for a shaped text run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShapedRunKey {
    style: FontStyle,
    ligatures: bool,
    size_qpt: u16,
    text: String,
}

#[inline]
fn hash_run_key(style: FontStyle, ligatures: bool, size_qpt: u16, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    style.hash(&mut h);
    ligatures.hash(&mut h);
    size_qpt.hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

const MAX_SHAPED_RUN_CACHE: usize = 16_384;

struct CachedRunEntry {
    key: ShapedRunKey,
    shaped: Arc<[ShapedGlyph]>,
    last_used: u64,
}

/// Collision-safe generational-LRU wrapper over any `TextShaper`, accelerating ligature-heavy frames
/// without full-cache-wipe latency spikes!
pub struct CachedShaper<S: TextShaper> {
    pub inner: S,
    /// Bucketed by 64-bit hash for zero-allocation hit lookup, with inline chaining on rare collisions.
    run_cache: FxHashMap<u64, Vec<CachedRunEntry>>,
    /// Lock-free L1-cache-resident ASCII `AtlasEntry` lookup table (4 styles * 95 printable ASCII).
    /// Eliminates 20,000 mutex locks + 20,000 HashMap lookups per 250x80 frame!
    ascii_entries: [Option<AtlasEntry>; 4 * 95],
    ascii_atlas_epoch: u64,
    entry_count: usize,
    tick: u64,
    pub hits: u64,
    pub misses: u64,
}

impl<S: TextShaper> CachedShaper<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            run_cache: FxHashMap::with_capacity_and_hasher(4096, Default::default()),
            ascii_entries: [None; 4 * 95],
            ascii_atlas_epoch: 0,
            entry_count: 0,
            tick: 0,
            hits: 0,
            misses: 0,
        }
    }

    pub fn clear_cache(&mut self) {
        self.run_cache.clear();
        self.ascii_entries.fill(None);
        self.ascii_atlas_epoch = 0;
        self.entry_count = 0;
    }

    /// Sync the per-frame atlas epoch ONCE per pane so `resolve_ascii_entry_synced` does ZERO atomics!
    #[inline(always)]
    pub fn sync_atlas_epoch(&mut self, epoch: u64) {
        if epoch != self.ascii_atlas_epoch {
            self.ascii_entries.fill(None);
            self.ascii_atlas_epoch = epoch;
        }
    }

    /// O(1) lock-free, atomic-free ASCII `AtlasEntry` lookup (single L1 array load on hit!).
    #[inline(always)]
    pub fn resolve_ascii_entry(
        &mut self,
        atlas: &GlyphAtlas,
        style: FontStyle,
        ascii_byte: u8,
        size_qpt: u16,
    ) -> Option<AtlasEntry> {
        if !(0x21..=0x7E).contains(&ascii_byte) {
            return None;
        }
        let slot = (style as usize) * 95 + (ascii_byte - 0x20) as usize;
        if let Some(entry) = self.ascii_entries[slot] {
            return Some(entry);
        }
        let (font_id, glyph_id) = self.inner.ascii_glyph(style, ascii_byte)?;
        let key = GlyphKey {
            font_id,
            style,
            glyph_id,
            size_qpt,
        };
        let entry = atlas.get_or_insert(key, || self.inner.rasterize_glyph(key))?;
        let post_ep = atlas.epoch();
        if post_ep != self.ascii_atlas_epoch {
            self.ascii_entries.fill(None);
            self.ascii_atlas_epoch = post_ep;
        }
        self.ascii_entries[slot] = Some(entry);
        Some(entry)
    }

    #[inline]
    pub fn cache_len(&self) -> usize {
        self.entry_count
    }

    #[inline]
    pub fn metrics(&self) -> CellMetrics {
        self.inner.metrics()
    }

    /// Evict the coldest ~25% of shaped runs incrementally instead of wiping all 16k entries at once.
    fn evict_cold_quarter(&mut self) {
        if self.entry_count < MAX_SHAPED_RUN_CACHE {
            return;
        }
        let mut ages: Vec<u64> = Vec::with_capacity(self.entry_count);
        for bucket in self.run_cache.values() {
            for e in bucket {
                ages.push(e.last_used);
            }
        }
        let drop_idx = ages.len() / 4;
        let (_, &mut cutoff, _) = ages.select_nth_unstable(drop_idx);
        let mut kept = 0usize;
        self.run_cache.retain(|_, bucket| {
            bucket.retain(|e| e.last_used > cutoff);
            kept += bucket.len();
            !bucket.is_empty()
        });
        self.entry_count = kept;
        // Fallback if all timestamps tied
        if self.entry_count >= MAX_SHAPED_RUN_CACHE {
            self.run_cache.clear();
            self.entry_count = 0;
        }
    }

    /// Shape `text` using the O(1) zero-allocation-on-hit collision-safe LRU shaped-run cache.
    pub fn shape_cached(
        &mut self,
        text: &str,
        style: FontStyle,
        ligatures: bool,
    ) -> Arc<[ShapedGlyph]> {
        self.tick = self.tick.wrapping_add(1);
        let now_tick = self.tick;
        let size_qpt = self.inner.metrics().size_qpt;
        let h = hash_run_key(style, ligatures, size_qpt, text);
        if let Some(bucket) = self.run_cache.get_mut(&h) {
            for entry in bucket.iter_mut() {
                if entry.key.style == style
                    && entry.key.ligatures == ligatures
                    && entry.key.size_qpt == size_qpt
                    && entry.key.text == text
                {
                    entry.last_used = now_tick;
                    self.hits += 1;
                    return Arc::clone(&entry.shaped);
                }
            }
        }
        self.misses += 1;
        let shaped: Arc<[ShapedGlyph]> = self.inner.shape_run(text, style, ligatures).into();
        if self.entry_count >= MAX_SHAPED_RUN_CACHE {
            self.evict_cold_quarter();
        }
        let key = ShapedRunKey {
            style,
            ligatures,
            size_qpt,
            text: text.to_string(),
        };
        self.run_cache.entry(h).or_default().push(CachedRunEntry {
            key,
            shaped: Arc::clone(&shaped),
            last_used: now_tick,
        });
        self.entry_count += 1;
        shaped
    }

    /// Resolve a glyph in `atlas`, rasterizing on miss.
    #[inline]
    pub fn resolve_glyph(&self, atlas: &GlyphAtlas, key: GlyphKey) -> Option<AtlasEntry> {
        atlas.get_or_insert(key, || self.inner.rasterize_glyph(key))
    }
}

/// Deterministic mock shaper for Linux unit tests & CPU frame-preparation benchmarks.
pub struct MockShaper {
    pub metrics: CellMetrics,
}

impl Default for MockShaper {
    fn default() -> Self {
        Self {
            metrics: CellMetrics::default(),
        }
    }
}

impl TextShaper for MockShaper {
    fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    #[inline]
    fn ascii_glyph(&self, _style: FontStyle, ascii_byte: u8) -> Option<(u32, u32)> {
        if (0x21..=0x7E).contains(&ascii_byte) {
            Some((0, ascii_byte as u32))
        } else {
            None
        }
    }

    fn shape_run(&self, text: &str, _style: FontStyle, ligatures: bool) -> Vec<ShapedGlyph> {
        let mut out = Vec::with_capacity(text.len());
        let chars: Vec<char> = text.chars().collect();
        let mut col = 0usize;
        let mut i = 0usize;
        while i < chars.len() {
            let ch = chars[i];
            if ligatures && i + 1 < chars.len() {
                let pair = (ch, chars[i + 1]);
                let lig_id = match pair {
                    ('-', '>') => Some(0xE001),
                    ('=', '>') => Some(0xE002),
                    ('!', '=') => Some(0xE003),
                    ('=', '=') => Some(0xE004),
                    ('<', '=') => Some(0xE005),
                    ('>', '=') => Some(0xE006),
                    _ => None,
                };
                if let Some(gid) = lig_id {
                    out.push(ShapedGlyph {
                        cluster_col: col as u16,
                        font_id: 0,
                        glyph_id: gid,
                        x_offset: 0.0,
                        y_offset: 0.0,
                    });
                    col += 2;
                    i += 2;
                    continue;
                }
            }
            if ch != ' ' {
                out.push(ShapedGlyph {
                    cluster_col: col as u16,
                    font_id: if ch.is_ascii() { 0 } else { 1 },
                    glyph_id: ch as u32,
                    x_offset: 0.0,
                    y_offset: 0.0,
                });
            }
            col += 1;
            i += 1;
        }
        out
    }

    fn rasterize_glyph(&self, _key: GlyphKey) -> Option<RasterizedGlyph> {
        let w = 10u16;
        let h = 18u16;
        Some(RasterizedGlyph {
            width: w,
            height: h,
            bearing_x: 1.0,
            bearing_y: 14.0,
            is_color: false,
            pixels: vec![255u8; (w as usize) * (h as usize)],
        })
    }
}
