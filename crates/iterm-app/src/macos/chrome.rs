//! Window chrome geometry & GPU overlay rendering:
//! - Compact 26 pt unified titlebar + tab strip (level with macOS traffic lights)
//! - Inline Find bar, Toast notifications, visual bell, hotkey accent border
//! - Inline `NSTextInputClient` IME preedit text + underline rendered at the active cursor
//! - Pure text helpers (`get_local_hostname`, `word_bounds_at`, `extract_word_at`)

use crate::macos::coretext_shaper::CoreTextShaper;
use crate::macos::window::{
    ImeState, TerminalWindowState, TransferActionHit, TransferHitbox,
};
use iterm_profile::{is_word_char, Profile};
use iterm_pty::{format_bytes, PaneRect, SessionId, TransferDirection, TransferState};
use iterm_render::{
    CachedShaper, FontStyle, GlyphAtlas, GlyphInstance, GlyphKey, QuadInstance, TextShaper,
};
use std::time::{Duration, Instant};

/// Compact unified titlebar + tab bar height (tightly aligned with macOS traffic lights at y=8..20pt).
pub(crate) const TITLEBAR_HEIGHT_PT: f32 = 26.0;
/// Left inset reserving clean space for the macOS red/yellow/green window buttons.
pub(crate) const TRAFFIC_LIGHT_INSET_PT: f32 = 74.0;
/// Width of the `+` new-tab button on the right edge of the unified titlebar.
pub(crate) const NEW_TAB_BTN_WIDTH_PT: f32 = 24.0;
pub(crate) const FIND_BAR_HEIGHT_PT: f32 = 24.0;
pub(crate) const DIVIDER_WIDTH_PT: f32 = 1.5;

impl TerminalWindowState {
    pub(crate) fn tab_bar_height_px(&self) -> f32 {
        (TITLEBAR_HEIGHT_PT * self.scale_factor).round()
    }

    pub(crate) fn tab_strip_bounds_px(&self, vp_w: f32) -> (f32, f32, f32) {
        let left = (TRAFFIC_LIGHT_INSET_PT * self.scale_factor).round();
        let plus_w = (NEW_TAB_BTN_WIDTH_PT * self.scale_factor).round();
        let right = (vp_w - plus_w - 6.0 * self.scale_factor).max(left + 40.0);
        (left, right, plus_w)
    }

    pub(crate) fn pane_area_rect(&self, vp_w: f32, vp_h: f32) -> PaneRect {
        let top = self.tab_bar_height_px();
        PaneRect {
            x: 0.0,
            y: top,
            width: vp_w.max(1.0),
            height: (vp_h - top).max(1.0),
        }
    }

    /// Compute the pixel rect `(x, y, w, h)` of the active terminal cursor in view coordinates
    /// (top-left origin, physical pixels). Used both for inline IME preedit rendering and for
    /// Cocoa `NSTextInputClient::firstRectForCharacterRange:` candidate window placement.
    pub(crate) fn active_cursor_rect_px(&self, vp_w: f32, vp_h: f32) -> Option<(f32, f32, f32, f32)> {
        let active_tab = self.tabs.try_active_tab()?;
        let active_sid = active_tab.panes.active;
        let bounds = self.pane_area_rect(vp_w, vp_h);
        let div_px = (DIVIDER_WIDTH_PT * self.scale_factor).max(1.0);
        let (panes, _) = active_tab.panes.layout(bounds, div_px);
        let (_, rect) = panes.into_iter().find(|(id, _)| *id == active_sid)?;
        let m = self.shaper.metrics();
        let (cur_col, cur_row) = if let Some(sess) = self.sessions.get(&active_sid) {
            let s = sess.screen.lock();
            (s.cursor.col, s.cursor.row)
        } else {
            (0, 0)
        };
        let cx = rect.x + cur_col as f32 * m.cell_width;
        let cy = rect.y + cur_row as f32 * m.cell_height;
        Some((cx, cy, m.cell_width, m.cell_height))
    }

    /// Render unified titlebar/tab strip, Find bar, Toast notifications, visual bell,
    /// hotkey border, and active IME preedit text + underline into `self.batches`.
    pub(crate) fn render_window_chrome(
        &mut self,
        vp_w: f32,
        vp_h: f32,
        panes: &[(SessionId, PaneRect)],
        active_sid: SessionId,
    ) {
        // Unified minimal titlebar + tab bar level with red/yellow/green traffic lights!
        let tab_h = self.tab_bar_height_px();
        if tab_h > 0.0 {
            let bg = self.profile.colors.background;
            let bar_alpha = (1.0 - self.profile.transparency * 0.65).clamp(0.4, 0.98);
            self.batches.overlay_quads.push(QuadInstance {
                rect: [0.0, 0.0, vp_w, tab_h],
                color: [
                    (bg.r * 0.85).clamp(0.04, 1.0),
                    (bg.g * 0.85).clamp(0.04, 1.0),
                    (bg.b * 0.85).clamp(0.04, 1.0),
                    bar_alpha,
                ],
                mode: 0,
                param: 0.0,
                _pad: [0.0; 2],
            });
            // Subtle bottom hairline separator
            self.batches.overlay_quads.push(QuadInstance {
                rect: [
                    0.0,
                    tab_h - self.scale_factor.max(1.0),
                    vp_w,
                    self.scale_factor.max(1.0),
                ],
                color: [1.0, 1.0, 1.0, 0.07],
                mode: 0,
                param: 0.0,
                _pad: [0.0; 2],
            });

            let (strip_left, strip_right, plus_w) = self.tab_strip_bounds_px(vp_w);
            let strip_w = (strip_right - strip_left).max(1.0);
            let n = self.tabs.tabs.len().max(1);
            let show_pills = !self.app_cfg.hide_tab_bar_when_single || n > 1;
            let m = self.ui_shaper.metrics();
            let pad_y = (2.0 * self.scale_factor).round();
            let pill_h = (tab_h - pad_y * 2.0).max(8.0);
            let text_y = pad_y + ((pill_h - m.cell_height) * 0.5).max(0.0) + m.baseline_y;

            if show_pills {
                let tab_w = strip_w / n as f32;
                let gap = (3.0 * self.scale_factor).round();
                for (idx, tab) in self.tabs.tabs.iter().enumerate() {
                    let is_active = idx == self.tabs.active_index;
                    let base_color = if let Some(c) = tab.tab_color {
                        let k = if is_active { 0.85 } else { 0.42 };
                        [c.r * k, c.g * k, c.b * k, 0.92]
                    } else if is_active {
                        [0.24, 0.25, 0.30, 0.95]
                    } else {
                        [0.14, 0.14, 0.18, 0.72]
                    };
                    let tx = strip_left + idx as f32 * tab_w + gap * 0.5;
                    let tw = (tab_w - gap).max(8.0);
                    self.batches.overlay_quads.push(QuadInstance {
                        rect: [tx, pad_y, tw, pill_h],
                        color: base_color,
                        mode: 3,
                        param: 5.0 * self.scale_factor,
                        _pad: [0.0; 2],
                    });
                    let label = format!("{}: {}", idx + 1, tab.title);
                    let max_chars = ((tw - 14.0 * self.scale_factor) / m.cell_width)
                        .floor()
                        .max(1.0) as usize;
                    let fg = if is_active {
                        [0.96, 0.97, 0.99, 1.0]
                    } else {
                        [0.72, 0.74, 0.78, 0.88]
                    };
                    render_ui_string(
                        &label,
                        max_chars,
                        tx + 7.0 * self.scale_factor,
                        text_y,
                        fg,
                        &mut self.ui_shaper,
                        &self.atlas,
                        &mut self.batches.overlay_glyphs,
                    );
                }
            } else if let Some(tab) = self.tabs.tabs.first() {
                if let Some(c) = tab.tab_color {
                    self.batches.overlay_quads.push(QuadInstance {
                        rect: [strip_left, pad_y, strip_w, pill_h],
                        color: [c.r * 0.75, c.g * 0.75, c.b * 0.75, 0.85],
                        mode: 3,
                        param: 5.0 * self.scale_factor,
                        _pad: [0.0; 2],
                    });
                }
                let max_chars = ((strip_w - 16.0 * self.scale_factor) / m.cell_width)
                    .floor()
                    .max(1.0) as usize;
                let title_len = tab.title.chars().count().min(max_chars);
                let tx = strip_left + ((strip_w - title_len as f32 * m.cell_width) * 0.5).max(8.0);
                render_ui_string(
                    &tab.title,
                    max_chars,
                    tx,
                    text_y,
                    [0.82, 0.84, 0.88, 0.92],
                    &mut self.ui_shaper,
                    &self.atlas,
                    &mut self.batches.overlay_glyphs,
                );
            }

            // `+` New Tab button on right end of unified titlebar
            let plus_x = strip_right + 3.0 * self.scale_factor;
            self.batches.overlay_quads.push(QuadInstance {
                rect: [plus_x, pad_y, plus_w, pill_h],
                color: [1.0, 1.0, 1.0, 0.08],
                mode: 3,
                param: 5.0 * self.scale_factor,
                _pad: [0.0; 2],
            });
            let plus_tx = plus_x + ((plus_w - m.cell_width) * 0.5).max(2.0);
            render_ui_string(
                "+",
                1,
                plus_tx,
                text_y,
                [0.85, 0.87, 0.90, 0.95],
                &mut self.ui_shaper,
                &self.atlas,
                &mut self.batches.overlay_glyphs,
            );
        }

        // Inline IME preedit / marked text overlay (CJK, dead keys, press-and-hold accents)
        if !self.ime.marked_text.is_empty() {
            if let Some((_, rect)) = panes.iter().find(|(id, _)| *id == active_sid) {
                if let Some(snap) = self.snapshots.get(&active_sid) {
                    let ime_copy = self.ime.clone();
                    render_ime_preedit(
                        &ime_copy,
                        rect,
                        snap.cursor.col,
                        snap.cursor.row,
                        &self.profile,
                        self.scale_factor,
                        &mut self.shaper,
                        &self.atlas,
                        &mut self.batches.overlay_quads,
                        &mut self.batches.overlay_glyphs,
                    );
                }
            }
        }

        // Inline Find Bar overlay (top-right pill when Cmd+F active)
        if self.find.active {
            let m = self.ui_shaper.metrics();
            let fw = (300.0 * self.scale_factor).min(vp_w - 16.0);
            let fh = FIND_BAR_HEIGHT_PT * self.scale_factor;
            let fx = vp_w - fw - 10.0 * self.scale_factor;
            let fy = tab_h + 6.0 * self.scale_factor;
            self.batches.overlay_quads.push(QuadInstance {
                rect: [fx, fy, fw, fh],
                color: [0.16, 0.17, 0.22, 0.96],
                mode: 3,
                param: 5.0 * self.scale_factor,
                _pad: [0.0; 2],
            });
            let find_label = format!("Find: {} ({})", self.find.query, self.find.match_count);
            let max_chars = ((fw - 14.0 * self.scale_factor) / m.cell_width)
                .floor()
                .max(1.0) as usize;
            render_ui_string(
                &find_label,
                max_chars,
                fx + 7.0 * self.scale_factor,
                fy + ((fh - m.cell_height) * 0.5).max(1.0) + m.baseline_y,
                [0.98, 0.92, 0.55, 1.0],
                &mut self.ui_shaper,
                &self.atlas,
                &mut self.batches.overlay_glyphs,
            );
        }

        // Live Transfer Queue HUD card (bottom-right, zero focus stealing!):
        // Auto-shows whenever any scp transfer is Queued/Running or finished within 5s,
        // or whenever manually toggled open via View -> Transfers (Cmd+Opt+L).
        self.transfer_hitboxes.clear();
        let now_t = Instant::now();
        let all_transfers = self.transfers.snapshots();
        let visible_transfers: Vec<_> = all_transfers
            .into_iter()
            .filter(|t| {
                self.show_transfers
                    || matches!(t.state, TransferState::Queued | TransferState::Running)
                    || t.finished_at
                        .is_some_and(|fin| now_t.saturating_duration_since(fin) < Duration::from_secs(5))
            })
            .take(6)
            .collect();

        let mut bottom_right_occupied_h = 0.0f32;
        if self.show_transfers || !visible_transfers.is_empty() {
            let m = self.ui_shaper.metrics();
            let s = self.scale_factor;
            let card_w = (360.0 * s).min(vp_w - 20.0 * s).max(200.0 * s);
            let header_h = (22.0 * s).max(m.cell_height + 4.0 * s);
            let row_h = (36.0 * s).max(m.cell_height * 2.0 + 10.0 * s);
            let rows_n = visible_transfers.len().max(1);
            let pad = 8.0 * s;
            let card_h = (header_h + rows_n as f32 * row_h + pad).min(vp_h - tab_h - 20.0 * s);
            let cx = vp_w - card_w - 10.0 * s;
            let cy = vp_h - card_h - 10.0 * s;
            bottom_right_occupied_h = card_h + 16.0 * s;

            // Card backdrop
            self.batches.overlay_quads.push(QuadInstance {
                rect: [cx, cy, card_w, card_h],
                color: [0.10, 0.11, 0.15, 0.95],
                mode: 3,
                param: 7.0 * s,
                _pad: [0.0; 2],
            });
            // Subtle header divider
            self.batches.overlay_quads.push(QuadInstance {
                rect: [cx + pad, cy + header_h, card_w - pad * 2.0, s.max(1.0)],
                color: [1.0, 1.0, 1.0, 0.09],
                mode: 0,
                param: 0.0,
                _pad: [0.0; 2],
            });

            let active_n = self.transfers.active_count();
            let hdr_text = if active_n > 0 {
                format!("Transfers ({active_n} active)")
            } else {
                "Transfers".to_string()
            };
            let hdr_base_y = cy + ((header_h - m.cell_height) * 0.5).max(1.0) + m.baseline_y;
            render_ui_string(
                &hdr_text,
                32,
                cx + pad,
                hdr_base_y,
                [0.92, 0.94, 0.98, 1.0],
                &mut self.ui_shaper,
                &self.atlas,
                &mut self.batches.overlay_glyphs,
            );

            // Header [Clear] and [x] buttons
            let close_w = (20.0 * s).max(m.cell_width * 2.2);
            let close_x = cx + card_w - pad - close_w;
            let btn_y = cy + 3.0 * s;
            let btn_h = (header_h - 6.0 * s).max(m.cell_height);
            self.transfer_hitboxes.push(TransferHitbox {
                rect: [close_x, btn_y, close_w, btn_h],
                action: TransferActionHit::Dismiss,
            });
            render_ui_string(
                "x",
                1,
                close_x + (close_w - m.cell_width) * 0.5,
                hdr_base_y,
                [0.65, 0.68, 0.75, 0.95],
                &mut self.ui_shaper,
                &self.atlas,
                &mut self.batches.overlay_glyphs,
            );

            let has_finished = visible_transfers
                .iter()
                .any(|t| !matches!(t.state, TransferState::Queued | TransferState::Running));
            if has_finished {
                let clear_w = (44.0 * s).max(m.cell_width * 5.6);
                let clear_x = close_x - clear_w - 6.0 * s;
                self.batches.overlay_quads.push(QuadInstance {
                    rect: [clear_x, btn_y, clear_w, btn_h],
                    color: [1.0, 1.0, 1.0, 0.08],
                    mode: 3,
                    param: 4.0 * s,
                    _pad: [0.0; 2],
                });
                self.transfer_hitboxes.push(TransferHitbox {
                    rect: [clear_x, btn_y, clear_w, btn_h],
                    action: TransferActionHit::ClearFinished,
                });
                render_ui_string(
                    "Clear",
                    5,
                    clear_x + (clear_w - m.cell_width * 5.0) * 0.5,
                    hdr_base_y,
                    [0.78, 0.82, 0.90, 0.95],
                    &mut self.ui_shaper,
                    &self.atlas,
                    &mut self.batches.overlay_glyphs,
                );
            }

            if visible_transfers.is_empty() {
                let ry = cy + header_h + 6.0 * s;
                render_ui_string(
                    "No active transfers (Cmd+Click remote file or Cmd+V)",
                    ((card_w - pad * 2.0) / m.cell_width).floor().max(1.0) as usize,
                    cx + pad,
                    ry + m.baseline_y,
                    [0.58, 0.62, 0.70, 0.9],
                    &mut self.ui_shaper,
                    &self.atlas,
                    &mut self.batches.overlay_glyphs,
                );
            } else {
                for (i, t) in visible_transfers.iter().enumerate() {
                    let ry = cy + header_h + 4.0 * s + i as f32 * row_h;
                    if ry + row_h > cy + card_h + 2.0 * s {
                        break;
                    }
                    let dir_sym = match t.direction {
                        TransferDirection::Download => "↓",
                        TransferDirection::Upload => "↑",
                    };
                    let title_line = format!("{dir_sym} {} ({})", t.label, t.host);
                    let is_active = matches!(t.state, TransferState::Queued | TransferState::Running);
                    let cancel_w = if is_active { (20.0 * s).max(m.cell_width * 2.2) } else { 0.0 };
                    let text_w = (card_w - pad * 2.0 - cancel_w - 4.0 * s).max(20.0);
                    let max_chars = (text_w / m.cell_width).floor().max(1.0) as usize;
                    render_ui_string(
                        &title_line,
                        max_chars,
                        cx + pad,
                        ry + m.baseline_y,
                        [0.92, 0.95, 0.99, 1.0],
                        &mut self.ui_shaper,
                        &self.atlas,
                        &mut self.batches.overlay_glyphs,
                    );

                    // Cancel [x] button for Queued/Running transfers
                    if is_active {
                        let bx = cx + card_w - pad - cancel_w;
                        self.batches.overlay_quads.push(QuadInstance {
                            rect: [bx, ry, cancel_w, m.cell_height + 2.0 * s],
                            color: [0.85, 0.25, 0.25, 0.28],
                            mode: 3,
                            param: 4.0 * s,
                            _pad: [0.0; 2],
                        });
                        self.transfer_hitboxes.push(TransferHitbox {
                            rect: [bx, ry, cancel_w, m.cell_height + 2.0 * s],
                            action: TransferActionHit::Cancel(t.id),
                        });
                        render_ui_string(
                            "x",
                            1,
                            bx + (cancel_w - m.cell_width) * 0.5,
                            ry + m.baseline_y,
                            [1.0, 0.65, 0.65, 1.0],
                            &mut self.ui_shaper,
                            &self.atlas,
                            &mut self.batches.overlay_glyphs,
                        );
                    }

                    // Progress bar track + smooth fill
                    let bar_y = ry + m.cell_height + 3.0 * s;
                    let bar_h = (5.0 * s).max(4.0);
                    let bar_w = (card_w - pad * 2.0).max(20.0);
                    self.batches.overlay_quads.push(QuadInstance {
                        rect: [cx + pad, bar_y, bar_w, bar_h],
                        color: [1.0, 1.0, 1.0, 0.12],
                        mode: 3,
                        param: bar_h * 0.5,
                        _pad: [0.0; 2],
                    });
                    let (frac, bar_col) = match &t.state {
                        TransferState::Queued => (0.04f32, [0.55, 0.62, 0.78, 0.9]),
                        TransferState::Running => (
                            (t.percent.unwrap_or(8) as f32 / 100.0).clamp(0.05, 1.0),
                            [0.36, 0.78, 1.0, 0.98],
                        ),
                        TransferState::Completed => (1.0, [0.35, 0.90, 0.52, 0.98]),
                        TransferState::Failed(_) => (1.0, [0.95, 0.35, 0.35, 0.95]),
                        TransferState::Cancelled => (
                            (t.percent.unwrap_or(0) as f32 / 100.0).clamp(0.0, 1.0),
                            [0.65, 0.58, 0.42, 0.85],
                        ),
                    };
                    if frac > 0.0 {
                        self.batches.overlay_quads.push(QuadInstance {
                            rect: [cx + pad, bar_y, (bar_w * frac).max(bar_h), bar_h],
                            color: bar_col,
                            mode: 3,
                            param: bar_h * 0.5,
                            _pad: [0.0; 2],
                        });
                    }

                    // Status subtitle (%, bytes, speed, ETA or outcome)
                    let sub = match &t.state {
                        TransferState::Queued => "Queued...".to_string(),
                        TransferState::Running => {
                            let pct_s = t
                                .percent
                                .map(|p| format!("{p}%"))
                                .unwrap_or_else(|| "...".into());
                            let bytes_s = format_bytes(t.bytes_done);
                            let mut parts = vec![pct_s, bytes_s];
                            if !t.speed.is_empty() {
                                parts.push(t.speed.clone());
                            }
                            if !t.eta.is_empty() {
                                parts.push(format!("ETA {}", t.eta));
                            }
                            parts.join("  ")
                        }
                        TransferState::Completed => format!("Done  {}", format_bytes(t.bytes_done)),
                        TransferState::Cancelled => "Cancelled".to_string(),
                        TransferState::Failed(err) => format!("Failed: {err}"),
                    };
                    let sub_col = match &t.state {
                        TransferState::Completed => [0.68, 0.96, 0.76, 0.95],
                        TransferState::Failed(_) => [1.0, 0.58, 0.58, 0.98],
                        _ => [0.72, 0.78, 0.88, 0.92],
                    };
                    let sub_max = ((card_w - pad * 2.0) / m.cell_width).floor().max(1.0) as usize;
                    render_ui_string(
                        &sub,
                        sub_max,
                        cx + pad,
                        bar_y + bar_h + 2.0 * s + m.baseline_y * 0.88,
                        sub_col,
                        &mut self.ui_shaper,
                        &self.atlas,
                        &mut self.batches.overlay_glyphs,
                    );
                }
            }
        }

        // Status toast overlay (bottom-right pill, floats cleanly above Transfer Queue card if visible)
        if let Some((ref msg, until)) = self.status_toast {
            if Instant::now() < until {
                let m = self.ui_shaper.metrics();
                let tw = ((msg.len() as f32 + 3.0) * m.cell_width).min(vp_w - 24.0);
                let th = (FIND_BAR_HEIGHT_PT * self.scale_factor).max(m.cell_height + 6.0);
                let tx = vp_w - tw - 10.0 * self.scale_factor;
                let ty = vp_h - th - 10.0 * self.scale_factor - bottom_right_occupied_h;
                self.batches.overlay_quads.push(QuadInstance {
                    rect: [tx, ty, tw, th],
                    color: [0.12, 0.22, 0.16, 0.94],
                    mode: 3,
                    param: 5.0 * self.scale_factor,
                    _pad: [0.0; 2],
                });
                let max_chars = ((tw - 12.0 * self.scale_factor) / m.cell_width)
                    .floor()
                    .max(1.0) as usize;
                render_ui_string(
                    msg,
                    max_chars,
                    tx + 6.0 * self.scale_factor,
                    ty + ((th - m.cell_height) * 0.5).max(1.0) + m.baseline_y,
                    [0.75, 0.98, 0.80, 1.0],
                    &mut self.ui_shaper,
                    &self.atlas,
                    &mut self.batches.overlay_glyphs,
                );
            } else {
                self.status_toast = None;
            }
        }

        // Live Metal GPU + CPU Performance HUD (toggle via View -> Toggle Performance HUD / Cmd+Opt+I)
        if self.show_perf_hud {
            let m = self.ui_shaper.metrics();
            let (gpu_last, gpu_ewma, gpu_frames) = self.renderer.gpu_timing_ms();
            let pwr = if !self.window_focused {
                "30Hz BG"
            } else if self.low_power_mode {
                "60Hz LPM"
            } else {
                "120Hz"
            };
            let hud = format!(
                "PERF [{pwr}] | CPU {:.2}ms | GPU {:.2}ms (avg {:.2}ms) | Cache {}/{} | Atlas {} | #{}",
                self.last_cpu_prep_ms,
                gpu_last,
                gpu_ewma,
                self.batches.rows_reused,
                self.batches.rows_reused + self.batches.rows_rebuilt,
                self.atlas.entry_count(),
                gpu_frames,
            );
            let hw = ((hud.len() as f32 + 2.0) * m.cell_width).min(vp_w - 20.0);
            let hh = (FIND_BAR_HEIGHT_PT * self.scale_factor).max(m.cell_height + 6.0);
            let hx = 10.0 * self.scale_factor;
            let hy = vp_h - hh - 10.0 * self.scale_factor;
            self.batches.overlay_quads.push(QuadInstance {
                rect: [hx, hy, hw, hh],
                color: [0.08, 0.10, 0.14, 0.90],
                mode: 3,
                param: 5.0 * self.scale_factor,
                _pad: [0.0; 2],
            });
            let max_chars = ((hw - 12.0 * self.scale_factor) / m.cell_width)
                .floor()
                .max(1.0) as usize;
            render_ui_string(
                &hud,
                max_chars,
                hx + 6.0 * self.scale_factor,
                hy + ((hh - m.cell_height) * 0.5).max(1.0) + m.baseline_y,
                [0.55, 0.92, 1.0, 1.0],
                &mut self.ui_shaper,
                &self.atlas,
                &mut self.batches.overlay_glyphs,
            );
        }

        // Visual bell flash overlay
        if self.visual_bell_until.is_some() {
            self.batches.overlay_quads.push(QuadInstance {
                rect: [0.0, 0.0, vp_w, vp_h],
                color: [1.0, 1.0, 1.0, 0.12],
                mode: 0,
                param: 0.0,
                _pad: [0.0; 2],
            });
        }

        // Hotkey window subtle top accent border
        if self.is_hotkey_window {
            self.batches.overlay_quads.push(QuadInstance {
                rect: [0.0, 0.0, vp_w, (2.0 * self.scale_factor).max(2.0)],
                color: [0.51, 0.67, 1.0, 0.9],
                mode: 0,
                param: 0.0,
                _pad: [0.0; 2],
            });
        }
    }
}

/// Render inline IME preedit text right at the active terminal cursor with a crisp backdrop + underline.
#[allow(clippy::too_many_arguments)]
fn render_ime_preedit(
    ime: &ImeState,
    pane_rect: &PaneRect,
    cursor_col: usize,
    cursor_row: usize,
    profile: &Profile,
    scale_factor: f32,
    shaper: &mut CachedShaper<CoreTextShaper>,
    atlas: &GlyphAtlas,
    out_quads: &mut Vec<QuadInstance>,
    out_glyphs: &mut Vec<GlyphInstance>,
) {
    use unicode_width::UnicodeWidthChar;

    let m = shaper.metrics();
    let mut cell_cols: Vec<usize> = Vec::with_capacity(ime.marked_text.len());
    let mut total_cols: usize = 0;
    for ch in ime.marked_text.chars() {
        cell_cols.push(total_cols);
        total_cols += ch.width().unwrap_or(1).max(1);
    }
    if total_cols == 0 {
        return;
    }

    let box_w = (total_cols as f32 * m.cell_width).min(pane_rect.width);
    let mut origin_x = pane_rect.x + cursor_col as f32 * m.cell_width;
    if origin_x + box_w > pane_rect.x + pane_rect.width {
        origin_x = (pane_rect.x + pane_rect.width - box_w).max(pane_rect.x);
    }
    let origin_y = pane_rect.y + cursor_row as f32 * m.cell_height;

    let bg = profile.colors.background;
    let fg = profile.colors.foreground;
    let accent = profile.colors.cursor;

    // Subtle opaque backdrop so preedit text reads crisply over underlying cells
    out_quads.push(QuadInstance {
        rect: [origin_x, origin_y, box_w, m.cell_height],
        color: [
            (bg.r * 1.15 + 0.06).clamp(0.08, 0.95),
            (bg.g * 1.15 + 0.06).clamp(0.08, 0.95),
            (bg.b * 1.15 + 0.08).clamp(0.08, 0.95),
            0.98,
        ],
        mode: 0,
        param: 0.0,
        _pad: [0.0; 2],
    });
    // Crisp IME composition underline
    let ul_h = (2.0 * scale_factor).max(2.0);
    out_quads.push(QuadInstance {
        rect: [origin_x, origin_y + m.cell_height - ul_h, box_w, ul_h],
        color: [accent.r, accent.g, accent.b, 0.95],
        mode: 0,
        param: 0.0,
        _pad: [0.0; 2],
    });

    // Shape preedit text via CoreText (supports CJK / Hiragana / Hangul / combining marks / Emoji!)
    let shaped = shaper.shape_cached(&ime.marked_text, FontStyle::Regular, false);
    let fg_rgba = [fg.r, fg.g, fg.b, 1.0];
    for sg in shaped.iter() {
        let rel_col = cell_cols
            .get(sg.cluster_col as usize)
            .copied()
            .unwrap_or(sg.cluster_col as usize);
        let key = GlyphKey {
            font_id: sg.font_id,
            style: FontStyle::Regular,
            glyph_id: sg.glyph_id,
            size_qpt: m.size_qpt,
        };
        if let Some(entry) = shaper.resolve_glyph(atlas, key) {
            if entry.width > 0 && entry.height > 0 {
                let gx = (origin_x + rel_col as f32 * m.cell_width + sg.x_offset + entry.bearing_x)
                    .round();
                let gy = (origin_y + m.baseline_y - sg.y_offset - entry.bearing_y).round();
                out_glyphs.push(GlyphInstance {
                    dst_rect: [gx, gy, entry.width as f32, entry.height as f32],
                    uv_rect: entry.uv_rect,
                    fg_color: fg_rgba,
                    flags: u32::from(entry.is_color),
                    _pad: [0; 3],
                });
            }
        }
    }
}

pub(crate) fn get_local_hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).into_owned()
    } else {
        "localhost".into()
    }
}

pub(crate) fn word_bounds_at(line: &str, col: usize, word_chars: &str) -> (usize, usize) {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let c = col.min(chars.len() - 1);
    if !is_word_char(chars[c], word_chars) {
        return (c, c);
    }
    let mut start = c;
    while start > 0 && is_word_char(chars[start - 1], word_chars) {
        start -= 1;
    }
    let mut end = c;
    while end + 1 < chars.len() && is_word_char(chars[end + 1], word_chars) {
        end += 1;
    }
    (start, end)
}

pub(crate) fn extract_word_at(line: &str, col: usize, word_chars: &str) -> String {
    let (s, e) = word_bounds_at(line, col, word_chars);
    line.chars().skip(s).take(e.saturating_sub(s) + 1).collect()
}

pub(crate) fn make_ui_chrome_shaper(scale_factor: f32) -> CoreTextShaper {
    let mut ui_prof = Profile::default();
    ui_prof.normal_font.name = "SFMono-Regular".into();
    ui_prof.normal_font.size = 12.0;
    ui_prof.ascii_ligatures = false;
    ui_prof.use_non_ascii_font = false;
    ui_prof.horizontal_spacing = 1.0;
    ui_prof.vertical_spacing = 1.0;
    CoreTextShaper::from_profile(&ui_prof, scale_factor)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_ui_string(
    text: &str,
    max_chars: usize,
    start_x: f32,
    baseline_y: f32,
    color: [f32; 4],
    shaper: &mut CachedShaper<CoreTextShaper>,
    atlas: &GlyphAtlas,
    out_glyphs: &mut Vec<GlyphInstance>,
) {
    use unicode_width::UnicodeWidthChar;

    let m = shaper.metrics();
    if text.is_ascii() {
        for (idx, ch) in text.chars().take(max_chars).enumerate() {
            if ch == ' ' {
                continue;
            }
            if let Some((font_id, glyph_id)) = shaper.inner.ascii_glyph(FontStyle::Regular, ch as u8)
            {
                let key = GlyphKey {
                    font_id,
                    style: FontStyle::Regular,
                    glyph_id,
                    size_qpt: m.size_qpt,
                };
                if let Some(entry) = shaper.resolve_glyph(atlas, key) {
                    if entry.width > 0 && entry.height > 0 {
                        let gx = (start_x + idx as f32 * m.cell_width + entry.bearing_x).round();
                        let gy = (baseline_y - entry.bearing_y).round();
                        out_glyphs.push(GlyphInstance {
                            dst_rect: [gx, gy, entry.width as f32, entry.height as f32],
                            uv_rect: entry.uv_rect,
                            fg_color: color,
                            flags: u32::from(entry.is_color),
                            _pad: [0; 3],
                        });
                    }
                }
            }
        }
        return;
    }

    // Full Unicode CoreText shaping for non-ASCII tab titles, Find queries, and Toasts (CJK, emoji, em-dash!)
    let mut clipped = String::with_capacity(text.len().min(max_chars * 4));
    let mut char_cols: Vec<usize> = Vec::with_capacity(max_chars);
    let mut used_cols = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(1);
        if used_cols + w > max_chars {
            break;
        }
        clipped.push(ch);
        char_cols.push(used_cols);
        used_cols += w;
    }
    if clipped.is_empty() {
        return;
    }
    let shaped = shaper.shape_cached(&clipped, FontStyle::Regular, false);
    for sg in shaped.iter() {
        let col = char_cols
            .get(sg.cluster_col as usize)
            .copied()
            .unwrap_or(sg.cluster_col as usize);
        let key = GlyphKey {
            font_id: sg.font_id,
            style: FontStyle::Regular,
            glyph_id: sg.glyph_id,
            size_qpt: m.size_qpt,
        };
        if let Some(entry) = shaper.resolve_glyph(atlas, key) {
            if entry.width > 0 && entry.height > 0 {
                let gx = (start_x + col as f32 * m.cell_width + sg.x_offset + entry.bearing_x).round();
                let gy = (baseline_y - sg.y_offset - entry.bearing_y).round();
                out_glyphs.push(GlyphInstance {
                    dst_rect: [gx, gy, entry.width as f32, entry.height as f32],
                    uv_rect: entry.uv_rect,
                    fg_color: color,
                    flags: u32::from(entry.is_color),
                    _pad: [0; 3],
                });
            }
        }
    }
}
