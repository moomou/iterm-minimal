pub mod atlas;
pub mod plan;
pub mod shaper;

pub use atlas::{AtlasEntry, AtlasUpload, FontStyle, GlyphAtlas, GlyphKey, RasterizedGlyph};
pub use plan::{
    FrameBatches, GlyphInstance, ImageDraw, PaneRenderCache, PaneSnapshot, QuadInstance,
    ViewportUniforms, TERMINAL_METAL_SHADER_SOURCE,
};
pub use shaper::{CachedShaper, CellMetrics, MockShaper, ShapedGlyph, TextShaper};

#[cfg(test)]
mod tests {
    use super::*;
    use iterm_profile::Profile;
    use iterm_vt::TerminalScreen;

    #[test]
    fn dirty_row_cache_rebuilds_only_mutated_rows() {
        let mut screen = TerminalScreen::new(120, 40, 1000);
        for r in 0..40 {
            screen.feed(format!("line {r:02}: fn foo(a: i32) -> bool {{ a != 0 }}\r\n").as_bytes());
        }
        let mut prof = Profile::default();
        prof.ascii_ligatures = true;
        let atlas = GlyphAtlas::new(1024, 1024);
        let mut shaper = CachedShaper::new(MockShaper::default());
        let mut cache = PaneRenderCache::new();
        let mut batches = FrameBatches::default();

        // First cold frame builds all 40 rows
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 1440.0, 960.0, true, 0.0, true, &mut batches,
        );
        assert_eq!(batches.rows_rebuilt, 40);
        assert_eq!(batches.rows_reused, 0);
        assert!(!batches.glyphs.is_empty());

        // Second frame with 1 row mutated rebuilds ONLY 1-2 rows (mutated + cursor row) and reuses >=38!
        screen.feed(b"\x1b[10;1Hmutated => ligature != check");
        batches.clear();
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 1440.0, 960.0, true, 0.0, true, &mut batches,
        );
        assert!(batches.rows_rebuilt <= 2, "rebuilt={}", batches.rows_rebuilt);
        assert!(batches.rows_reused >= 38, "reused={}", batches.rows_reused);
        assert!(shaper.hits > 0, "shaped-run cache should have hits");

        // Third frame: SCROLL UP BY 1 LINE — scroll-invariant row_gen cache reuses >=38 of 40 shifted rows!
        screen.feed(b"\x1b[40;1H\r\nnew scrolled bottom line");
        batches.clear();
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 1440.0, 960.0, true, 0.0, true, &mut batches,
        );
        assert!(
            batches.rows_reused >= 37,
            "scroll-invariant row cache must reuse shifted rows (reused={}, rebuilt={})",
            batches.rows_reused,
            batches.rows_rebuilt
        );
    }

    #[test]
    fn metal_shader_source_contains_all_entrypoints() {
        for sym in [
            "quad_vertex",
            "quad_fragment",
            "glyph_vertex",
            "glyph_fragment",
            "image_vertex",
            "image_fragment",
        ] {
            assert!(
                TERMINAL_METAL_SHADER_SOURCE.contains(sym),
                "missing Metal shader entry point: {sym}"
            );
        }
    }

    #[test]
    fn smart_ligature_bypass_skips_plain_words_but_shapes_ligatures_and_unicode() {
        for plain in ["NVIDIA-SMI", "719633", "4h08:30", "75.2%", "moomou", "Tasks:", "570.211.01"] {
            assert!(
                !plan::run_needs_shaping(plain, true),
                "{plain} should take O(1) ASCII fast path"
            );
        }
        for lig in ["->", "=>", "!=", "===", "<=", ">=", "::", "//", "/*", "a->b", "日本語", "λ"] {
            assert!(
                plan::run_needs_shaping(lig, true),
                "{lig} must invoke ligature/Unicode shaper"
            );
        }
        assert!(!plan::run_needs_shaping("->==!=", false));
        assert!(plan::run_needs_shaping("日本語", false));
    }

    #[test]
    fn alt_screen_htop_never_collides_with_primary_row_cache() {
        let mut screen = TerminalScreen::new(80, 24, 500);
        for r in 0..24 {
            screen.feed(format!("PRIMARY SHELL ROW {r:02}\r\n").as_bytes());
        }
        let prof = Profile::default();
        let atlas = GlyphAtlas::new(1024, 1024);
        let mut shaper = CachedShaper::new(MockShaper::default());
        let mut cache = PaneRenderCache::new();
        let mut batches = FrameBatches::default();

        // Render primary shell (populates PaneRenderCache with primary row_gens)
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 960.0, 576.0, true, 0.0, true, &mut batches,
        );
        assert_eq!(batches.rows_rebuilt, 24);

        // Enter alt-screen (like htop `\x1b[?1049h`) — ZERO rows may erroneously reuse cached primary rows!
        screen.feed(b"\x1b[?1049h\x1b[1;1HCPU[||||  20%]");
        batches.clear();
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 960.0, 576.0, true, 0.0, true, &mut batches,
        );
        assert_eq!(
            batches.rows_reused, 0,
            "entering alt-screen MUST NOT collide with cached primary rows!"
        );
        assert_eq!(batches.rows_rebuilt, 24);

        // Exit alt-screen (`\x1b[?1049l`) — primary rows were marked dirty and must NOT reuse alt-screen rows!
        screen.feed(b"\x1b[?1049l");
        batches.clear();
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 960.0, 576.0, true, 0.0, true, &mut batches,
        );
        assert_eq!(batches.rows_reused, 0);
        assert_eq!(batches.rows_rebuilt, 24);
    }

    #[test]
    fn multipane_clips_geometry_and_draws_procedural_box_elements() {
        let mut screen = TerminalScreen::new(40, 10, 100);
        // Box-drawing + block elements + multi-color htop CPU bar
        screen.feed("┌───┐█▀▄\r\n\x1b[32m|||\x1b[31m|||\x1b[0m".as_bytes());
        let mut prof = Profile::default();
        prof.ascii_ligatures = true;
        let atlas = GlyphAtlas::new(1024, 1024);
        let mut shaper = CachedShaper::new(MockShaper::default());
        let mut cache = PaneRenderCache::new();
        let mut batches = FrameBatches::default();

        // Clip strictly to narrow 200x100 split pane rect
        let pane_x = 10.0;
        let pane_y = 26.0;
        let pane_w = 200.0;
        let pane_h = 100.0;
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            pane_x, pane_y, pane_w, pane_h, true, 0.0, true, &mut batches,
        );

        // Procedural box-drawing / block quads were emitted
        assert!(!batches.deco_quads.is_empty(), "expected procedural box/block deco quads");

        // EVERY emitted quad & glyph must stay strictly inside [pane_x..pane_x+pane_w, pane_y..pane_y+pane_h]
        for q in batches.bg_quads.iter().chain(batches.deco_quads.iter()) {
            assert!(q.rect[0] >= pane_x - 0.01 && q.rect[0] + q.rect[2] <= pane_x + pane_w + 0.01);
            assert!(q.rect[1] >= pane_y - 0.01 && q.rect[1] + q.rect[3] <= pane_y + pane_h + 0.01);
        }
        for g in &batches.glyphs {
            assert!(g.dst_rect[0] >= pane_x - 0.01 && g.dst_rect[0] + g.dst_rect[2] <= pane_x + pane_w + 0.01);
            assert!(g.dst_rect[1] >= pane_y - 0.01 && g.dst_rect[1] + g.dst_rect[3] <= pane_y + pane_h + 0.01);
        }
    }

    #[test]
    fn procedural_braille_and_lru_shaper_cache_eviction() {
        let mut screen = TerminalScreen::new(10, 2, 10);
        // Full 8-dot braille '⣿' (U+28FF) should emit exactly 8 dot quads & zero atlas glyphs!
        screen.feed("⣿".as_bytes());
        let prof = Profile::default();
        let atlas = GlyphAtlas::new(512, 512);
        let mut shaper = CachedShaper::new(MockShaper::default());
        let mut cache = PaneRenderCache::new();
        let mut batches = FrameBatches::default();

        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 200.0, 60.0, true, 0.0, true, &mut batches,
        );
        assert_eq!(batches.deco_quads.len(), 8);
        assert_eq!(atlas.entry_count(), 0);

        // Generational LRU shaper evicts only ~25% on overflow instead of wiping all 16k entries!
        for i in 0..17_000u32 {
            let s = format!("run_{i}->val");
            let _ = shaper.shape_cached(&s, FontStyle::Regular, true);
        }
        assert!(
            shaper.cache_len() > 10_000 && shaper.cache_len() <= 16_384,
            "expected gradual LRU eviction retaining hot entries, got {}",
            shaper.cache_len()
        );
    }

    #[test]
    fn procedural_powerline_sdfs_and_non_block_cursor_clip_to_pane() {
        let mut screen = TerminalScreen::new(20, 4, 10);
        screen.cursor.style = iterm_profile::CursorStyle::Underline;
        // Emit all 4 solid Powerline separators (   )
        screen.feed("\u{E0B0}\u{E0B2}\u{E0B4}\u{E0B6}".as_bytes());
        screen.cursor.col = 19; // Put cursor far outside narrow pane clip

        let prof = Profile::default();
        let atlas = GlyphAtlas::new(512, 512);
        let mut shaper = CachedShaper::new(MockShaper::default());
        let mut cache = PaneRenderCache::new();
        let mut batches = FrameBatches::default();

        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 100.0, 80.0, true, 0.0, true, &mut batches,
        );
        let modes: Vec<u32> = batches.deco_quads.iter().map(|q| q.mode).collect();
        assert_eq!(modes, vec![4, 5, 6, 7]);
        assert_eq!(atlas.entry_count(), 0);
        // Out-of-bounds underline cursor at col 19 (x=228 > pane_w=100) must be cleanly clipped out!
        assert!(batches.overlay_quads.is_empty());
    }
}
