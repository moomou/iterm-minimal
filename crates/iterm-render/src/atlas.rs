use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Font face style bitmask used as part of glyph and shaped-run cache keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontStyle {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

/// Unique key identifying a rasterized glyph in the GPU texture atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// CoreText font pointer hash or fallback font slot ID (`0` = primary font, `1+` = fallback).
    pub font_id: u32,
    pub style: FontStyle,
    /// Platform glyph ID (CoreText `CGGlyph`, or Unicode codepoint in headless/mock tests).
    pub glyph_id: u32,
    /// Quantized font size in quarter-points (`(pt * 4.0) as u16`).
    pub size_qpt: u16,
}

/// Location & bearing metrics of a glyph packed into the GPU atlas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasEntry {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// Horizontal offset from cell origin in pixels.
    pub bearing_x: f32,
    /// Vertical offset from cell baseline in pixels (positive = above baseline).
    pub bearing_y: f32,
    /// True for full-color glyphs (Apple Color Emoji).
    pub is_color: bool,
    /// Normalized atlas UVs `[u0, v0, u1, v1]`.
    pub uv_rect: [f32; 4],
}

/// Newly rasterized glyph rectangle pending upload to Metal (`R8Unorm` mono or `RGBA8Unorm` color atlas).
#[derive(Debug, Clone)]
pub struct AtlasUpload {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub is_color: bool,
    /// `width * height` bytes when `!is_color` (R8), or `width * height * 4` bytes when `is_color` (RGBA8).
    pub pixels: Vec<u8>,
}

/// Rasterized glyph returned by a platform rasterizer (CoreText on macOS, mock on Linux tests).
#[derive(Debug, Clone)]
pub struct RasterizedGlyph {
    pub width: u16,
    pub height: u16,
    pub bearing_x: f32,
    pub bearing_y: f32,
    pub is_color: bool,
    /// Single-channel R8 coverage (`width * height` B) if `!is_color`, or RGBA8 (`width * height * 4` B) if `is_color`.
    pub pixels: Vec<u8>,
}

struct Shelf {
    y: u32,
    height: u32,
    next_x: u32,
}

struct AtlasPlane {
    width: u32,
    height: u32,
    shelves: Vec<Shelf>,
    next_y: u32,
    epoch: u64,
}

impl AtlasPlane {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            shelves: Vec::with_capacity(128),
            next_y: 1,
            epoch: 1,
        }
    }

    fn reset(&mut self) {
        self.shelves.clear();
        self.next_y = 1;
        self.epoch += 1;
    }

    fn allocate_rect(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let pad = 1u32;
        let pw = w + pad;
        let ph = h + pad;

        for shelf in &mut self.shelves {
            if ph <= shelf.height && shelf.height <= ph + 12 && shelf.next_x + pw <= self.width {
                let pos = (shelf.next_x, shelf.y);
                shelf.next_x += pw;
                return Some(pos);
            }
        }
        if self.next_y + ph <= self.height && pw <= self.width {
            let y = self.next_y;
            self.next_y += ph;
            self.shelves.push(Shelf {
                y,
                height: ph,
                next_x: 1 + pw,
            });
            return Some((1, y));
        }
        None
    }
}

struct AtlasInner {
    mono: AtlasPlane,
    color: AtlasPlane,
    entries: FxHashMap<GlyphKey, AtlasEntry>,
    pending_uploads: Vec<AtlasUpload>,
}

impl AtlasInner {
    fn new(mono_w: u32, mono_h: u32, color_w: u32, color_h: u32) -> Self {
        Self {
            mono: AtlasPlane::new(mono_w, mono_h),
            color: AtlasPlane::new(color_w, color_h),
            entries: FxHashMap::default(),
            pending_uploads: Vec::new(),
        }
    }

    fn reset_all(&mut self) {
        self.mono.reset();
        self.color.reset();
        self.entries.clear();
        self.pending_uploads.clear();
    }

    fn reset_plane(&mut self, is_color: bool) {
        if is_color {
            self.color.reset();
        } else {
            self.mono.reset();
        }
        self.entries.retain(|_, v| v.is_color != is_color);
        self.pending_uploads.retain(|u| u.is_color != is_color);
    }
}

/// Thread-safe Dual-Plane GPU glyph atlas:
/// - **Monochrome R8 plane** (default `4096x4096`, 1 B/px = 16 MiB, ~12,000+ Retina glyphs!)
/// - **Color RGBA8 plane** (default `1024x1024`, 4 B/px = 4 MiB for Apple Color Emoji)
pub struct GlyphAtlas {
    inner: Mutex<AtlasInner>,
    /// Lock-free combined `(mono_epoch << 32) | color_epoch` for 0-cost hot-path cache validation!
    cached_epoch: AtomicU64,
}

#[inline(always)]
fn pack_epochs(mono: u64, color: u64) -> u64 {
    (mono << 32) | (color & 0xFFFF_FFFF)
}

impl GlyphAtlas {
    pub fn new(mono_w: u32, mono_h: u32) -> Self {
        Self::new_dual(mono_w, mono_h, 1024, 1024)
    }

    pub fn new_dual(mono_w: u32, mono_h: u32, color_w: u32, color_h: u32) -> Self {
        let inner = AtlasInner::new(
            mono_w.max(512),
            mono_h.max(512),
            color_w.max(256),
            color_h.max(256),
        );
        let ep = pack_epochs(inner.mono.epoch, inner.color.epoch);
        Self {
            inner: Mutex::new(inner),
            cached_epoch: AtomicU64::new(ep),
        }
    }

    pub fn mono_dimensions(&self) -> (u32, u32) {
        let g = self.inner.lock();
        (g.mono.width, g.mono.height)
    }

    pub fn color_dimensions(&self) -> (u32, u32) {
        let g = self.inner.lock();
        (g.color.width, g.color.height)
    }

    /// Combined monotonic epoch (lock-free atomic load; changes whenever either mono or color plane resets).
    #[inline(always)]
    pub fn epoch(&self) -> u64 {
        self.cached_epoch.load(Ordering::Acquire)
    }

    pub fn entry_count(&self) -> usize {
        self.inner.lock().entries.len()
    }

    /// Look up `key` or invoke `rasterize()` on miss, packing the bitmap into the mono R8 or color RGBA8 plane.
    pub fn get_or_insert<F>(&self, key: GlyphKey, rasterize: F) -> Option<AtlasEntry>
    where
        F: FnOnce() -> Option<RasterizedGlyph>,
    {
        {
            let guard = self.inner.lock();
            if let Some(&entry) = guard.entries.get(&key) {
                return Some(entry);
            }
        }

        // Rasterize OUTSIDE the atlas lock so multiple threads can rasterize concurrently!
        let rast = rasterize()?;
        let w = rast.width as u32;
        let h = rast.height as u32;

        let mut guard = self.inner.lock();
        if let Some(&entry) = guard.entries.get(&key) {
            return Some(entry);
        }

        if w == 0 || h == 0 {
            let empty = AtlasEntry {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                bearing_x: 0.0,
                bearing_y: 0.0,
                is_color: rast.is_color,
                uv_rect: [0.0; 4],
            };
            guard.entries.insert(key, empty);
            return Some(empty);
        }

        let pos = {
            let plane = if rast.is_color { &mut guard.color } else { &mut guard.mono };
            plane.allocate_rect(w, h)
        };
        let pos = match pos {
            Some(p) => p,
            None => {
                // Only clear the full plane (mono OR color), leaving the other plane untouched!
                guard.reset_plane(rast.is_color);
                self.cached_epoch.store(
                    pack_epochs(guard.mono.epoch, guard.color.epoch),
                    Ordering::Release,
                );
                let plane = if rast.is_color { &mut guard.color } else { &mut guard.mono };
                plane.allocate_rect(w, h)?
            }
        };

        let (pw, ph) = if rast.is_color {
            (guard.color.width, guard.color.height)
        } else {
            (guard.mono.width, guard.mono.height)
        };
        let inv_w = 1.0 / pw as f32;
        let inv_h = 1.0 / ph as f32;
        let entry = AtlasEntry {
            x: pos.0 as u16,
            y: pos.1 as u16,
            width: rast.width,
            height: rast.height,
            bearing_x: rast.bearing_x,
            bearing_y: rast.bearing_y,
            is_color: rast.is_color,
            uv_rect: [
                pos.0 as f32 * inv_w,
                pos.1 as f32 * inv_h,
                (pos.0 + w) as f32 * inv_w,
                (pos.1 + h) as f32 * inv_h,
            ],
        };
        guard.entries.insert(key, entry);
        guard.pending_uploads.push(AtlasUpload {
            x: pos.0,
            y: pos.1,
            width: w,
            height: h,
            is_color: rast.is_color,
            pixels: rast.pixels,
        });
        Some(entry)
    }

    /// Drain pending incremental atlas uploads along with `(mono_epoch, color_epoch)`.
    pub fn drain_uploads(&self) -> (u64, u64, Vec<AtlasUpload>) {
        let mut guard = self.inner.lock();
        (
            guard.mono.epoch,
            guard.color.epoch,
            std::mem::take(&mut guard.pending_uploads),
        )
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock();
        g.reset_all();
        self.cached_epoch
            .store(pack_epochs(g.mono.epoch, g.color.epoch), Ordering::Release);
    }
}
