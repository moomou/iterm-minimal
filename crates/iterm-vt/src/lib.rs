pub mod bench;
pub mod cell;
pub mod grid;
pub mod inline_image;
pub mod parser;
pub mod screen;
pub mod scrollback;

pub use cell::{Cell, CellExtra, CellFlags, PackedColor, Pen};
pub use grid::{row_cells_to_string, ActiveGrid, PromptMark, RowMeta, StringPool};
pub use inline_image::{
    compute_image_cell_size, decode_transfer, parse_remote_artifact, DecodedTransfer,
    FileTransferParams, ImageCellRef, ImageDimension, InlineImage,
};
pub use parser::{CsiParams, VtParser, VtSink};
pub use screen::{
    Cursor, SearchMatch, Selection, SelectionMode, SelectionPoint, TermModes, TerminalEvent,
    TerminalScreen,
};
pub use scrollback::{Scrollback, ScrollbackLineMeta};

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn ascii_fast_path_and_truecolor_sgr() {
        let mut s = TerminalScreen::new(80, 24, 1000);
        s.feed(b"\x1b[1;38;2;255;85;85;48;5;235mHello, World!\x1b[0m\r\nSecond line");
        assert_eq!(s.primary.row_text(0), "Hello, World!");
        assert_eq!(s.primary.row_text(1), "Second line");

        let cell = s.primary.row(0)[0];
        assert_eq!(cell.ch, 'H');
        assert!(cell.flags.contains(CellFlags::BOLD));
        assert_eq!(cell.fg, PackedColor::rgb(255, 85, 85));
        assert_eq!(cell.bg, PackedColor::indexed(235));
    }

    #[test]
    fn colon_subparams_truecolor_and_curly_underline() {
        let mut s = TerminalScreen::new(80, 24, 100);
        s.feed(b"\x1b[4:3;38:2::10:20:30;58:2::255:0:0mErr\x1b[0m");
        let c = s.primary.row(0)[0];
        assert!(c.flags.contains(CellFlags::CURLY_UNDERLINE));
        assert_eq!(c.fg, PackedColor::rgb(10, 20, 30));
        assert_eq!(s.cell_underline_color(&c), PackedColor::rgb(255, 0, 0));
    }

    #[test]
    fn osc6_tab_color_matches_user_bash_fns() {
        let mut s = TerminalScreen::new(80, 24, 100);
        // Exact sequences emitted by ~/.bash_fns color-ssh & tab-reset
        s.feed(b"\x1b]6;1;bg;red;brightness;255\x07");
        s.feed(b"\x1b]6;1;bg;green;brightness;145\x07");
        s.feed(b"\x1b]6;1;bg;blue;brightness;117\x07");
        let c = s.tab_color.expect("tab color set");
        assert_eq!(c.to_u8(), [255, 145, 117, 255]);

        s.feed(b"\x1b]6;1;bg;*;default\x07");
        assert!(s.tab_color.is_none());
    }

    #[test]
    fn tmux_dcs_passthrough_unwraps_it2dl_and_remote_artifact() {
        let mut s = TerminalScreen::new(80, 24, 100);

        // Wrap RemoteHost + CurrentDir + explicit Artifact inside tmux DCS passthrough!
        s.feed(b"\x1bPtmux;\x1b\x1b]1337;RemoteHost=moomou@gpu-server\x07\x1b\\");
        s.feed(b"\x1bPtmux;\x1b\x1b]1337;CurrentDir=/data/exp\x07\x1b\\");
        s.feed(b"\x1bPtmux;\x1b\x1b]1337;Artifact=v=1;host=moomou@gpu-server;path=/data/exp/out.png;len=7\x07\x1b\\out.png");

        assert_eq!(s.current_host(), Some("moomou@gpu-server"));
        assert_eq!(s.current_cwd(), Some("/data/exp"));
        assert_eq!(s.primary.row_text(0), "out.png");
        let abs0 = s.screen_top_abs_line();
        let arts = s.artifacts_at_abs_line(abs0).expect("artifact stamped");
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].abs_path, "/data/exp/out.png");
        assert_eq!(arts[0].start_col, 0);
        assert_eq!(arts[0].end_col, 7);

        // Test tmux-wrapped multipart it2dl download!
        let payload = b"hello downloaded file!";
        let name_b64 = base64::engine::general_purpose::STANDARD.encode("report.txt");
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let mid = data_b64.len() / 2;

        s.feed(
            format!(
                "\x1bPtmux;\x1b\x1b]1337;MultipartFile=name={name_b64};size={}\x07\x1b\\",
                payload.len()
            )
            .as_bytes(),
        );
        s.feed(format!("\x1bPtmux;\x1b\x1b]1337;FilePart={}\x07\x1b\\", &data_b64[..mid]).as_bytes());
        s.feed(format!("\x1bPtmux;\x1b\x1b]1337;FilePart={}\x07\x1b\\", &data_b64[mid..]).as_bytes());
        s.feed(b"\x1bPtmux;\x1b\x1b]1337;FileEnd\x07\x1b\\");

        let dl = s.drain_events().into_iter().find_map(|e| match e {
            TerminalEvent::FileDownload { name, data } => Some((name, data)),
            _ => None,
        });
        assert_eq!(dl, Some(("report.txt".to_string(), payload.to_vec())));
    }

    #[test]
    fn inline_image_imgcat_png_places_image_cells() {
        // Generate a real 2x2 red PNG in memory
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let mut png_bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

        let mut s = TerminalScreen::new(80, 24, 100);
        s.feed(format!("\x1b]1337;File=inline=1;width=4;height=2:{b64}\x07").as_bytes());

        assert_eq!(s.images.len(), 1);
        let cell = s.primary.row(0)[0];
        assert!(cell.flags.contains(CellFlags::IMAGE_CELL));
        let iref = s.image_cell_ref(cell.extra_id).expect("image cell ref");
        assert_eq!(iref.total_cols, 4);
        assert_eq!(iref.total_rows, 2);
        assert_eq!(iref.tile_col, 0);
        assert_eq!(iref.tile_row, 0);
    }

    #[test]
    fn scrollback_search_and_selection_extraction() {
        let mut s = TerminalScreen::new(40, 5, 500);
        for i in 0..30 {
            s.feed(format!("log entry number {i:03} status=OK\r\n").as_bytes());
        }
        assert!(s.scrollback.len() >= 25);

        let found = s.search("number 01[0-5]", true, false);
        assert_eq!(found, 6);

        let top = s.screen_top_abs_line();
        s.selection = Some(Selection {
            start: SelectionPoint { abs_line: top, col: 0 },
            end: SelectionPoint { abs_line: top, col: 19 },
            mode: SelectionMode::Normal,
        });
        let text = s.extract_selection_text().unwrap();
        assert!(text.starts_with("log entry number "));
    }

    #[test]
    fn row_generation_only_bumps_dirty_rows() {
        let mut s = TerminalScreen::new(80, 24, 100);
        let gen_row0_before = s.primary.meta(0).generation;
        let gen_row5_before = s.primary.meta(5).generation;

        s.feed(b"\x1b[6;1Honly row 5 mutated");
        assert_eq!(s.primary.meta(0).generation, gen_row0_before);
        assert!(s.primary.meta(5).generation > gen_row5_before);
    }

    #[test]
    fn alt_screen_preserves_primary_buffer_and_cursor() {
        let mut s = TerminalScreen::new(80, 24, 100);
        s.feed(b"Primary text\r\nLine 2");
        let saved_col = s.cursor.col;
        let saved_row = s.cursor.row;

        s.feed(b"\x1b[?1049hAlt screen TUI");
        assert!(s.modes.contains(TermModes::ALT_SCREEN));
        assert_eq!(s.active_grid().row_text(0), "Alt screen TUI");

        s.feed(b"\x1b[?1049l");
        assert!(!s.modes.contains(TermModes::ALT_SCREEN));
        assert_eq!(s.active_grid().row_text(0), "Primary text");
        assert_eq!(s.cursor.col, saved_col);
        assert_eq!(s.cursor.row, saved_row);
    }

    #[test]
    fn tmux_reattach_decstbm_full_region_delete_and_insert_lines_never_panics() {
        // Exact repro of the macOS 44-row tmux reattach crash:
        // tmux enters alt screen, sets DECSTBM 1..43 (leaving row 44 for status bar),
        // then issues CSI 43 M / CSI 99 M / CSI 43 L / CSI 99 L / CSI 99 S / CSI 99 T!
        let mut s = TerminalScreen::new(120, 44, 1000);
        s.feed(b"\x1b[?1049h");
        s.feed(b"\x1b[1;43r");
        s.feed(b"\x1b[1;1H\x1b[43M");
        s.feed(b"\x1b[1;1H\x1b[99M");
        s.feed(b"\x1b[1;1H\x1b[43L");
        s.feed(b"\x1b[1;1H\x1b[99L");
        s.feed(b"\x1b[43S\x1b[99S\x1b[43T\x1b[99T");
        // Sub-region starting below row 0 as well
        s.feed(b"\x1b[5;43r");
        s.feed(b"\x1b[5;1H\x1b[39M\x1b[99M\x1b[39L\x1b[99L");
        // Resize smaller while in alt-screen, then leave alt-screen (saved cursor clamp)
        s.resize(80, 20);
        s.feed(b"\x1b[?1049l");
        // Out-of-bounds ICH/DCH/REP stress
        s.feed(b"\x1b[20;80H\x1b[99@\x1b[99P\x1b[99X\x1b[999b");
        assert!(s.cursor.row < s.rows());
        assert!(s.cursor.col < s.cols());
    }

    #[test]
    fn dual_tier_scrollback_preserves_packed_ascii_and_styled_lines_losslessly() {
        let mut s = TerminalScreen::new(60, 4, 1000);
        // Plain ASCII line -> packed at 1 byte/char
        s.feed(b"plain ascii line 0\r\n");
        // Styled truecolor line -> full 16B Cell slab
        s.feed(b"\x1b[1;38;2;12;34;56mstyled line 1\x1b[0m\r\n");
        // Unicode wide line -> full 16B Cell slab
        s.feed("日本語 line 2\r\n".as_bytes());
        // Push all three into scrollback
        s.feed(b"line 3\r\nline 4\r\nline 5\r\nline 6\r\n");

        assert!(s.scrollback.len() >= 3);
        assert_eq!(s.scrollback.line_text(0).as_deref(), Some("plain ascii line 0"));
        assert_eq!(s.scrollback.line_text(1).as_deref(), Some("styled line 1"));
        assert_eq!(s.scrollback.line_text(2).as_deref(), Some("日本語 line 2"));

        let mut row = vec![Cell::BLANK; 60];
        let m0 = s.scrollback.copy_line_padded(0, &mut row).unwrap();
        assert!(m0.is_packed_ascii());
        assert_eq!(row[0].ch, 'p');
        assert!(row[0].fg.is_default());

        let m1 = s.scrollback.copy_line_padded(1, &mut row).unwrap();
        assert!(!m1.is_packed_ascii());
        assert_eq!(row[0].ch, 's');
        assert_eq!(row[0].fg, PackedColor::rgb(12, 34, 56));
        assert!(row[0].flags.contains(CellFlags::BOLD));
    }

    #[test]
    fn combining_marks_variation_selectors_and_zwj_attach_to_base_cell() {
        let mut s = TerminalScreen::new(40, 5, 100);
        // Combining acute accent on 'e', variation selector on lightning, ZWJ emoji sequence
        s.feed("e\u{0301} ⚡\u{FE0F} 👨\u{200D}👩\u{200D}👧".as_bytes());
        let row = s.primary.row(0);
        assert_eq!(row[0].ch, 'e');
        assert!(row[0].flags.contains(CellFlags::HAS_GRAPHEME_TAIL));
        assert_eq!(s.cell_grapheme_tail(&row[0]), Some("\u{0301}"));

        // Verify ZWJ family emoji stays attached as one cluster on the '👨' cell
        let family_cell = row.iter().find(|c| c.ch == '👨').expect("man emoji cell");
        assert!(family_cell.flags.contains(CellFlags::HAS_GRAPHEME_TAIL));
        assert_eq!(s.cell_grapheme_tail(family_cell), Some("\u{200D}👩\u{200D}👧"));
    }

    #[test]
    fn primary_resize_reflows_soft_wrapped_lines_and_tracks_cursor() {
        let mut s = TerminalScreen::new(10, 6, 100);
        // Write a 20-char single logical line that wraps across rows 0 and 1
        s.feed(b"ABCDEFGHIJ0123456789\r\nprompt> X");
        assert_eq!(s.primary.row_text(0), "ABCDEFGHIJ");
        assert!(s.primary.meta(0).wrapped());
        assert_eq!(s.primary.row_text(1), "0123456789");
        assert!(!s.primary.meta(1).wrapped());
        assert_eq!(s.primary.row_text(2), "prompt> X");

        // Widen terminal to 24 cols -> wrapped line unwraps cleanly back into ONE row!
        s.resize(24, 6);
        assert_eq!(s.primary.row_text(0), "ABCDEFGHIJ0123456789");
        assert!(!s.primary.meta(0).wrapped());
        assert_eq!(s.primary.row_text(1), "prompt> X");
        assert_eq!(s.cursor.row, 1);
        assert_eq!(s.cursor.col, 9);

        // Narrow terminal to 6 cols -> wraps into 4 segments cleanly!
        s.resize(6, 10);
        assert_eq!(s.primary.row_text(0), "ABCDEF");
        assert!(s.primary.meta(0).wrapped());
        assert_eq!(s.primary.row_text(1), "GHIJ01");
        assert!(s.primary.meta(1).wrapped());
    }

    #[test]
    fn side_table_gc_prevents_u16_saturation_across_80k_hyperlinks() {
        // Bounded scrollback = 50 lines; emit 60,000 unique hyperlinks so old ones scroll off!
        // Without GC, cell_extras would hit u16::MAX (65,535) and break permanently.
        let mut s = TerminalScreen::new(40, 10, 50);
        for i in 0..60_000u32 {
            s.feed(format!("\x1b]8;;https://example.com/{i}\x07L\x1b]8;;\x07\r\n").as_bytes());
        }
        // Auto-GC at 48,000 watermark must have prevented u16::MAX (65,535) saturation!
        assert!(
            s.side_table_len() < 48_000,
            "expected auto-GC to prevent saturation, got {}",
            s.side_table_len()
        );
        // Latest hyperlink on screen must still resolve accurately!
        let r = s.cursor.row.saturating_sub(1);
        let cell = s.primary.row(r)[0];
        assert_eq!(cell.ch, 'L');
        assert_eq!(
            s.cell_hyperlink_url(&cell),
            Some("https://example.com/59999")
        );

        // Explicit GC shrinks side tables down to just the live cells (~60 rows)
        s.gc_side_tables();
        assert!(
            s.side_table_len() <= 100,
            "expected explicit GC to compact to live set, got {}",
            s.side_table_len()
        );
        assert_eq!(
            s.cell_hyperlink_url(&s.primary.row(r)[0]),
            Some("https://example.com/59999")
        );

        // Clear buffer should drop side_table back to empty (1 sentinel slot)
        s.clear_scrollback_and_screen();
        assert_eq!(s.side_table_len(), 1);
    }

    #[test]
    fn esctest2_vt_conformance_origin_mode_scroll_regions_and_wrap() {
        // 1. Origin mode (DECOM ?6) homes cursor inside DECSTBM region and clamps CUP
        let mut s = TerminalScreen::new(20, 10, 100);
        s.feed(b"\x1b[3;7r\x1b[?6h");
        assert_eq!((s.cursor.row, s.cursor.col), (2, 0));
        s.feed(b"\x1b[1;1HTOP\x1b[99;1HBOT");
        assert_eq!(s.primary.row_text(2), "TOP");
        assert_eq!(s.primary.row_text(6), "BOT");
        // Reverse index (ESC M) at top of scroll region scrolls region down without touching outside rows
        s.feed(b"\x1b[1;1H\x1bMNEW");
        assert_eq!(s.primary.row_text(2), "NEW");
        assert_eq!(s.primary.row_text(3), "TOP");

        // 2. Pending wrap + Backspace stays on last column without prematurely wrapping
        let mut w = TerminalScreen::new(5, 3, 10);
        w.feed(b"12345\x08Z");
        assert_eq!(w.primary.row_text(0), "123Z5");
        assert_eq!(w.cursor.row, 0);
    }

    #[test]
    fn dec_special_graphics_charset_renders_htop_tree_lines() {
        let mut s = TerminalScreen::new(40, 5, 100);
        // htop/ncurses switches G0 to DEC Special Graphics via ESC ( 0, draws tree, resets via ESC ( B
        s.feed(b"\x1b(0lqqk\r\nx  x\r\ntqqj\x1b(B normal");
        assert_eq!(s.primary.row_text(0), "┌──┐");
        assert_eq!(s.primary.row_text(1), "│  │");
        assert_eq!(s.primary.row_text(2), "├──┘ normal");
    }

    #[test]
    fn osc11_decrqm_and_decstr_respond_accurately() {
        let mut s = TerminalScreen::new(40, 5, 100);
        s.default_bg_rgb = (0x28, 0x2A, 0x36);
        s.feed(b"\x1b]11;?\x07\x1b[?2026h\x1b[?2026$p\x1b[!p\x1b[?2026$p");
        let wb: Vec<String> = s
            .drain_events()
            .into_iter()
            .filter_map(|e| match e {
                TerminalEvent::WriteBack(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(wb[0], "\x1b]11;rgb:2828/2a2a/3636\x1b\\");
        assert_eq!(wb[1], "\x1b[?2026;1$y");
        assert_eq!(wb[2], "\x1b[?2026;2$y");
    }

    #[test]
    fn scrollback_pullback_reflow_restores_history_on_widen_and_grow() {
        let mut s = TerminalScreen::new(10, 4, 200);
        // Push 6 lines so top 2 lines scroll into scrollback
        s.feed(b"LINE0\r\nLINE1\r\nABCDEFGHIJ0123456789\r\nPROMPT>");
        assert!(s.scrollback.len() >= 1);

        // Grow & widen window -> lines pulled cleanly back from scrollback AND wrapped line unwraps!
        s.resize(30, 8);
        let visible: Vec<String> = (0..8).map(|r| s.primary.row_text(r)).collect();
        assert!(visible.iter().any(|l| l == "LINE0"), "visible={visible:?}");
        assert!(
            visible.iter().any(|l| l == "ABCDEFGHIJ0123456789"),
            "wrapped line should unwrap: {visible:?}"
        );
        assert_eq!(s.primary.row_text(s.cursor.row), "PROMPT>");
        assert_eq!(s.cursor.col, 7);
    }

    #[test]
    fn xtgettcap_osc4_osc9_osc777_and_title_stack_work() {
        let mut s = TerminalScreen::new(40, 5, 100);
        // Push title, change title, pop title (`CSI 22 t` / `CSI 23 t`)
        s.feed(b"\x1b]0;shell\x07\x1b[22t\x1b]0;vim\x07\x1b[23t");
        assert_eq!(s.title, "shell");

        // Query XTGETTCAP (`RGB`, `TN`, `ZZ`), OSC 4 palette 1, OSC 9 & OSC 777 notifications
        s.drain_events();
        s.feed(
            b"\x1bP+q524742;544E;5A5A\x1b\\\x1b]4;1;?\x07\x1b]9;Build ready\x07\x1b]777;notify;CI;Passed\x07",
        );
        let evs = s.drain_events();
        let wbs: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::WriteBack(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect();
        assert!(wbs.iter().any(|w| w == "\x1bP1+r524742\x1b\\"), "wbs={wbs:?}");
        assert!(wbs.iter().any(|w| w.starts_with("\x1bP1+r544E=")), "wbs={wbs:?}");
        assert!(wbs.iter().any(|w| w == "\x1bP0+r5A5A\x1b\\"), "wbs={wbs:?}");
        assert!(wbs.iter().any(|w| w.starts_with("\x1b]4;1;rgb:")), "wbs={wbs:?}");

        let notifs: Vec<(&str, &str)> = evs
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::Notification { title, body } => Some((title.as_str(), body.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(notifs.len(), 2);
        assert_eq!(notifs[0].1, "Build ready");
        assert_eq!(notifs[1], ("CI", "Passed"));
    }

    #[test]
    fn bulk_ascii_line_fast_path_matches_byte_by_byte_reference_losslessly() {
        // Feed 150 lines (including wrapped >cols lines, trailing spaces, empty lines) in one chunk
        // vs 1 byte at a time -> verify primary screen, scrollback, wrapped flags, and cursor match 100%!
        let mut payload = Vec::new();
        for i in 0..150usize {
            if i % 11 == 0 {
                payload.extend_from_slice(b"\r\n");
            } else if i % 7 == 0 {
                payload.extend_from_slice(
                    format!("LONG-{i:03}-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij\r\n")
                        .as_bytes(),
                );
            } else {
                payload.extend_from_slice(format!("line {i:03} trailing   \r\n").as_bytes());
            }
        }
        payload.extend_from_slice(b"final prompt> ");

        let mut fast = TerminalScreen::new(32, 10, 500);
        fast.feed(&payload);

        let mut slow = TerminalScreen::new(32, 10, 500);
        for b in &payload {
            slow.feed(std::slice::from_ref(b));
        }

        assert_eq!(fast.cursor.row, slow.cursor.row);
        assert_eq!(fast.cursor.col, slow.cursor.col);
        assert_eq!(fast.scrollback.len(), slow.scrollback.len());
        for r in 0..10 {
            assert_eq!(fast.primary.row_text(r), slow.primary.row_text(r), "primary row {r}");
            assert_eq!(fast.primary.meta(r).wrapped(), slow.primary.meta(r).wrapped(), "wrapped row {r}");
        }
        for i in 0..slow.scrollback.len() {
            assert_eq!(fast.scrollback.line_text(i), slow.scrollback.line_text(i), "sb line {i}");
        }
    }

    #[test]
    fn vertical_resize_scrollback_pullback_never_collides_sb_gen_and_sgr_slice_keeps_blink_double_ul() {
        // 1. SGR 5/21/25 parity in multi-param fast CSI path
        let mut s = TerminalScreen::new(20, 4, 50);
        s.feed(b"\x1b[1;5;21mA\x1b[25mB");
        let r0 = s.primary.row(0);
        assert!(r0[0].flags.contains(CellFlags::BOLD | CellFlags::BLINK | CellFlags::DOUBLE_UNDERLINE));
        assert!(!r0[1].flags.contains(CellFlags::BLINK));

        // 2. Scrollback generation epoch bumps across pop_back_line so vertical resize + re-push never collides
        let mut sb = TerminalScreen::new(20, 3, 50);
        sb.feed(b"OLD0\r\nOLD1\r\nOLD2\r\nOLD3\r\nOLD4\r\n");
        sb.scroll_viewport(1);
        let mut scratch = Vec::new();
        let (_, gen_before, _, _, _) = sb.read_visible_row(0, &mut scratch);
        sb.viewport_offset = 0;

        // Grow vertically (pulls lines back from scrollback), then shrink & push brand-new content
        sb.resize(20, 6);
        sb.resize(20, 3);
        sb.feed(b"NEW0\r\nNEW1\r\nNEW2\r\nNEW3\r\n");
        sb.scroll_viewport(1);
        let (_, gen_after, _, _, _) = sb.read_visible_row(0, &mut scratch);
        assert_ne!(
            gen_before, gen_after,
            "sb_gen must change after scrollback pull-back + re-push!"
        );
    }

    #[test]
    fn line_and_trimmed_line_selection_handles_whitespace_softwrap_wide_and_blank() {
        let mut s = TerminalScreen::new(20, 6, 50);
        s.feed(b"   hello world   \r\n");
        s.feed(b"    x    \r\n");
        s.feed(b"        \r\n");
        s.feed("  你好  \r\n".as_bytes());

        // Row 0: Option+double-click selects whole line (cols 0..19)
        let l0 = s.visible_abs_line(0);
        let whole = s.select_whole_line(l0);
        assert_eq!(whole.mode, SelectionMode::Line);
        assert_eq!(whole.start, SelectionPoint { abs_line: l0, col: 0 });
        assert_eq!(whole.end, SelectionPoint { abs_line: l0, col: 19 });
        assert_eq!(s.extract_selection_text().as_deref(), Some("   hello world"));

        // Row 0: Option+Shift+double-click selects trimmed text only (cols 3..13)
        let trimmed = s.select_trimmed_line(l0).unwrap();
        assert_eq!(trimmed.start, SelectionPoint { abs_line: l0, col: 3 });
        assert_eq!(trimmed.end, SelectionPoint { abs_line: l0, col: 13 });
        assert!(!trimmed.contains(l0, 2));
        assert!(trimmed.contains(l0, 3));
        assert!(trimmed.contains(l0, 13));
        assert!(!trimmed.contains(l0, 14));
        assert_eq!(s.extract_selection_text().as_deref(), Some("hello world"));

        // Row 1: Single non-whitespace character
        let l1 = s.visible_abs_line(1);
        let single = s.select_trimmed_line(l1).unwrap();
        assert_eq!(single.start, SelectionPoint { abs_line: l1, col: 4 });
        assert_eq!(single.end, SelectionPoint { abs_line: l1, col: 4 });
        assert_eq!(s.extract_selection_text().as_deref(), Some("x"));

        // Row 2: All-whitespace line selects nothing
        let l2 = s.visible_abs_line(2);
        assert!(s.select_trimmed_line(l2).is_none());
        assert!(s.selection.is_none());

        // Row 3: Wide CJK characters preserve full 2-column highlight on trailing wide char
        let l3 = s.visible_abs_line(3);
        let wide = s.select_trimmed_line(l3).unwrap();
        assert_eq!(wide.start, SelectionPoint { abs_line: l3, col: 2 });
        assert_eq!(wide.end, SelectionPoint { abs_line: l3, col: 5 });
        assert_eq!(s.extract_selection_text().as_deref(), Some("你好"));

        // Soft-wrapped multi-row line (width 10): "  abcdefgh" (wrapped) + "ij  "
        let mut w = TerminalScreen::new(10, 4, 50);
        w.feed(b"  abcdefghij  \r\n");
        let r0 = w.visible_abs_line(0);
        let r1 = w.visible_abs_line(1);
        assert!(w.primary.meta(0).wrapped());
        // Clicking continuation row r1 selects across both rows
        let wrap_whole = w.select_whole_line(r1);
        assert_eq!(wrap_whole.start.abs_line, r0);
        assert_eq!(wrap_whole.end.abs_line, r1);
        assert_eq!(w.extract_selection_text().as_deref(), Some("  abcdefghij"));

        let wrap_trim = w.select_trimmed_line(r1).unwrap();
        assert_eq!(wrap_trim.start, SelectionPoint { abs_line: r0, col: 2 });
        assert_eq!(wrap_trim.end, SelectionPoint { abs_line: r1, col: 1 });
        assert_eq!(w.extract_selection_text().as_deref(), Some("abcdefghij"));
    }

    #[test]
    fn adversarial_random_vt_fuzz_1mb_never_panics() {
        // Deterministic 1 MiB adversarial corpus mixing random CSI/OSC/DCS/tmux/UTF-8/invalid bytes + mid-stream resizes!
        let mut s = TerminalScreen::new(80, 24, 500);
        let mut rng: u64 = 0xDEAD_BEEF_CAFE_BABEu64;
        let mut next = || -> u64 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let mut chunk = Vec::with_capacity(4096);
        let mut total = 0usize;
        while total < 1_048_576 {
            chunk.clear();
            for _ in 0..4096 {
                let r = next();
                match r & 0x1F {
                    0 => chunk.extend_from_slice(b"\x1b["),
                    1 => chunk.extend_from_slice(b"\x1b]1337;"),
                    2 => chunk.extend_from_slice(b"\x1bPtmux;\x1b\x1b]"),
                    3 => chunk.extend_from_slice(b"\x1b\\"),
                    4 => chunk.push(0x07),
                    5 => chunk.extend_from_slice(b"\x1b[?1049h"),
                    6 => chunk.extend_from_slice(b"\x1b[?1049l"),
                    7 => chunk.extend_from_slice(b"\x1b[1;23r\x1b[99M\x1b[99L"),
                    8 => chunk.extend_from_slice(b"\x1b[65535;65535H"),
                    9 => chunk.extend_from_slice("日e\u{0301}👨\u{200D}👧".as_bytes()),
                    _ => chunk.push((r >> 8) as u8),
                }
            }
            s.feed(&chunk);
            total += chunk.len();
            if (next() & 0xF) == 0 {
                let nc = ((next() % 140) + 2) as usize;
                let nr = ((next() % 50) + 1) as usize;
                s.resize(nc, nr);
            }
            assert!(s.cursor.row < s.rows());
            assert!(s.cursor.col < s.cols());
        }
    }
}
