//! Native macOS CoreText text shaper & CoreGraphics glyph rasterizer.
//!
//! Features:
//! - True OpenType ligatures via `CTLine` + `CTRun` with `kCFTypeDictionaryKeyCallBacks` and `CTRun::positions()`
//! - Drift-free monospace cell alignment (`x_offset = pos.x - cluster_col * native_advance`)
//! - Pre-cached O(1) ASCII glyph lookup table for non-ligature fast path
//! - Verified monospace fallback cascade (never silently drops to proportional Helvetica)
//! - Pure 8-bit grayscale `CGBitmapContext` rasterization for crisp Retina text (immune to transparent-bg
//!   LCD smoothing corruption) + full-color RGBA rasterization for Apple Color Emoji.

use iterm_profile::Profile;
use iterm_render::{CellMetrics, FontStyle, GlyphKey, RasterizedGlyph, ShapedGlyph, TextShaper};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFAttributedString,
    CFDictionary, CFIndex, CFNumber, CFRange, CFRetained, CFString, CGFloat, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapInfo, CGColorSpace, CGContext, CGGlyph, CGImageAlphaInfo,
};
use objc2_core_text::{
    kCTFontAttributeName, kCTLigatureAttributeName, CTFont, CTFontOrientation,
    CTFontSymbolicTraits, CTFontUIFontType, CTLine, CTRun,
};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::ptr::NonNull;

/// CoreText font set for all 4 styles plus dynamically discovered fallback fonts (CJK, Emoji, Non-ASCII).
pub struct CoreTextShaper {
    fonts: [CFRetained<CTFont>; 4],
    non_ascii_font: Option<CFRetained<CTFont>>,
    /// Extra fallback fonts discovered via `CTRun` font attribute (`font_id = 2 + idx`).
    fallback_fonts: Mutex<Vec<CFRetained<CTFont>>>,
    /// Pre-warmed ASCII glyph table (`0x20..=0x7E`) for each of the 4 font styles.
    ascii_table: [[u16; 95]; 4],
    /// Exact unrounded typographic advance of `'M'` in physical pixels (used to compute drift-free `x_offset`).
    native_advance: f32,
    metrics: CellMetrics,
}

// CoreText CTFont is immutable & thread-safe on macOS (Apple CoreText Programming Guide).
unsafe impl Send for CoreTextShaper {}
unsafe impl Sync for CoreTextShaper {}

impl CoreTextShaper {
    pub fn from_profile(profile: &Profile, scale_factor: f32) -> Self {
        let scale = scale_factor.max(1.0);
        let px_size = (profile.normal_font.size * scale) as CGFloat;

        let regular = resolve_monospace_font(&profile.normal_font.name, px_size);
        let bold = derive_trait_font(&regular, CTFontSymbolicTraits::TraitBold);
        let italic = derive_trait_font(&regular, CTFontSymbolicTraits::TraitItalic);
        let bold_italic = derive_trait_font(
            &regular,
            CTFontSymbolicTraits::TraitBold | CTFontSymbolicTraits::TraitItalic,
        );

        let non_ascii_font = if profile.use_non_ascii_font {
            let na_size = (profile.non_ascii_font.size * scale) as CGFloat;
            try_create_exact_font(&profile.non_ascii_font.name, na_size)
                .or_else(|| try_create_exact_font("Monaco", na_size))
        } else {
            None
        };

        let fonts = [regular, bold, italic, bold_italic];

        // Compute exact cell geometry from regular font
        let (native_adv, cell_w, cell_h, ascent, descent, ul_pos, ul_thick) = unsafe {
            let asc = fonts[0].ascent() as f32;
            let desc = fonts[0].descent() as f32;
            let lead = fonts[0].leading() as f32;
            let ul_p = fonts[0].underline_position() as f32;
            let ul_t = (fonts[0].underline_thickness() as f32).max(scale);

            let sample_char: u16 = b'M' as u16;
            let mut m_glyph: CGGlyph = 0;
            let mut adv = CGSize {
                width: px_size * 0.6,
                height: 0.0,
            };
            if fonts[0].glyphs_for_characters(
                NonNull::from(&sample_char),
                NonNull::from(&mut m_glyph),
                1,
            ) {
                let _ = fonts[0].advances_for_glyphs(
                    CTFontOrientation::Horizontal,
                    NonNull::from(&m_glyph),
                    &mut adv as *mut CGSize,
                    1,
                );
            }
            let nat = (adv.width as f32).max(4.0);
            let cw = (nat * profile.horizontal_spacing).ceil().max(4.0);
            let ch = ((asc + desc + lead.max(0.0)) * profile.vertical_spacing)
                .ceil()
                .max(8.0);
            let ps_name = fonts[0].post_script_name().to_string();
            log::info!(
                "CoreTextShaper resolved font={} (requested={}) px_size={:.1} cell={:.0}x{:.0} native_adv={:.2}",
                ps_name,
                profile.normal_font.name,
                px_size,
                cw,
                ch,
                nat
            );
            (nat, cw, ch, asc, desc, ul_p, ul_t)
        };

        // Pre-warm ASCII table for all 4 styles
        let mut ascii_table = [[0u16; 95]; 4];
        let chars_utf16: [u16; 95] = std::array::from_fn(|i| (0x20 + i) as u16);
        for (style_idx, font) in fonts.iter().enumerate() {
            let mut glyphs = [0 as CGGlyph; 95];
            unsafe {
                let _ = font.glyphs_for_characters(
                    NonNull::new_unchecked(chars_utf16.as_ptr() as *mut u16),
                    NonNull::new_unchecked(glyphs.as_mut_ptr()),
                    95,
                );
            }
            ascii_table[style_idx] = glyphs;
        }

        let baseline_y = ascent.ceil();
        let metrics = CellMetrics {
            cell_width: cell_w,
            cell_height: cell_h,
            ascent,
            descent,
            baseline_y,
            underline_offset: (baseline_y - ul_pos).clamp(baseline_y + 1.0, cell_h - ul_thick),
            underline_thickness: ul_thick,
            strikeout_offset: (baseline_y - ascent * 0.35).max(1.0),
            scale_factor: scale,
            size_qpt: (profile.normal_font.size * scale * 4.0).round() as u16,
        };

        Self {
            fonts,
            non_ascii_font,
            fallback_fonts: Mutex::new(Vec::new()),
            ascii_table,
            native_advance: native_adv,
            metrics,
        }
    }

    fn font_for_id(&self, font_id: u32, style: FontStyle) -> CFRetained<CTFont> {
        match font_id {
            0 => self.fonts[style as usize].clone(),
            1 => self
                .non_ascii_font
                .clone()
                .unwrap_or_else(|| self.fonts[style as usize].clone()),
            other => {
                let idx = (other - 2) as usize;
                let guard = self.fallback_fonts.lock();
                guard
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| self.fonts[style as usize].clone())
            }
        }
    }

    fn intern_fallback_font(&self, font: &CTFont, style: FontStyle) -> u32 {
        let primary_ptr = (&*self.fonts[style as usize] as *const CTFont) as usize;
        let ptr = (font as *const CTFont) as usize;
        if ptr == primary_ptr {
            return 0;
        }
        if let Some(ref na) = self.non_ascii_font {
            if ptr == (&**na as *const CTFont) as usize {
                return 1;
            }
        }
        let mut guard = self.fallback_fonts.lock();
        for (i, existing) in guard.iter().enumerate() {
            if ((&**existing as *const CTFont) as usize) == ptr {
                return (i as u32) + 2;
            }
        }
        let retained = unsafe { CFRetained::retain(NonNull::from(font)) };
        let id = (guard.len() as u32) + 2;
        guard.push(retained);
        id
    }
}

/// Normalize font names for fuzzy comparison (strip spaces/hyphens/case).
fn norm_font_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Attempt to load `name` via CoreText, returning `None` if macOS silently substituted Helvetica.
fn try_create_exact_font(name: &str, px_size: CGFloat) -> Option<CFRetained<CTFont>> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cf_name = CFString::from_str(trimmed);
    let font = unsafe { CTFont::with_name(&cf_name, px_size, std::ptr::null()) };
    let ps = unsafe { font.post_script_name().to_string() };
    let fam = unsafe { font.family_name().to_string() };
    let req_norm = norm_font_name(trimmed);
    let ps_norm = norm_font_name(&ps);
    let fam_norm = norm_font_name(&fam);

    // Accept if PostScript/family name matches requested name (or prefix, e.g. "FiraCode" -> "FiraCode-Retina")
    if ps_norm == req_norm
        || fam_norm == req_norm
        || ps_norm.starts_with(&req_norm)
        || req_norm.starts_with(&fam_norm)
    {
        return Some(font);
    }

    // Also accept if user asked for Helvetica explicitly
    if req_norm.contains("helvetica") {
        return Some(font);
    }

    // macOS silently fell back (typically to Helvetica) — reject so caller tries monospace fallbacks
    None
}

/// Resolve a guaranteed monospace CoreText font, never silently dropping to proportional Helvetica.
fn resolve_monospace_font(requested: &str, px_size: CGFloat) -> CFRetained<CTFont> {
    if let Some(f) = try_create_exact_font(requested, px_size) {
        return f;
    }
    // Try stripping "-Retina" / "-Regular" suffix to match family name (e.g. "FiraCode-Retina" -> "Fira Code")
    if let Some((base, _)) = requested.split_once('-') {
        if let Some(f) = try_create_exact_font(base, px_size) {
            return f;
        }
    }
    for candidate in [
        "Menlo-Regular",
        "SFMono-Regular",
        "Monaco",
        "JetBrainsMono-Regular",
        "Courier",
    ] {
        if let Some(f) = try_create_exact_font(candidate, px_size) {
            return f;
        }
    }
    unsafe {
        CTFont::new_ui_font_for_language(CTFontUIFontType::UserFixedPitch, px_size, None)
            .unwrap_or_else(|| {
                let cf = CFString::from_str("Menlo-Regular");
                CTFont::with_name(&cf, px_size, std::ptr::null())
            })
    }
}

fn derive_trait_font(base: &CTFont, traits: CTFontSymbolicTraits) -> CFRetained<CTFont> {
    unsafe {
        base.copy_with_symbolic_traits(0.0, std::ptr::null(), traits, traits)
            .unwrap_or_else(|| CFRetained::retain(NonNull::from(base)))
    }
}

impl TextShaper for CoreTextShaper {
    #[inline]
    fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    #[inline]
    fn ascii_glyph(&self, style: FontStyle, ascii_byte: u8) -> Option<(u32, u32)> {
        if !(0x21..=0x7E).contains(&ascii_byte) {
            return None;
        }
        let g = self.ascii_table[style as usize][(ascii_byte - 0x20) as usize];
        if g == 0 {
            None
        } else {
            Some((0, g as u32))
        }
    }

    fn shape_run(&self, text: &str, style: FontStyle, ligatures: bool) -> Vec<ShapedGlyph> {
        let mut out = Vec::with_capacity(text.len());
        if text.is_empty() {
            return out;
        }

        // Map UTF-16 code unit index -> character (cell) index inside `text`
        let mut utf16_to_char: Vec<u16> = Vec::with_capacity(text.len() + 2);
        for (char_idx, ch) in text.chars().enumerate() {
            let units = ch.len_utf16();
            for _ in 0..units {
                utf16_to_char.push(char_idx as u16);
            }
        }

        let is_ascii_run = text.is_ascii();
        let base_font: &CTFont = if !is_ascii_run {
            if let Some(ref na) = self.non_ascii_font {
                na
            } else {
                &self.fonts[style as usize]
            }
        } else {
            &self.fonts[style as usize]
        };

        unsafe {
            let cf_str = CFString::from_str(text);
            // CoreText ligatures: 2 = all available OpenType ligatures (calt/liga/dlig), 0 = none
            let lig_val: i32 = if ligatures { 2 } else { 0 };
            let cf_lig = CFNumber::new_i32(lig_val);

            let keys: [*const c_void; 2] = [
                (kCTFontAttributeName as *const CFString).cast(),
                (kCTLigatureAttributeName as *const CFString).cast(),
            ];
            let vals: [*const c_void; 2] = [
                (base_font as *const CTFont).cast(),
                (&*cf_lig as *const CFNumber).cast(),
            ];

            // CRITICAL: MUST pass kCFTypeDictionaryKeyCallBacks & ValueCallBacks so CoreText
            // finds kCTFontAttributeName by CFEqual/CFHash instead of raw pointer identity!
            let Some(attrs) = CFDictionary::new(
                None,
                keys.as_ptr() as *mut *const c_void,
                vals.as_ptr() as *mut *const c_void,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            ) else {
                return out;
            };
            let Some(attr_str) = CFAttributedString::new(None, Some(&cf_str), Some(&attrs)) else {
                return out;
            };
            let line = CTLine::with_attributed_string(&attr_str);
            let runs = line.glyph_runs();
            let run_count = runs.count();

            for r_idx in 0..run_count {
                let run_ptr = runs.value_at_index(r_idx);
                if run_ptr.is_null() {
                    continue;
                }
                let run: &CTRun = &*(run_ptr as *const CTRun);
                let gcount = run.glyph_count() as usize;
                if gcount == 0 {
                    continue;
                }

                // Determine actual font CoreText cascaded to for this run (handles Emoji / CJK / Nerd Fonts!)
                let run_attrs = run.attributes();
                let font_key: *const c_void = (kCTFontAttributeName as *const CFString).cast();
                let run_font_ptr = run_attrs.value(font_key);
                let font_id = if !run_font_ptr.is_null() {
                    let rf: &CTFont = &*(run_font_ptr as *const CTFont);
                    self.intern_fallback_font(rf, style)
                } else {
                    0
                };

                let mut glyphs = vec![0 as CGGlyph; gcount];
                let mut indices = vec![0 as CFIndex; gcount];
                let mut positions = vec![CGPoint { x: 0.0, y: 0.0 }; gcount];
                let full_range = CFRange {
                    location: 0,
                    length: gcount as CFIndex,
                };
                run.glyphs(full_range, NonNull::new_unchecked(glyphs.as_mut_ptr()));
                run.string_indices(full_range, NonNull::new_unchecked(indices.as_mut_ptr()));
                run.positions(full_range, NonNull::new_unchecked(positions.as_mut_ptr()));

                let mut last_cluster_col: Option<u16> = None;
                let mut cluster_base_x: f64 = 0.0;
                for i in 0..gcount {
                    let gid = glyphs[i];
                    if gid == 0 {
                        continue;
                    }
                    let u16_idx =
                        (indices[i].max(0) as usize).min(utf16_to_char.len().saturating_sub(1));
                    let cluster_col = utf16_to_char.get(u16_idx).copied().unwrap_or(0);
                    if last_cluster_col != Some(cluster_col) {
                        last_cluster_col = Some(cluster_col);
                        cluster_base_x = positions[i].x;
                    }

                    // Drift-free typographic offset: preserves exact CoreText ligature placement
                    // and combining-mark positioning within a cluster without drifting off the cell grid!
                    let (x_offset, y_offset) = if is_ascii_run && font_id == 0 {
                        let expected_x = cluster_col as f32 * self.native_advance;
                        let dx = (positions[i].x as f32 - expected_x)
                            .clamp(-self.metrics.cell_width * 4.0, self.metrics.cell_width * 4.0);
                        (dx, positions[i].y as f32)
                    } else {
                        let dx = ((positions[i].x - cluster_base_x) as f32)
                            .clamp(-self.metrics.cell_width * 2.0, self.metrics.cell_width * 2.0);
                        (dx, positions[i].y as f32)
                    };

                    out.push(ShapedGlyph {
                        cluster_col,
                        font_id,
                        glyph_id: gid as u32,
                        x_offset,
                        y_offset,
                    });
                }
            }
        }
        out
    }

    fn rasterize_glyph(&self, key: GlyphKey) -> Option<RasterizedGlyph> {
        let font = self.font_for_id(key.font_id, key.style);
        let glyph = key.glyph_id as CGGlyph;
        if glyph == 0 {
            return None;
        }

        unsafe {
            let mut bbox = CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: 0.0,
                    height: 0.0,
                },
            };
            let _ = font.bounding_rects_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::from(&glyph),
                &mut bbox as *mut CGRect,
                1,
            );

            // Empty/spacer glyph (e.g. ligature spacer in FiraCode) -> return 0x0 so it draws nothing
            if bbox.size.width <= 0.01 || bbox.size.height <= 0.01 {
                return Some(RasterizedGlyph {
                    width: 0,
                    height: 0,
                    bearing_x: 0.0,
                    bearing_y: 0.0,
                    is_color: false,
                    pixels: Vec::new(),
                });
            }

            let pad = 2i32;
            let min_x = bbox.origin.x.floor() as i32 - pad;
            let min_y = bbox.origin.y.floor() as i32 - pad;
            let max_x = (bbox.origin.x + bbox.size.width).ceil() as i32 + pad;
            let max_y = (bbox.origin.y + bbox.size.height).ceil() as i32 + pad;

            let width = (max_x - min_x).clamp(1, 512) as usize;
            let height = (max_y - min_y).clamp(1, 512) as usize;
            let traits = font.symbolic_traits();
            let is_color = traits.contains(CTFontSymbolicTraits::TraitColorGlyphs);

            let draw_pos = CGPoint {
                x: (-min_x) as CGFloat,
                y: (-min_y) as CGFloat,
            };

            if !is_color {
                // Pure 8-bit grayscale bitmap on opaque black backing:
                // 100% immune to macOS transparent-surface LCD font smoothing corruption,
                // razor-sharp Retina coverage, and 4x smaller bitmap memory during rasterization.
                let mut gray = vec![0u8; width * height];
                let color_space = CGColorSpace::new_device_gray()?;
                let ctx = CGBitmapContextCreate(
                    gray.as_mut_ptr().cast(),
                    width,
                    height,
                    8,
                    width,
                    Some(&color_space),
                    CGImageAlphaInfo::None.0,
                )?;

                CGContext::set_allows_antialiasing(Some(&ctx), true);
                CGContext::set_should_antialias(Some(&ctx), true);
                CGContext::set_allows_font_smoothing(Some(&ctx), false);
                CGContext::set_should_smooth_fonts(Some(&ctx), false);
                CGContext::set_gray_fill_color(Some(&ctx), 1.0, 1.0);

                font.draw_glyphs(NonNull::from(&glyph), NonNull::from(&draw_pos), 1, &ctx);

                // Pass raw 1-byte/px grayscale coverage straight into the 4096x4096 R8Unorm atlas!
                // Zero 4x RGBA expansion overhead and 4x smaller GPU uploads!
                Some(RasterizedGlyph {
                    width: width as u16,
                    height: height as u16,
                    bearing_x: min_x as f32,
                    bearing_y: max_y as f32,
                    is_color: false,
                    pixels: gray,
                })
            } else {
                // Full-color bitmap glyph (Apple Color Emoji)
                let bytes_per_row = width * 4;
                let mut buf = vec![0u8; bytes_per_row * height];
                let color_space = CGColorSpace::new_device_rgb()?;
                #[allow(deprecated)]
                let bitmap_info: u32 =
                    CGImageAlphaInfo::PremultipliedLast.0 | CGBitmapInfo::ByteOrder32Big.0;

                let ctx = CGBitmapContextCreate(
                    buf.as_mut_ptr().cast(),
                    width,
                    height,
                    8,
                    bytes_per_row,
                    Some(&color_space),
                    bitmap_info,
                )?;

                CGContext::set_allows_antialiasing(Some(&ctx), true);
                CGContext::set_should_antialias(Some(&ctx), true);
                font.draw_glyphs(NonNull::from(&glyph), NonNull::from(&draw_pos), 1, &ctx);

                // Row 0 is already top; un-premultiply RGB so SourceAlpha blending is exact
                let mut rgba = vec![0u8; buf.len()];
                for y in 0..height {
                    let row = y * bytes_per_row;
                    for x in 0..width {
                        let s = row + x * 4;
                        let a = buf[s + 3];
                        if a == 0 {
                            continue;
                        }
                        if a == 255 {
                            rgba[s..s + 4].copy_from_slice(&buf[s..s + 4]);
                        } else {
                            let inv = 255.0 / a as f32;
                            rgba[s] = ((buf[s] as f32 * inv).min(255.0)) as u8;
                            rgba[s + 1] = ((buf[s + 1] as f32 * inv).min(255.0)) as u8;
                            rgba[s + 2] = ((buf[s + 2] as f32 * inv).min(255.0)) as u8;
                            rgba[s + 3] = a;
                        }
                    }
                }

                Some(RasterizedGlyph {
                    width: width as u16,
                    height: height as u16,
                    bearing_x: min_x as f32,
                    bearing_y: max_y as f32,
                    is_color: true,
                    pixels: rgba,
                })
            }
        }
    }
}
