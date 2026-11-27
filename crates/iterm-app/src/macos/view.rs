//! Native macOS `TerminalMetalView` (`NSView` subclass with `CAMetalLayer`,
//! `NSDraggingDestination`, and full `NSTextInputClient` IME support) plus `create_terminal_window`.

use crate::input_logic::{
    encode_mouse_event, resolve_key_event_with_kitty, KeyEventInput, UiCommand,
};
use crate::macos::chrome::{word_bounds_at, DIVIDER_WIDTH_PT};
use crate::macos::clipboard::{read_pasteboard_files, write_clipboard};
use crate::macos::coretext_shaper::CoreTextShaper;
use crate::macos::window::{DragDividerState, TerminalWindowState, TransferActionHit};
use iterm_profile::{modifiers, AppConfig, OptionKeyMode, Profile, SmartSelector};
use iterm_pty::SplitAxis;
use iterm_render::TextShaper;
use iterm_vt::{Selection, SelectionMode, SelectionPoint, TermModes};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingInfo, NSEvent, NSMenu, NSMenuItem,
    NSPasteboardTypeFileURL, NSTextInputClient, NSView, NSViewLayerContentsRedrawPolicy, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWindowTitleVisibility, NSWorkspace,
};
use objc2_core_graphics::CGColor;
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSNotFound, NSPoint, NSRange,
    NSRangePointer, NSRect, NSSize, NSString, NSUInteger,
};
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering;

/// macOS `NX_DEVICERALTKEYMASK` (IOKit / AppKit device-dependent Right Option bit).
const NX_DEVICERALTKEYMASK: usize = 0x0040;

pub struct TerminalViewIvars {
    pub state: RefCell<Option<TerminalWindowState>>,
    /// Set by `NSTextInputClient` callbacks (`insertText:` / `setMarkedText:`) during `interpretKeyEvents:`.
    ime_handled: Cell<bool>,
    /// Accumulated trackpad pinch-to-zoom delta (`magnifyWithEvent:`).
    magnify_accum: Cell<f64>,
}

/// Extract Rust UTF-8 string from Cocoa's `id` parameter (`NSString` or `NSAttributedString`).
fn cocoa_any_string_to_rust(obj: &AnyObject) -> String {
    if let Some(ns_str) = obj.downcast_ref::<NSString>() {
        ns_str.to_string()
    } else if let Some(attr) = obj.downcast_ref::<NSAttributedString>() {
        attr.string().to_string()
    } else {
        String::new()
    }
}

/// True for control/special non-IME keys that should bypass Cocoa text composition when no preedit is active.
fn is_non_ime_special_key(vk: u16, key_code_char: u32) -> bool {
    matches!(
        vk,
        36 | 48 | 51 | 53 | 76 | 115 | 116 | 117 | 119 | 121 | 123..=126
    ) || matches!(
        key_code_char,
        0x09 | 0x0D | 0x19 | 0x1B | 0x7F | 0xF700..=0xF746
    )
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TerminalViewIvars]
    pub struct TerminalMetalView;

    impl TerminalMetalView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        /// Prevent AppKit from wiping/clearing our `CAMetalLayer` to white on `Cmd+Tab` /
        /// activation / appearance transitions! Metal owns all drawing; keep the existing
        /// presented drawable intact and only paint if dirty.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let (vp_w, vp_h) = self.viewport_px();
            if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
                st.render_if_needed(vp_w, vp_h);
            }
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            self.ivars()
                .state
                .borrow()
                .as_ref()
                .is_some_and(|st| st.profile.transparency <= 0.001)
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, new_size: NSSize) {
            unsafe {
                let _: () = msg_send![super(self), setFrameSize: new_size];
            }
            if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
                let vp_w = (new_size.width as f32 * st.scale_factor).max(1.0);
                let vp_h = (new_size.height as f32 * st.scale_factor).max(1.0);
                st.layout_and_resize_ptys(vp_w, vp_h);
            }
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            let scale = self
                .window()
                .map(|w| w.backingScaleFactor() as f32)
                .unwrap_or(2.0);
            let frame = self.bounds();
            if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
                if (st.scale_factor - scale).abs() > 0.01 {
                    st.scale_factor = scale;
                    st.metal_layer.setContentsScale(scale as f64);
                    let vp_w = (frame.size.width as f32 * scale).max(1.0);
                    let vp_h = (frame.size.height as f32 * scale).max(1.0);
                    st.rebuild_fonts(vp_w, vp_h);
                }
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let (vp_w, vp_h) = self.viewport_px();
            let raw_flags = event.modifierFlags().bits();
            let mods = (raw_flags as u32) & modifiers::RELEVANT_MASK;
            let is_right_option =
                (mods & modifiers::OPTION) != 0 && (raw_flags & NX_DEVICERALTKEYMASK) != 0;
            let vk = event.keyCode();
            let chars = event.characters().map(|s| s.to_string()).unwrap_or_default();
            let ign = event
                .charactersIgnoringModifiers()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let key_code_char = ign.encode_utf16().next().map(|u| u as u32).unwrap_or(0);

            // Inspect minimal state flags without holding the RefCell borrow across `interpretKeyEvents:`!
            let (should_skip, has_marked, find_active, bypass_ime) = {
                let guard = self.ivars().state.borrow();
                let Some(ref st) = *guard else {
                    return;
                };
                if st.should_close || st.tabs.is_empty() {
                    return;
                }
                let has_marked = !st.ime.marked_text.is_empty();
                let active_opt_mode = if is_right_option {
                    st.profile.right_option_key_sends
                } else {
                    st.profile.option_key_sends
                };
                let opt_as_esc = (mods & modifiers::OPTION) != 0
                    && active_opt_mode != OptionKeyMode::Normal;
                let has_cmd_or_ctrl = (mods & (modifiers::COMMAND | modifiers::CONTROL)) != 0;
                let has_custom_binding = st
                    .profile
                    .keymap
                    .lookup(key_code_char, mods, Some(vk))
                    .is_some()
                    || st
                        .app_cfg
                        .global_keymap
                        .lookup(key_code_char, mods, Some(vk))
                        .is_some();
                let kitty_report_all = st
                    .tabs
                    .try_active_session()
                    .and_then(|sid| st.sessions.get(&sid))
                    .is_some_and(|s| (s.screen.lock().kitty_keyboard_flags() & 8) != 0);
                let bypass = has_cmd_or_ctrl
                    || opt_as_esc
                    || has_custom_binding
                    || kitty_report_all
                    || is_non_ime_special_key(vk, key_code_char);
                (false, has_marked, st.find.active, bypass)
            };
            if should_skip {
                return;
            }

            // 1. Active IME composition OR regular text typing (CJK, dead keys, accents):
            //    delegate to AppKit `interpretKeyEvents:` first so Cocoa IME handles composition cleanly!
            if has_marked || (!bypass_ime && !find_active) {
                self.ivars().ime_handled.set(false);
                let arr = NSArray::from_slice(&[event]);
                self.interpretKeyEvents(&arr);
                if self.ivars().ime_handled.get() {
                    return;
                }
            }

            // 2. Inline Find bar active (with no IME preedit in progress)
            if find_active && !has_marked {
                if !bypass_ime {
                    self.ivars().ime_handled.set(false);
                    let arr = NSArray::from_slice(&[event]);
                    self.interpretKeyEvents(&arr);
                    if self.ivars().ime_handled.get() {
                        return;
                    }
                }
                let mut guard = self.ivars().state.borrow_mut();
                if let Some(ref mut st) = *guard {
                    if vk == 53 {
                        st.handle_command(UiCommand::ToggleFind, vp_w, vp_h);
                        st.render_if_needed(vp_w, vp_h);
                        return;
                    } else if vk == 36 {
                        let shift = (mods & modifiers::SHIFT) != 0;
                        st.handle_command(
                            if shift {
                                UiCommand::FindPrev
                            } else {
                                UiCommand::FindNext
                            },
                            vp_w,
                            vp_h,
                        );
                        st.render_if_needed(vp_w, vp_h);
                        return;
                    } else if vk == 51 {
                        st.find.query.pop();
                        if let Some(sid) = st.tabs.try_active_session() {
                            if let Some(sess) = st.sessions.get(&sid) {
                                st.find.match_count = sess.screen.lock().search(
                                    &st.find.query,
                                    st.find.regex,
                                    st.find.ignore_case,
                                );
                            }
                        }
                        st.wake_flag.store(true, Ordering::Release);
                        st.render_if_needed(vp_w, vp_h);
                        return;
                    } else if (mods & modifiers::COMMAND) == 0
                        && !chars.is_empty()
                        && !chars.chars().all(|c| c.is_control())
                    {
                        st.find.query.push_str(&chars);
                        if let Some(sid) = st.tabs.try_active_session() {
                            if let Some(sess) = st.sessions.get(&sid) {
                                st.find.match_count = sess.screen.lock().search(
                                    &st.find.query,
                                    st.find.regex,
                                    st.find.ignore_case,
                                );
                            }
                        }
                        st.wake_flag.store(true, Ordering::Release);
                        st.render_if_needed(vp_w, vp_h);
                        return;
                    }
                }
            }

            // 3. Direct terminal / iTerm2 keymap / Kitty keyboard protocol dispatch
            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return;
            };
            if st.should_close || st.tabs.is_empty() {
                return;
            }
            let active_id = st.tabs.active_session();
            let (modes, kitty_flags) = st
                .sessions
                .get(&active_id)
                .map(|s| {
                    let scr = s.screen.lock();
                    (scr.modes, scr.kitty_keyboard_flags())
                })
                .unwrap_or((TermModes::default(), 0));

            let input = KeyEventInput {
                key_code_char,
                characters: chars,
                virtual_key: vk,
                mods,
                is_right_option,
            };
            let cmd =
                resolve_key_event_with_kitty(&input, &st.app_cfg, &st.profile, modes, kitty_flags);
            if cmd == UiCommand::ToggleFullscreen {
                drop(guard);
                if let Some(w) = self.window() {
                    w.toggleFullScreen(None);
                }
                return;
            }
            if cmd == UiCommand::MinimizeWindow {
                drop(guard);
                if let Some(w) = self.window() {
                    w.performMiniaturize(None);
                }
                return;
            }
            st.handle_command(cmd, vp_w, vp_h);
            st.render_if_needed(vp_w, vp_h);
            let opacity_sync = if std::mem::take(&mut st.window_opacity_dirty) {
                Some((st.profile.transparency <= 0.001, st.profile.colors.background))
            } else {
                None
            };
            let need_app_tick = st.request_open_prefs || st.request_new_window || st.should_close;
            let next_wake = st.next_scheduled_wakeup();
            drop(guard);
            if let Some((is_opaque, bg)) = opacity_sync {
                self.apply_window_opacity(is_opaque, bg);
            }
            if need_app_tick {
                crate::macos::app::schedule_main_wakeup();
            } else if let Some(d) = next_wake {
                crate::macos::app::schedule_main_wakeup_after(d);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let (px, py) = self.event_px(event);
            let (vp_w, vp_h) = self.viewport_px();
            let mods = (event.modifierFlags().bits() as u32) & modifiers::RELEVANT_MASK;
            let clicks = event.clickCount();

            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return;
            };
            if st.should_close || st.tabs.is_empty() {
                return;
            }
            st.ime.clear();

            // Transfer Queue overlay buttons ([x] cancel, Clear, Dismiss)
            for hb in &st.transfer_hitboxes {
                let r = hb.rect;
                if px >= r[0] && px <= r[0] + r[2] && py >= r[1] && py <= r[1] + r[3] {
                    match hb.action {
                        TransferActionHit::Cancel(id) => {
                            st.transfers.cancel(id);
                        }
                        TransferActionHit::ClearFinished => {
                            st.transfers.clear_finished();
                            if st.transfers.active_count() == 0 {
                                st.show_transfers = false;
                            }
                        }
                        TransferActionHit::Dismiss => {
                            st.show_transfers = false;
                            st.transfers.clear_finished();
                        }
                    }
                    st.wake_flag.store(true, Ordering::Release);
                    st.render_if_needed(vp_w, vp_h);
                    return;
                }
            }

            // Unified titlebar/tab bar interaction (level with red/yellow/green traffic lights!)
            let tab_h = st.tab_bar_height_px();
            if tab_h > 0.0 && py < tab_h {
                let (strip_left, strip_right, _) = st.tab_strip_bounds_px(vp_w);
                if px >= strip_right {
                    st.handle_command(UiCommand::NewTab, vp_w, vp_h);
                    st.render_if_needed(vp_w, vp_h);
                    return;
                }
                let n = st.tabs.tabs.len();
                let show_pills = !st.app_cfg.hide_tab_bar_when_single || n > 1;
                if show_pills && n > 1 && px >= strip_left && px < strip_right {
                    let strip_w = (strip_right - strip_left).max(1.0);
                    let idx = (((px - strip_left) / strip_w) * n as f32).floor() as usize;
                    let idx = idx.min(n - 1);
                    if idx != st.tabs.active_index {
                        let prev_sid = st.tabs.active_session();
                        st.tabs.select_tab(idx);
                        st.layout_and_resize_ptys(vp_w, vp_h);
                        st.notify_pane_focus_transition(prev_sid);
                        st.render_if_needed(vp_w, vp_h);
                        return;
                    }
                }
                drop(guard);
                if let Some(w) = self.window() {
                    w.performWindowDragWithEvent(event);
                }
                return;
            }

            let bounds = st.pane_area_rect(vp_w, vp_h);
            let div_px = (DIVIDER_WIDTH_PT * st.scale_factor).max(1.0);
            let (panes, dividers) = st.tabs.active_tab().panes.layout(bounds, div_px);

            let tol = 4.0 * st.scale_factor;
            for div in dividers {
                let hit = px >= div.rect.x - tol
                    && px <= div.rect.x + div.rect.width + tol
                    && py >= div.rect.y - tol
                    && py <= div.rect.y + div.rect.height + tol;
                if hit {
                    st.drag_divider = Some(DragDividerState {
                        split_id: div.split_id,
                        axis: div.axis,
                    });
                    return;
                }
            }

            let Some((sid, rect)) = panes.into_iter().find(|(_, r)| r.contains(px, py)) else {
                return;
            };
            let focus_changed = st.focus_pane_session(sid);

            let m = st.shaper.metrics();
            let col = (((px - rect.x) / m.cell_width).floor() as isize).max(0) as usize;
            let row = (((py - rect.y) / m.cell_height).floor() as isize).max(0) as usize;

            if (mods & modifiers::COMMAND) != 0 {
                st.handle_semantic_cmd_click(sid, row, col);
                if focus_changed {
                    st.render_if_needed(vp_w, vp_h);
                }
                return;
            }

            if st.profile.mouse_reporting && (mods & modifiers::OPTION) == 0 {
                if let Some(sess) = st.sessions.get(&sid) {
                    let modes = sess.screen.lock().modes;
                    if let Some(seq) = encode_mouse_event(modes, 0, true, false, col, row, mods) {
                        let _ = sess.write_input(&seq);
                        if focus_changed {
                            st.render_if_needed(vp_w, vp_h);
                        }
                        return;
                    }
                }
            }

            if let Some(sess) = st.sessions.get(&sid) {
                let mut s = sess.screen.lock();
                let abs_line = s.visible_abs_line(row.min(s.rows().saturating_sub(1)));
                let col_clamped = col.min(s.cols().saturating_sub(1));
                if clicks == 2 {
                    let mut scratch = Vec::new();
                    let (cells, _, _, _, _) =
                        s.read_visible_row(row.min(s.rows() - 1), &mut scratch);
                    let line = iterm_vt::row_cells_to_string(cells, false);
                    let (c0, c1) = word_bounds_at(&line, col_clamped, &st.app_cfg.word_characters);
                    s.selection = Some(Selection {
                        start: SelectionPoint { abs_line, col: c0 },
                        end: SelectionPoint { abs_line, col: c1 },
                        mode: SelectionMode::Word,
                    });
                    st.selection_anchor = None;
                } else if clicks >= 3 {
                    let max_c = s.cols().saturating_sub(1);
                    s.selection = Some(Selection {
                        start: SelectionPoint { abs_line, col: 0 },
                        end: SelectionPoint {
                            abs_line,
                            col: max_c,
                        },
                        mode: SelectionMode::Line,
                    });
                    st.selection_anchor = None;
                } else {
                    let mode = if (mods & modifiers::OPTION) != 0 {
                        SelectionMode::Block
                    } else {
                        SelectionMode::Normal
                    };
                    let pt = SelectionPoint {
                        abs_line,
                        col: col_clamped,
                    };
                    s.selection = None;
                    st.selection_anchor = Some((pt, mode));
                }
                drop(s);
                st.selecting = true;
                st.wake_flag.store(true, Ordering::Release);
            }
            st.render_if_needed(vp_w, vp_h);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let (px, py) = self.event_px(event);
            let (vp_w, vp_h) = self.viewport_px();
            let mods = (event.modifierFlags().bits() as u32) & modifiers::RELEVANT_MASK;

            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return;
            };
            if st.should_close || st.tabs.is_empty() {
                return;
            }

            if let Some(ref drag) = st.drag_divider {
                let bounds = st.pane_area_rect(vp_w, vp_h);
                let ratio = match drag.axis {
                    SplitAxis::Vertical => (px - bounds.x) / bounds.width.max(1.0),
                    SplitAxis::Horizontal => (py - bounds.y) / bounds.height.max(1.0),
                };
                let split_id = drag.split_id;
                st.tabs.active_tab_mut().panes.set_split_ratio(split_id, ratio);
                st.layout_and_resize_ptys(vp_w, vp_h);
                st.render_if_needed(vp_w, vp_h);
                return;
            }

            let active_id = st.tabs.active_session();
            let bounds = st.pane_area_rect(vp_w, vp_h);
            let (panes, _) = st.tabs.active_tab().panes.layout(bounds, 1.0);
            let Some((_, rect)) = panes.into_iter().find(|(id, _)| *id == active_id) else {
                return;
            };
            let m = st.shaper.metrics();
            let col = (((px - rect.x) / m.cell_width).floor() as isize).max(0) as usize;
            let row = (((py - rect.y) / m.cell_height).floor() as isize).max(0) as usize;

            if !st.selecting && st.profile.mouse_reporting {
                if let Some(sess) = st.sessions.get(&active_id) {
                    let modes = sess.screen.lock().modes;
                    if let Some(seq) = encode_mouse_event(modes, 0, true, true, col, row, mods) {
                        let _ = sess.write_input(&seq);
                        return;
                    }
                }
            }

            if st.selecting {
                if let Some(sess) = st.sessions.get(&active_id) {
                    let mut s = sess.screen.lock();
                    let r_clamped = row.min(s.rows().saturating_sub(1));
                    let c_clamped = col.min(s.cols().saturating_sub(1));
                    let abs_line = s.visible_abs_line(r_clamped);
                    let cur_pt = SelectionPoint {
                        abs_line,
                        col: c_clamped,
                    };
                    if let Some(ref mut sel) = s.selection {
                        sel.end = cur_pt;
                    } else if let Some((anchor, mode)) = st.selection_anchor {
                        if cur_pt != anchor {
                            s.selection = Some(Selection {
                                start: anchor,
                                end: cur_pt,
                                mode,
                            });
                        }
                    }
                    drop(s);
                    st.wake_flag.store(true, Ordering::Release);
                }
                st.render_if_needed(vp_w, vp_h);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let (px, py) = self.event_px(event);
            let (vp_w, vp_h) = self.viewport_px();
            let mods = (event.modifierFlags().bits() as u32) & modifiers::RELEVANT_MASK;

            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return;
            };
            if st.should_close || st.tabs.is_empty() {
                return;
            }

            if st.drag_divider.take().is_some() {
                return;
            }

            let active_id = st.tabs.active_session();
            if st.selecting {
                st.selecting = false;
                st.selection_anchor = None;
                if st.app_cfg.copy_selection {
                    if let Some(sess) = st.sessions.get(&active_id) {
                        let s = sess.screen.lock();
                        if let Some(sel) = s.selection {
                            if sel.start != sel.end || sel.mode != SelectionMode::Normal {
                                if let Some(text) = s.extract_selection_text() {
                                    if !text.is_empty() {
                                        write_clipboard(&text);
                                    }
                                }
                            }
                        }
                    }
                }
                return;
            }

            if st.profile.mouse_reporting {
                let bounds = st.pane_area_rect(vp_w, vp_h);
                let (panes, _) = st.tabs.active_tab().panes.layout(bounds, 1.0);
                if let Some((_, rect)) = panes.into_iter().find(|(id, _)| *id == active_id) {
                    let m = st.shaper.metrics();
                    let col = (((px - rect.x) / m.cell_width).floor() as isize).max(0) as usize;
                    let row = (((py - rect.y) / m.cell_height).floor() as isize).max(0) as usize;
                    if let Some(sess) = st.sessions.get(&active_id) {
                        let modes = sess.screen.lock().modes;
                        if let Some(seq) = encode_mouse_event(modes, 0, false, false, col, row, mods)
                        {
                            let _ = sess.write_input(&seq);
                        }
                    }
                }
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (px, py) = self.event_px(event);
            let (vp_w, vp_h) = self.viewport_px();
            let mods = (event.modifierFlags().bits() as u32) & modifiers::RELEVANT_MASK;
            let dy = event.scrollingDeltaY();

            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return;
            };
            if st.should_close || st.tabs.is_empty() {
                return;
            }

            let step = if event.hasPreciseScrollingDeltas() {
                st.scroll_accum_y += dy;
                let lines = (st.scroll_accum_y / 14.0).trunc() as isize;
                st.scroll_accum_y -= lines as f64 * 14.0;
                lines
            } else {
                (dy * 3.0).round() as isize
            };
            if step == 0 {
                return;
            }

            let bounds = st.pane_area_rect(vp_w, vp_h);
            let target_sid = st
                .tabs
                .active_tab()
                .panes
                .pane_at(px, py, bounds, 1.0)
                .unwrap_or_else(|| st.tabs.active_session());

            let (panes, _) = st.tabs.active_tab().panes.layout(bounds, 1.0);
            let rect = panes
                .into_iter()
                .find(|(id, _)| *id == target_sid)
                .map(|(_, r)| r)
                .unwrap_or(bounds);
            let m = st.shaper.metrics();
            let col = (((px - rect.x) / m.cell_width).floor() as isize).max(0) as usize;
            let row = (((py - rect.y) / m.cell_height).floor() as isize).max(0) as usize;

            if let Some(sess) = st.sessions.get(&target_sid) {
                let modes = sess.screen.lock().modes;
                let btn = if step > 0 { 64u8 } else { 65u8 };
                let count = step.unsigned_abs().min(10);
                if st.profile.mouse_reporting
                    && modes.intersects(
                        TermModes::MOUSE_VT200 | TermModes::MOUSE_BUTTON | TermModes::MOUSE_ANY,
                    )
                {
                    for _ in 0..count {
                        if let Some(seq) = encode_mouse_event(modes, btn, true, false, col, row, mods)
                        {
                            let _ = sess.write_input(&seq);
                        }
                    }
                    return;
                }
                if modes.contains(TermModes::ALT_SCREEN) && modes.contains(TermModes::ALT_SCROLL) {
                    let seq = if step > 0 { b"\x1b[A" } else { b"\x1b[B" };
                    for _ in 0..count {
                        let _ = sess.write_input(seq);
                    }
                    return;
                }
                sess.screen.lock().scroll_viewport(step);
                st.wake_flag.store(true, Ordering::Release);
            }
            st.render_if_needed(vp_w, vp_h);
        }

        // ---- Native macOS right-click context menu & trackpad pinch-to-zoom ----
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            // If mouse reporting is active and Option isn't held, forward right-click (button 2) to TUI!
            let (px, py) = self.event_px(event);
            let (vp_w, vp_h) = self.viewport_px();
            let mods = (event.modifierFlags().bits() as u32) & modifiers::RELEVANT_MASK;
            {
                let mut guard = self.ivars().state.borrow_mut();
                if let Some(ref mut st) = *guard {
                    if !st.should_close && !st.tabs.is_empty() && st.profile.mouse_reporting && (mods & modifiers::OPTION) == 0 {
                        let bounds = st.pane_area_rect(vp_w, vp_h);
                        let div_px = (DIVIDER_WIDTH_PT * st.scale_factor).max(1.0);
                        let (panes, _) = st.tabs.active_tab().panes.layout(bounds, div_px);
                        if let Some((sid, rect)) = panes.into_iter().find(|(_, r)| r.contains(px, py)) {
                            let focus_changed = st.focus_pane_session(sid);
                            let m = st.shaper.metrics();
                            let col = (((px - rect.x) / m.cell_width).floor() as isize).max(0) as usize;
                            let row = (((py - rect.y) / m.cell_height).floor() as isize).max(0) as usize;
                            if let Some(sess) = st.sessions.get(&sid) {
                                let modes = sess.screen.lock().modes;
                                if let Some(seq) = encode_mouse_event(modes, 2, true, false, col, row, mods) {
                                    let _ = sess.write_input(&seq);
                                    if focus_changed {
                                        st.render_if_needed(vp_w, vp_h);
                                    }
                                    return;
                                }
                            }
                        }
                    }
                }
            }

            let mtm = MainThreadMarker::from(self);
            let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Context"));
            let add_ctx = |title: &str, action: Sel| unsafe {
                let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                    mtm.alloc(),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::from_str(""),
                );
                item.setTarget(Some(self));
                menu.addItem(&item);
            };
            add_ctx("Copy", sel!(ctxCopy:));
            add_ctx("Paste", sel!(ctxPaste:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add_ctx("Split Vertically", sel!(ctxSplitVertical:));
            add_ctx("Split Horizontally", sel!(ctxSplitHorizontal:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add_ctx("Clear Buffer", sel!(ctxClearBuffer:));
            add_ctx("Use Transparency", sel!(ctxToggleTransparency:));
            add_ctx("Transfers", sel!(ctxToggleTransfers:));
            add_ctx("Toggle Performance HUD", sel!(ctxTogglePerfHud:));
            add_ctx("Settings…", sel!(ctxOpenPreferences:));
            NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
        }

        #[unsafe(method(ctxCopy:))]
        fn ctx_copy(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::Copy);
        }

        #[unsafe(method(ctxPaste:))]
        fn ctx_paste(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::Paste);
        }

        #[unsafe(method(ctxSplitVertical:))]
        fn ctx_split_vertical(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::SplitVertical);
        }

        #[unsafe(method(ctxSplitHorizontal:))]
        fn ctx_split_horizontal(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::SplitHorizontal);
        }

        #[unsafe(method(ctxClearBuffer:))]
        fn ctx_clear_buffer(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::ClearBuffer);
        }

        #[unsafe(method(ctxToggleTransparency:))]
        fn ctx_toggle_transparency(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::ToggleTransparency);
        }

        #[unsafe(method(ctxToggleTransfers:))]
        fn ctx_toggle_transfers(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::ToggleTransfers);
        }

        #[unsafe(method(ctxTogglePerfHud:))]
        fn ctx_toggle_perf_hud(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::TogglePerfHud);
        }

        #[unsafe(method(ctxOpenPreferences:))]
        fn ctx_open_preferences(&self, _sender: Option<&AnyObject>) {
            self.send_command(UiCommand::OpenPreferences);
        }

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify_with_event(&self, event: &NSEvent) {
            let mut acc = self.ivars().magnify_accum.get() + event.magnification();
            if acc >= 0.08 {
                acc = 0.0;
                self.send_command(UiCommand::FontIncrease);
            } else if acc <= -0.08 {
                acc = 0.0;
                self.send_command(UiCommand::FontDecrease);
            }
            self.ivars().magnify_accum.set(acc);
        }

        // ---- Drag-and-drop files onto terminal view (local path paste or SSH SCP upload) ----
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let pb = sender.draggingPasteboard();
            let paths = read_pasteboard_files(&pb);
            if paths.is_empty() {
                false
            } else if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
                if let Some(sid) = st.tabs.try_active_session() {
                    st.handle_local_files_attach(sid, paths);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
    }

    // ---- Native macOS NSTextInputClient (CJK IME, dead keys, Emoji picker, Dictation) ----
    unsafe impl NSTextInputClient for TerminalMetalView {
        #[unsafe(method(insertText:replacementRange:))]
        unsafe fn insert_text_replacement_range(
            &self,
            string: &AnyObject,
            _replacement_range: NSRange,
        ) {
            let text = cocoa_any_string_to_rust(string);
            let (vp_w, vp_h) = self.viewport_px();
            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return;
            };
            st.ime.clear();
            self.ivars().ime_handled.set(true);
            if text.is_empty() || st.should_close || st.tabs.is_empty() {
                st.wake_flag.store(true, Ordering::Release);
                st.render_if_needed(vp_w, vp_h);
                return;
            }
            if st.find.active {
                st.find.query.push_str(&text);
                if let Some(sid) = st.tabs.try_active_session() {
                    if let Some(sess) = st.sessions.get(&sid) {
                        st.find.match_count = sess.screen.lock().search(
                            &st.find.query,
                            st.find.regex,
                            st.find.ignore_case,
                        );
                    }
                }
                st.wake_flag.store(true, Ordering::Release);
                st.render_if_needed(vp_w, vp_h);
                return;
            }
            st.handle_command(UiCommand::SendBytes(text.into_bytes()), vp_w, vp_h);
            st.render_if_needed(vp_w, vp_h);
        }

        #[unsafe(method(doCommandBySelector:))]
        unsafe fn do_command_by_selector(&self, _selector: Sel) {
            // Intentionally leave `ime_handled = false` so `keyDown:` falls through to our
            // terminal / iTerm2 keymap / Kitty keyboard protocol encoder!
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        unsafe fn set_marked_text_selected_range_replacement_range(
            &self,
            string: &AnyObject,
            selected_range: NSRange,
            _replacement_range: NSRange,
        ) {
            let text = cocoa_any_string_to_rust(string);
            let (vp_w, vp_h) = self.viewport_px();
            if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
                st.ime.marked_text = text;
                st.ime.selected_range = (selected_range.location, selected_range.length);
                st.wake_flag.store(true, Ordering::Release);
                st.render_if_needed(vp_w, vp_h);
            }
            self.ivars().ime_handled.set(true);
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let (vp_w, vp_h) = self.viewport_px();
            if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
                if !st.ime.marked_text.is_empty() {
                    st.ime.clear();
                    st.wake_flag.store(true, Ordering::Release);
                    st.render_if_needed(vp_w, vp_h);
                }
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            if let Some(ref st) = *self.ivars().state.borrow() {
                if !st.ime.marked_text.is_empty() {
                    return NSRange {
                        location: st.ime.selected_range.0,
                        length: st.ime.selected_range.1,
                    };
                }
            }
            NSRange {
                location: NSNotFound as NSUInteger,
                length: 0,
            }
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            if let Some(ref st) = *self.ivars().state.borrow() {
                let utf16_len = st.ime.marked_text.encode_utf16().count();
                if utf16_len > 0 {
                    return NSRange {
                        location: 0,
                        length: utf16_len,
                    };
                }
            }
            NSRange {
                location: NSNotFound as NSUInteger,
                length: 0,
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars()
                .state
                .borrow()
                .as_ref()
                .is_some_and(|st| !st.ime.marked_text.is_empty())
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        unsafe fn attributed_substring_for_proposed_range_actual_range(
            &self,
            _range: NSRange,
            _actual_range: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes_for_marked_text(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        unsafe fn first_rect_for_character_range_actual_range(
            &self,
            _range: NSRange,
            _actual_range: NSRangePointer,
        ) -> NSRect {
            let (vp_w, vp_h) = self.viewport_px();
            let (cx_pt, cy_pt, cw_pt, ch_pt) = {
                let guard = self.ivars().state.borrow();
                if let Some(ref st) = *guard {
                    let s = st.scale_factor.max(1.0);
                    let (cx, cy, cw, ch) = st
                        .active_cursor_rect_px(vp_w, vp_h)
                        .unwrap_or((0.0, 0.0, 12.0 * s, 20.0 * s));
                    (
                        (cx / s) as f64,
                        (cy / s) as f64,
                        (cw / s) as f64,
                        (ch / s) as f64,
                    )
                } else {
                    (0.0, 0.0, 12.0, 20.0)
                }
            };
            let view_rect = NSRect::new(NSPoint::new(cx_pt, cy_pt), NSSize::new(cw_pt, ch_pt));
            let win_rect = self.convertRect_toView(view_rect, None);
            if let Some(win) = self.window() {
                win.convertRectToScreen(win_rect)
            } else {
                win_rect
            }
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            0
        }
    }
);

impl TerminalMetalView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        app_cfg: AppConfig,
        profile: Profile,
        scale_factor: f32,
        is_hotkey_window: bool,
        reduce_transparency: bool,
    ) -> Result<Retained<Self>, String> {
        let this = mtm.alloc::<Self>().set_ivars(TerminalViewIvars {
            state: RefCell::new(None),
            ime_handled: Cell::new(false),
            magnify_accum: Cell::new(0.0),
        });
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };

        let drag_types = unsafe { NSArray::from_slice(&[NSPasteboardTypeFileURL]) };
        view.registerForDraggedTypes(&drag_types);

        let metal_layer = CAMetalLayer::new();
        metal_layer.setContentsScale(scale_factor as f64);
        metal_layer.setNeedsDisplayOnBoundsChange(false);
        let bg = profile.colors.background;
        let is_opaque = reduce_transparency || profile.transparency <= 0.001;
        let layer_alpha = if is_opaque {
            1.0
        } else {
            (1.0 - profile.transparency).clamp(0.05, 1.0) as f64
        };
        let cg_bg = CGColor::new_srgb(bg.r as f64, bg.g as f64, bg.b as f64, layer_alpha);
        metal_layer.setBackgroundColor(Some(&cg_bg));
        // Set layer BEFORE wantsLayer=true to configure a proper AppKit layer-hosting NSView,
        // and disable AppKit automatic redraw wipes on Cmd+Tab / focus changes!
        view.setLayer(Some(&metal_layer));
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);

        let state = TerminalWindowState::new(
            app_cfg,
            profile,
            scale_factor,
            is_hotkey_window,
            reduce_transparency,
            metal_layer,
        )?;
        *view.ivars().state.borrow_mut() = Some(state);
        Ok(view)
    }

    pub fn viewport_px(&self) -> (f32, f32) {
        let b = self.bounds();
        let scale = self
            .ivars()
            .state
            .borrow()
            .as_ref()
            .map(|s| s.scale_factor)
            .unwrap_or(2.0);
        (
            (b.size.width as f32 * scale).max(1.0),
            (b.size.height as f32 * scale).max(1.0),
        )
    }

    fn event_px(&self, event: &NSEvent) -> (f32, f32) {
        let loc_in_win = event.locationInWindow();
        let local: NSPoint = self.convertPoint_fromView(loc_in_win, None);
        let scale = self
            .ivars()
            .state
            .borrow()
            .as_ref()
            .map(|s| s.scale_factor)
            .unwrap_or(2.0);
        (local.x as f32 * scale, local.y as f32 * scale)
    }

    pub fn tick(
        &self,
        is_visible: bool,
        low_power: bool,
        reduce_transparency: bool,
    ) -> (bool, bool, bool, Option<String>, Option<std::time::Duration>) {
        let is_key = self.window().is_some_and(|w| w.isKeyWindow());
        let (vp_w, vp_h) = self.viewport_px();
        let mut opacity_sync = None;
        let res = {
            let mut guard = self.ivars().state.borrow_mut();
            let Some(ref mut st) = *guard else {
                return (false, false, false, None, None);
            };
            if st.should_close || st.tabs.is_empty() {
                let req_new = std::mem::take(&mut st.request_new_window);
                let req_prefs = std::mem::take(&mut st.request_open_prefs);
                return (true, req_new, req_prefs, None, None);
            }
            st.set_power_visibility_state(is_visible, low_power, reduce_transparency);
            st.set_window_focused(is_key);
            if std::mem::take(&mut st.window_opacity_dirty) {
                opacity_sync = Some((st.profile.transparency <= 0.001, st.profile.colors.background));
            }
            let title = st.process_session_events(vp_w, vp_h);
            if is_visible && !st.should_close && !st.tabs.is_empty() {
                st.render_if_needed(vp_w, vp_h);
            }
            let next_wake = st.next_scheduled_wakeup();
            let req_new = std::mem::take(&mut st.request_new_window);
            let req_prefs = std::mem::take(&mut st.request_open_prefs);
            (
                st.should_close || st.tabs.is_empty(),
                req_new,
                req_prefs,
                title,
                next_wake,
            )
        };
        if let Some((is_opaque, bg)) = opacity_sync {
            self.apply_window_opacity(is_opaque, bg);
        }
        res
    }

    /// Live-apply updated `AppConfig` & profile from the Preferences panel to this window!
    pub fn apply_config_update(&self, new_cfg: AppConfig) {
        let reduce_transparency = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency();
        let (vp_w, vp_h) = self.viewport_px();
        let mut guard = self.ivars().state.borrow_mut();
        let Some(ref mut st) = *guard else {
            return;
        };
        let mut new_prof = new_cfg.default_profile().clone();
        if new_prof.transparency > 0.001 {
            st.saved_transparency = new_prof.transparency;
        }
        st.reduce_transparency = reduce_transparency;
        st.manual_transparency_override = None;
        if reduce_transparency {
            new_prof.transparency = 0.0;
        }
        st.app_cfg = new_cfg;
        st.profile = new_prof.clone();
        st.font_size_delta = 0.0;
        st.smart_selector = SmartSelector::new(&new_prof.smart_selection_rules);
        st.renderer
            .configure_layer(&st.metal_layer, new_prof.transparency <= 0.001);
        let to_rgb = |c: iterm_profile::Rgba| -> (u8, u8, u8) {
            (
                (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            )
        };
        for sess in st.sessions.values_mut() {
            sess.profile = new_prof.clone();
            let mut s = sess.screen.lock();
            s.cursor.style = new_prof.cursor_style;
            s.cursor.blinking = new_prof.blinking_cursor;
            s.default_fg_rgb = to_rgb(new_prof.colors.foreground);
            s.default_bg_rgb = to_rgb(new_prof.colors.background);
            s.default_cursor_rgb = to_rgb(new_prof.colors.cursor);
            s.scrollback.set_max_lines(if new_prof.unlimited_scrollback {
                1_000_000
            } else {
                new_prof.scrollback_lines
            });
        }
        st.rebuild_fonts(vp_w, vp_h);
        let is_opaque = new_prof.transparency <= 0.001;
        let bg = new_prof.colors.background;
        drop(guard);

        self.apply_window_opacity(is_opaque, bg);
    }

    fn apply_window_opacity(&self, is_opaque: bool, bg: iterm_profile::Rgba) {
        // Sync CAMetalLayer CGColor background so bare layer never flashes white on Cmd+Tab / resize!
        if let Some(ref st) = *self.ivars().state.borrow() {
            let alpha = if is_opaque {
                1.0
            } else {
                (1.0 - st.profile.transparency).clamp(0.05, 1.0) as f64
            };
            let cg_bg = CGColor::new_srgb(bg.r as f64, bg.g as f64, bg.b as f64, alpha);
            st.metal_layer.setBackgroundColor(Some(&cg_bg));
        }
        if let Some(win) = self.window() {
            apply_window_appearance_and_bg(&win, is_opaque, bg);
            win.invalidateShadow();
        }
    }

    pub fn send_command(&self, cmd: UiCommand) {
        let (vp_w, vp_h) = self.viewport_px();
        let mut need_app_tick = false;
        let mut opacity_sync = None;
        let mut next_wake = None;
        if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
            st.handle_command(cmd, vp_w, vp_h);
            st.render_if_needed(vp_w, vp_h);
            if std::mem::take(&mut st.window_opacity_dirty) {
                opacity_sync = Some((st.profile.transparency <= 0.001, st.profile.colors.background));
            }
            need_app_tick = st.request_open_prefs || st.request_new_window || st.should_close;
            next_wake = st.next_scheduled_wakeup();
        }
        if let Some((is_opaque, bg)) = opacity_sync {
            self.apply_window_opacity(is_opaque, bg);
        }
        if need_app_tick {
            crate::macos::app::schedule_main_wakeup();
        } else if let Some(d) = next_wake {
            crate::macos::app::schedule_main_wakeup_after(d);
        }
    }

    pub fn show_toast(&self, msg: String) {
        let (vp_w, vp_h) = self.viewport_px();
        let mut next_wake = None;
        if let Some(ref mut st) = *self.ivars().state.borrow_mut() {
            st.status_toast = Some((msg, std::time::Instant::now() + std::time::Duration::from_secs(5)));
            st.wake_flag.store(true, Ordering::Release);
            st.render_if_needed(vp_w, vp_h);
            next_wake = st.next_scheduled_wakeup();
        }
        if let Some(d) = next_wake {
            crate::macos::app::schedule_main_wakeup_after(d);
        }
    }
}

/// Apply matching Dark/Light `NSAppearance` and opaque/clear window background color so macOS
/// `NSThemeFrame` & titlebar NEVER flash white during `Cmd+Tab` app switches!
fn apply_window_appearance_and_bg(window: &NSWindow, is_opaque: bool, bg: iterm_profile::Rgba) {
    let lum = 0.2126 * bg.r + 0.7152 * bg.g + 0.0722 * bg.b;
    let appearance_name = unsafe {
        if lum < 0.5 {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    if let Some(app) = NSAppearance::appearanceNamed(appearance_name) {
        window.setAppearance(Some(&app));
    }
    window.setOpaque(is_opaque);
    if is_opaque {
        let ns_bg = NSColor::colorWithSRGBRed_green_blue_alpha(
            bg.r as f64,
            bg.g as f64,
            bg.b as f64,
            1.0,
        );
        window.setBackgroundColor(Some(&ns_bg));
    } else {
        // Transparent NSWindow MUST use clearColor() so Metal's pane_bg alpha is NEVER double-blended!
        window.setBackgroundColor(Some(&NSColor::clearColor()));
    }
}

/// Create a native macOS `NSWindow` hosting a `TerminalMetalView`.
pub fn create_terminal_window(
    mtm: MainThreadMarker,
    app_cfg: AppConfig,
    profile: Profile,
    is_hotkey_window: bool,
) -> Result<(Retained<NSWindow>, Retained<TerminalMetalView>), String> {
    let shaper = CoreTextShaper::from_profile(&profile, 2.0);
    let m = shaper.metrics();
    let w_pt = ((profile.columns as f32 * m.cell_width) / 2.0).clamp(480.0, 2200.0) as f64;
    let h_pt = ((profile.rows as f32 * m.cell_height) / 2.0).clamp(320.0, 1400.0) as f64;

    let frame = NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(w_pt, h_pt));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str(&format!(
        "JTerm — {}",
        profile.name
    )));
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    // CRITICAL: NSWindow defaults to isReleasedWhenClosed = true, which double-releases
    // when Retained<NSWindow> also drops (causing SIGSEGV in AutoreleasePoolPage::releaseUntil!).
    unsafe { window.setReleasedWhenClosed(false) };
    let reduce_transparency = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency();
    let is_opaque = reduce_transparency || profile.transparency <= 0.001;
    apply_window_appearance_and_bg(&window, is_opaque, profile.colors.background);
    // NEVER set `CanJoinAllSpaces` — terminal windows must stay on their assigned macOS Space
    // when switching workspaces instead of following the user across Spaces!
    if is_hotkey_window {
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenAuxiliary);
    } else {
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    }

    let scale = window.backingScaleFactor() as f32;
    let content_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w_pt, h_pt));
    let view = TerminalMetalView::new(
        mtm,
        content_rect,
        app_cfg,
        profile,
        scale,
        is_hotkey_window,
        reduce_transparency,
    )?;
    window.setContentView(Some(&view));
    let _ = window.makeFirstResponder(Some(&view));
    window.center();
    window.makeKeyAndOrderFront(None);

    {
        let (vp_w, vp_h) = view.viewport_px();
        if let Some(ref mut st) = *view.ivars().state.borrow_mut() {
            st.layout_and_resize_ptys(vp_w, vp_h);
        }
    }

    Ok((window, view))
}
