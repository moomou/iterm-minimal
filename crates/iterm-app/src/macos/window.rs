//! Core `TerminalWindowState` lifecycle, PTY session event processing, command execution,
//! and lock-free snapshot frame submission.
//!
//! Companion modules:
//! - `view.rs`: `TerminalMetalView` (`NSView` + `NSDraggingDestination` + `NSTextInputClient` IME)
//! - `chrome.rs`: compact 26 pt unified titlebar/tab strip, Find bar, Toast, and inline IME preedit
//! - `transfers.rs`: SSH SCP paste/drop upload & Semantic History Cmd+Click downloads

use crate::input_logic::UiCommand;
use crate::macos::app::{schedule_main_wakeup, schedule_main_wakeup_after};
use crate::macos::chrome::{get_local_hostname, make_ui_chrome_shaper, DIVIDER_WIDTH_PT};
use crate::macos::clipboard::{
    read_clipboard, read_clipboard_content, write_clipboard, ClipboardContent,
};
use crate::macos::coretext_shaper::CoreTextShaper;
use crate::macos::metal_renderer::MetalRenderer;
use crossbeam_channel::{unbounded, Receiver, Sender};
use iterm_profile::{AppConfig, Profile, SmartSelector};
use iterm_pty::{
    save_inline_download, shell_escape_path, FocusDirection, PaneRect, PtySize, SessionId,
    SplitAxis, TabManager, TerminalSession, WakeCallback,
};
use iterm_render::{
    CachedShaper, FrameBatches, GlyphAtlas, PaneRenderCache, PaneSnapshot, QuadInstance,
};
use iterm_vt::{InlineImage, SelectionMode, SelectionPoint, TermModes, TerminalEvent};
use objc2::rc::Retained;
use objc2_core_foundation::CGSize;
use objc2_quartz_core::CAMetalLayer;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) struct FindState {
    pub(crate) active: bool,
    pub(crate) query: String,
    pub(crate) regex: bool,
    pub(crate) ignore_case: bool,
    pub(crate) match_count: usize,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            active: false,
            query: String::new(),
            regex: false,
            ignore_case: true,
            match_count: 0,
        }
    }
}

/// Active `NSTextInputClient` IME composition state (CJK, dead keys, press-and-hold accents).
#[derive(Clone, Debug, Default)]
pub(crate) struct ImeState {
    pub(crate) marked_text: String,
    pub(crate) selected_range: (usize, usize),
}

impl ImeState {
    pub(crate) fn clear(&mut self) {
        self.marked_text.clear();
        self.selected_range = (0, 0);
    }
}

pub(crate) struct DragDividerState {
    pub(crate) split_id: u64,
    pub(crate) axis: SplitAxis,
}

/// Completed background file transfer event delivered back to the UI thread.
pub(crate) enum AsyncPasteResult {
    Uploaded {
        sid: SessionId,
        remote_path: String,
        bytes: u64,
    },
    Toast {
        msg: String,
    },
    Failed {
        label: String,
        error: String,
    },
}

/// Complete mutable state of one native terminal window.
pub struct TerminalWindowState {
    pub app_cfg: AppConfig,
    pub profile: Profile,
    pub font_size_delta: f32,
    pub tabs: TabManager,
    pub sessions: FxHashMap<SessionId, TerminalSession>,
    pub render_caches: FxHashMap<SessionId, PaneRenderCache>,
    pub snapshots: FxHashMap<SessionId, PaneSnapshot>,
    pub atlas: Arc<GlyphAtlas>,
    pub shaper: CachedShaper<CoreTextShaper>,
    pub ui_shaper: CachedShaper<CoreTextShaper>,
    pub renderer: MetalRenderer,
    pub batches: FrameBatches,
    pub smart_selector: SmartSelector,
    pub local_hostname: String,
    pub wake_flag: Arc<AtomicBool>,
    pub scale_factor: f32,
    pub(crate) find: FindState,
    pub(crate) ime: ImeState,
    pub visual_bell_until: Option<Instant>,
    pub status_toast: Option<(String, Instant)>,
    pub is_hotkey_window: bool,
    pub should_close: bool,
    pub request_new_window: bool,
    pub request_open_prefs: bool,
    pub(crate) drag_divider: Option<DragDividerState>,
    pub(crate) selecting: bool,
    pub(crate) selection_anchor: Option<(SelectionPoint, SelectionMode)>,
    start_time: Instant,
    pub(crate) scroll_accum_y: f64,
    pub(crate) metal_layer: Retained<CAMetalLayer>,
    pub(crate) window_focused: bool,
    pub(crate) show_perf_hud: bool,
    pub(crate) saved_transparency: f32,
    pub(crate) window_opacity_dirty: bool,
    pub(crate) last_cpu_prep_ms: f32,
    last_drawable_size: (u32, u32),
    last_present: Instant,
    sync_defer_since: Option<Instant>,
    pub(crate) upload_tx: Sender<AsyncPasteResult>,
    upload_rx: Receiver<AsyncPasteResult>,
}

impl TerminalWindowState {
    pub fn new(
        app_cfg: AppConfig,
        profile: Profile,
        scale_factor: f32,
        is_hotkey_window: bool,
        metal_layer: Retained<CAMetalLayer>,
    ) -> Result<Self, String> {
        let atlas = Arc::new(GlyphAtlas::new_dual(4096, 4096, 1024, 1024));
        let renderer = MetalRenderer::new(&atlas)?;
        renderer.configure_layer(&metal_layer, profile.transparency <= 0.001);

        let shaper = CachedShaper::new(CoreTextShaper::from_profile(&profile, scale_factor));
        let ui_shaper = CachedShaper::new(make_ui_chrome_shaper(scale_factor));
        let metrics = shaper.metrics();
        let wake_flag = Arc::new(AtomicBool::new(true));
        let wake_cb: WakeCallback = {
            let wf = Arc::clone(&wake_flag);
            Arc::new(move |_| {
                wf.store(true, Ordering::Release);
                schedule_main_wakeup();
            })
        };

        let init_size = PtySize {
            cols: profile.columns,
            rows: profile.rows,
            pixel_width: (profile.columns as f32 * metrics.cell_width) as u16,
            pixel_height: (profile.rows as f32 * metrics.cell_height) as u16,
        };

        let session = TerminalSession::spawn(profile.clone(), init_size, None, Some(wake_cb))
            .map_err(|e| format!("failed to spawn initial PTY: {e}"))?;
        let sid = session.id;
        let tabs = TabManager::new(sid, profile.name.clone());

        let mut sessions = FxHashMap::default();
        sessions.insert(sid, session);
        let mut render_caches = FxHashMap::default();
        render_caches.insert(sid, PaneRenderCache::new());
        let mut snapshots = FxHashMap::default();
        snapshots.insert(sid, PaneSnapshot::default());

        let smart_selector = SmartSelector::new(&profile.smart_selection_rules);
        let local_hostname = get_local_hostname();
        let (upload_tx, upload_rx) = unbounded();

        let saved_transparency = if profile.transparency > 0.001 {
            profile.transparency
        } else {
            0.24
        };

        Ok(Self {
            app_cfg,
            profile,
            font_size_delta: 0.0,
            tabs,
            sessions,
            render_caches,
            snapshots,
            atlas,
            shaper,
            ui_shaper,
            renderer,
            batches: FrameBatches::default(),
            smart_selector,
            local_hostname,
            wake_flag,
            scale_factor,
            find: FindState::default(),
            ime: ImeState::default(),
            visual_bell_until: None,
            status_toast: None,
            is_hotkey_window,
            should_close: false,
            request_new_window: false,
            request_open_prefs: false,
            drag_divider: None,
            selecting: false,
            selection_anchor: None,
            start_time: Instant::now(),
            scroll_accum_y: 0.0,
            metal_layer,
            window_focused: true,
            show_perf_hud: false,
            saved_transparency,
            window_opacity_dirty: false,
            last_cpu_prep_ms: 0.0,
            last_drawable_size: (0, 0),
            last_present: Instant::now() - Duration::from_secs(1),
            sync_defer_since: None,
            upload_tx,
            upload_rx,
        })
    }

    fn make_wake_cb(&self) -> WakeCallback {
        let wf = Arc::clone(&self.wake_flag);
        Arc::new(move |_| {
            wf.store(true, Ordering::Release);
            schedule_main_wakeup();
        })
    }

    fn active_cwd_for_recycle(&self) -> Option<String> {
        let active_id = self.tabs.try_active_session()?;
        let sess = self.sessions.get(&active_id)?;
        let s = sess.screen.lock();
        // Only recycle CWD if the active session is local (not remote SSH)
        if let Some(rh) = s.current_host() {
            let h = rh.split('@').next_back().unwrap_or(rh);
            if !h.is_empty() && !h.eq_ignore_ascii_case(&self.local_hostname) {
                return None;
            }
        }
        s.current_cwd().map(ToOwned::to_owned)
    }

    fn spawn_new_session(&mut self, view_bounds_px: PaneRect) -> Option<SessionId> {
        let cwd = match self.profile.initial_directory {
            iterm_profile::InitialDirectory::Recycle => self.active_cwd_for_recycle(),
            iterm_profile::InitialDirectory::Custom(ref d) if !d.is_empty() => Some(d.clone()),
            _ => None,
        };
        let m = self.shaper.metrics();
        let cols = ((view_bounds_px.width / m.cell_width).floor() as u16).max(20);
        let rows = ((view_bounds_px.height / m.cell_height).floor() as u16).max(5);
        let size = PtySize {
            cols,
            rows,
            pixel_width: view_bounds_px.width as u16,
            pixel_height: view_bounds_px.height as u16,
        };
        let sess = TerminalSession::spawn(
            self.profile.clone(),
            size,
            cwd,
            Some(self.make_wake_cb()),
        )
        .ok()?;
        let sid = sess.id;
        self.sessions.insert(sid, sess);
        self.render_caches.insert(sid, PaneRenderCache::new());
        self.snapshots.insert(sid, PaneSnapshot::default());
        Some(sid)
    }

    /// Resize all PTYs in the active tab to match their computed pixel rects.
    pub fn layout_and_resize_ptys(&mut self, vp_w: f32, vp_h: f32) {
        if self.should_close || self.tabs.is_empty() {
            return;
        }
        let bounds = self.pane_area_rect(vp_w, vp_h);
        let div_px = (DIVIDER_WIDTH_PT * self.scale_factor).max(1.0);
        let Some(active_tab) = self.tabs.try_active_tab() else {
            return;
        };
        let (panes, _) = active_tab.panes.layout(bounds, div_px);
        let m = self.shaper.metrics();

        for (sid, rect) in panes {
            if let Some(sess) = self.sessions.get(&sid) {
                let cols = ((rect.width / m.cell_width).floor() as u16).max(4);
                let rows = ((rect.height / m.cell_height).floor() as u16).max(2);
                let cur_dims = {
                    let s = sess.screen.lock();
                    (s.cols() as u16, s.rows() as u16)
                };
                if cur_dims != (cols, rows) {
                    let _ = sess.resize(
                        PtySize {
                            cols,
                            rows,
                            pixel_width: rect.width as u16,
                            pixel_height: rect.height as u16,
                        },
                        m.cell_width,
                        m.cell_height,
                    );
                }
            }
        }
        self.wake_flag.store(true, Ordering::Release);
    }

    pub(crate) fn rebuild_fonts(&mut self, vp_w: f32, vp_h: f32) {
        let mut adj = self.profile.clone();
        adj.normal_font.size = (self.profile.normal_font.size + self.font_size_delta).clamp(8.0, 72.0);
        adj.non_ascii_font.size =
            (self.profile.non_ascii_font.size + self.font_size_delta).clamp(8.0, 72.0);
        self.shaper = CachedShaper::new(CoreTextShaper::from_profile(&adj, self.scale_factor));
        self.ui_shaper = CachedShaper::new(make_ui_chrome_shaper(self.scale_factor));
        self.atlas.clear();
        for rc in self.render_caches.values_mut() {
            rc.invalidate_all();
        }
        self.layout_and_resize_ptys(vp_w, vp_h);
    }

    /// Drain out-of-band terminal events (title, OSC 6 tab color, OSC 52 clipboard, `it2dl` downloads, bell)
    /// and clean up exited sessions. Returns `Some(new_window_title)` if title changed.
    pub fn process_session_events(&mut self, vp_w: f32, vp_h: f32) -> Option<String> {
        if self.should_close || self.tabs.is_empty() {
            self.should_close = true;
            return None;
        }
        let mut new_title: Option<String> = None;
        let mut exited_sessions = Vec::new();
        let Some(active_sid) = self.tabs.try_active_session() else {
            self.should_close = true;
            return None;
        };

        let all_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for sid in all_ids {
            let (events, exited) = {
                let Some(sess) = self.sessions.get(&sid) else {
                    continue;
                };
                let evs = sess.screen.lock().drain_events();
                (evs, sess.has_exited())
            };

            for ev in events {
                match ev {
                    TerminalEvent::TitleChanged(title) => {
                        self.tabs.update_session_title(sid, title.clone());
                        if sid == active_sid {
                            new_title = Some(title);
                        }
                    }
                    TerminalEvent::TabColorChanged(color) => {
                        self.tabs.update_session_tab_color(sid, color);
                        self.wake_flag.store(true, Ordering::Release);
                    }
                    TerminalEvent::Bell => {
                        if self.profile.visual_bell || self.profile.flashing_bell {
                            self.visual_bell_until = Some(Instant::now() + Duration::from_millis(120));
                            self.wake_flag.store(true, Ordering::Release);
                        }
                    }
                    TerminalEvent::ClipboardSet(text) => {
                        if self.app_cfg.allow_clipboard_access {
                            write_clipboard(&text);
                        }
                    }
                    TerminalEvent::ClipboardRequest => {
                        // Security: block silent OSC 52 clipboard reads (exfiltration vector from remote SSH hosts);
                        // OSC 52 writes (tmux yank -> macOS clipboard) remain enabled via `allow_clipboard_access`.
                        log::debug!("Blocked unsolicited OSC 52 clipboard read request");
                    }
                    TerminalEvent::FileDownload { name, data } => {
                        match save_inline_download(&self.app_cfg.download_dir, &name, &data) {
                            Ok(dest) => {
                                let msg = format!(
                                    "Downloaded {} ({} bytes)",
                                    dest.file_name().and_then(|s| s.to_str()).unwrap_or(&name),
                                    data.len()
                                );
                                log::info!("{msg} -> {}", dest.display());
                                self.status_toast = Some((msg, Instant::now() + Duration::from_secs(4)));
                                self.wake_flag.store(true, Ordering::Release);
                            }
                            Err(e) => {
                                log::error!("it2dl save failed for {name}: {e}");
                            }
                        }
                    }
                    TerminalEvent::Notification { title, body } => {
                        let msg = if title.is_empty() || title == "Notification" {
                            body
                        } else {
                            format!("{title}: {body}")
                        };
                        self.status_toast = Some((msg, Instant::now() + Duration::from_secs(5)));
                        self.wake_flag.store(true, Ordering::Release);
                    }
                    TerminalEvent::ProfileChangeRequested(pname) => {
                        if let Some(found) = self.app_cfg.profile_by_name(&pname).cloned() {
                            self.profile = found;
                            self.rebuild_fonts(vp_w, vp_h);
                        }
                    }
                    _ => {}
                }
            }

            if exited && self.profile.close_sessions_on_end {
                exited_sessions.push(sid);
            }
        }

        for sid in exited_sessions {
            self.sessions.remove(&sid);
            self.render_caches.remove(&sid);
            self.snapshots.remove(&sid);
            if self.tabs.close_session(sid) {
                self.should_close = true;
            } else {
                self.layout_and_resize_ptys(vp_w, vp_h);
            }
        }

        // Drain completed background file uploads (pasted/dropped files into SSH sessions)
        while let Ok(res) = self.upload_rx.try_recv() {
            match res {
                AsyncPasteResult::Uploaded {
                    sid,
                    remote_path,
                    bytes,
                } => {
                    if let Some(sess) = self.sessions.get(&sid) {
                        let escaped = format!("{} ", shell_escape_path(&remote_path));
                        let _ = sess.write_paste(&escaped, None);
                    }
                    self.status_toast = Some((
                        format!("Uploaded -> {remote_path} ({bytes} B)"),
                        Instant::now() + Duration::from_secs(4),
                    ));
                    self.wake_flag.store(true, Ordering::Release);
                }
                AsyncPasteResult::Toast { msg } => {
                    self.status_toast = Some((msg, Instant::now() + Duration::from_secs(4)));
                    self.wake_flag.store(true, Ordering::Release);
                }
                AsyncPasteResult::Failed { label, error } => {
                    log::error!("Upload failed ({label}): {error}");
                    self.status_toast = Some((
                        format!("Upload failed: {label}"),
                        Instant::now() + Duration::from_secs(4),
                    ));
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
        }
        new_title
    }

    /// Execute a resolved `UiCommand` (keyboard shortcut or menu item).
    pub fn handle_command(&mut self, cmd: UiCommand, vp_w: f32, vp_h: f32) {
        if self.should_close || self.tabs.is_empty() {
            if matches!(cmd, UiCommand::NewWindow) {
                self.request_new_window = true;
            } else if matches!(cmd, UiCommand::OpenPreferences) {
                self.request_open_prefs = true;
            }
            return;
        }
        let Some(active_id) = self.tabs.try_active_session() else {
            self.should_close = true;
            return;
        };
        match cmd {
            UiCommand::SendBytes(bytes) => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    {
                        let mut s = sess.screen.lock();
                        if s.viewport_offset > 0 {
                            s.viewport_offset = 0;
                        }
                        if s.selection.is_some() {
                            s.selection = None;
                            self.wake_flag.store(true, Ordering::Release);
                        }
                    }
                    let _ = sess.write_input(&bytes);
                }
            }
            UiCommand::NewWindow => {
                self.request_new_window = true;
            }
            UiCommand::NewTab => {
                let bounds = self.pane_area_rect(vp_w, vp_h);
                if let Some(sid) = self.spawn_new_session(bounds) {
                    self.tabs.add_tab(sid, self.profile.name.clone());
                    self.layout_and_resize_ptys(vp_w, vp_h);
                }
            }
            UiCommand::ClosePaneOrTab => {
                self.sessions.remove(&active_id);
                self.render_caches.remove(&active_id);
                self.snapshots.remove(&active_id);
                if self.tabs.close_session(active_id) {
                    self.should_close = true;
                } else {
                    self.layout_and_resize_ptys(vp_w, vp_h);
                }
            }
            UiCommand::SplitVertical => {
                let bounds = self.pane_area_rect(vp_w, vp_h);
                if let Some(sid) = self.spawn_new_session(bounds) {
                    self.tabs.split_active_pane(SplitAxis::Vertical, sid);
                    self.layout_and_resize_ptys(vp_w, vp_h);
                }
            }
            UiCommand::SplitHorizontal => {
                let bounds = self.pane_area_rect(vp_w, vp_h);
                if let Some(sid) = self.spawn_new_session(bounds) {
                    self.tabs.split_active_pane(SplitAxis::Horizontal, sid);
                    self.layout_and_resize_ptys(vp_w, vp_h);
                }
            }
            UiCommand::ToggleZoomPane => {
                self.tabs.active_tab_mut().panes.toggle_zoom();
                self.layout_and_resize_ptys(vp_w, vp_h);
            }
            UiCommand::FocusPaneLeft => {
                let prev = active_id;
                let bounds = self.pane_area_rect(vp_w, vp_h);
                self.tabs.focus_direction(FocusDirection::Left, bounds);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::FocusPaneRight => {
                let prev = active_id;
                let bounds = self.pane_area_rect(vp_w, vp_h);
                self.tabs.focus_direction(FocusDirection::Right, bounds);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::FocusPaneUp => {
                let prev = active_id;
                let bounds = self.pane_area_rect(vp_w, vp_h);
                self.tabs.focus_direction(FocusDirection::Up, bounds);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::FocusPaneDown => {
                let prev = active_id;
                let bounds = self.pane_area_rect(vp_w, vp_h);
                self.tabs.focus_direction(FocusDirection::Down, bounds);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::NextPane => {
                let prev = active_id;
                self.tabs.active_tab_mut().panes.cycle_pane(true);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::PrevPane => {
                let prev = active_id;
                self.tabs.active_tab_mut().panes.cycle_pane(false);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::NextTab => {
                let prev = active_id;
                self.tabs.next_tab();
                self.layout_and_resize_ptys(vp_w, vp_h);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::PrevTab => {
                let prev = active_id;
                self.tabs.prev_tab();
                self.layout_and_resize_ptys(vp_w, vp_h);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::SelectTab(idx) => {
                let prev = active_id;
                self.tabs.select_tab(idx);
                self.layout_and_resize_ptys(vp_w, vp_h);
                self.notify_pane_focus_transition(prev);
            }
            UiCommand::MoveTabLeft => {
                self.tabs.move_active_tab(-1);
                self.wake_flag.store(true, Ordering::Release);
            }
            UiCommand::MoveTabRight => {
                self.tabs.move_active_tab(1);
                self.wake_flag.store(true, Ordering::Release);
            }
            UiCommand::Copy => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    if let Some(text) = sess.screen.lock().extract_selection_text() {
                        write_clipboard(&text);
                    }
                }
            }
            UiCommand::Paste => {
                match read_clipboard_content() {
                    Some(ClipboardContent::Files(paths)) => {
                        self.handle_local_files_attach(active_id, paths);
                    }
                    Some(ClipboardContent::ImagePng(png_bytes)) => {
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        let pid = std::process::id();
                        let tmp = std::env::temp_dir()
                            .join(format!("iterm-paste-{pid}-{ts:x}.png"));
                        let wrote = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .open(&tmp)
                            .and_then(|mut f| f.write_all(&png_bytes))
                            .is_ok();
                        if wrote {
                            self.handle_temp_files_attach(active_id, vec![tmp]);
                        }
                    }
                    Some(ClipboardContent::Text(text)) => {
                        if let Some(sess) = self.sessions.get(&active_id) {
                            let _ = sess.write_paste(&text, None);
                        }
                    }
                    None => {}
                }
            }
            UiCommand::AdvancedPaste => {
                // Advanced paste: normalize line endings, strip ESC, force bracketed paste if enabled
                if let (Some(sess), Some(text)) = (self.sessions.get(&active_id), read_clipboard()) {
                    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
                    let _ = sess.write_paste(&normalized, Some(true));
                }
            }
            UiCommand::ClearBuffer => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().clear_scrollback_and_screen();
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ToggleFind => {
                self.find.active = !self.find.active;
                if !self.find.active {
                    if let Some(sess) = self.sessions.get(&active_id) {
                        let mut s = sess.screen.lock();
                        s.search_matches.clear();
                        s.active_search_match = None;
                    }
                }
                self.wake_flag.store(true, Ordering::Release);
            }
            UiCommand::FindNext => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().next_search_match(true);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::FindPrev => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().next_search_match(false);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::FontIncrease => {
                self.font_size_delta = (self.font_size_delta + 1.0).min(36.0);
                self.rebuild_fonts(vp_w, vp_h);
            }
            UiCommand::FontDecrease => {
                self.font_size_delta = (self.font_size_delta - 1.0).max(-12.0);
                self.rebuild_fonts(vp_w, vp_h);
            }
            UiCommand::FontReset => {
                self.font_size_delta = 0.0;
                self.rebuild_fonts(vp_w, vp_h);
            }
            UiCommand::JumpPrevPrompt => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().jump_to_prompt(true);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::JumpNextPrompt => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().jump_to_prompt(false);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ScrollPageUp => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    let mut s = sess.screen.lock();
                    let page = s.rows().saturating_sub(2) as isize;
                    s.scroll_viewport(page);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ScrollPageDown => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    let mut s = sess.screen.lock();
                    let page = s.rows().saturating_sub(2) as isize;
                    s.scroll_viewport(-page);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ScrollLineUp => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().scroll_viewport(1);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ScrollLineDown => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().scroll_viewport(-1);
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ScrollHome => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    let mut s = sess.screen.lock();
                    s.viewport_offset = s.scrollback.len();
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ScrollEnd => {
                if let Some(sess) = self.sessions.get(&active_id) {
                    sess.screen.lock().viewport_offset = 0;
                    self.wake_flag.store(true, Ordering::Release);
                }
            }
            UiCommand::ToggleMouseReporting => {
                self.profile.mouse_reporting = !self.profile.mouse_reporting;
            }
            UiCommand::ToggleTransparency => {
                if self.profile.transparency > 0.001 {
                    self.saved_transparency = self.profile.transparency;
                    self.profile.transparency = 0.0;
                } else {
                    self.profile.transparency = self.saved_transparency.clamp(0.05, 0.90);
                }
                let is_opaque = self.profile.transparency <= 0.001;
                self.renderer.configure_layer(&self.metal_layer, is_opaque);
                for rc in self.render_caches.values_mut() {
                    rc.invalidate_all();
                }
                self.window_opacity_dirty = true;
                self.wake_flag.store(true, Ordering::Release);
            }
            UiCommand::TogglePerfHud => {
                self.show_perf_hud = !self.show_perf_hud;
                self.wake_flag.store(true, Ordering::Release);
            }
            UiCommand::OpenPreferences => {
                self.request_open_prefs = true;
            }
            UiCommand::ToggleFullscreen | UiCommand::None => {}
        }
    }

    /// Emit `DECSET ?1004` FocusOut (`\x1b[O`) / FocusIn (`\x1b[I`) when switching active pane or tab.
    pub(crate) fn notify_pane_focus_transition(&mut self, prev_sid: SessionId) -> bool {
        let Some(new_sid) = self.tabs.try_active_session() else {
            return false;
        };
        if prev_sid == new_sid {
            return false;
        }
        if let Some(old_sess) = self.sessions.get(&prev_sid) {
            if old_sess.screen.lock().modes.contains(TermModes::FOCUS_EVENTS) {
                let _ = old_sess.write_input(b"\x1b[O");
            }
        }
        if self.window_focused {
            if let Some(new_sess) = self.sessions.get(&new_sid) {
                if new_sess.screen.lock().modes.contains(TermModes::FOCUS_EVENTS) {
                    let _ = new_sess.write_input(b"\x1b[I");
                }
            }
        }
        self.wake_flag.store(true, Ordering::Release);
        true
    }

    /// Focus `sid` within the active tab, returning `true` if focus actually changed.
    pub(crate) fn focus_pane_session(&mut self, sid: SessionId) -> bool {
        let Some(prev) = self.tabs.try_active_session() else {
            return false;
        };
        if prev == sid {
            return false;
        }
        self.tabs.active_tab_mut().panes.active = sid;
        self.notify_pane_focus_transition(prev)
    }

    /// Forward DECSET ?1004 FocusIn (`\x1b[I`) / FocusOut (`\x1b[O`) to the active session when window focus changes.
    pub fn set_window_focused(&mut self, focused: bool) {
        if self.window_focused == focused {
            return;
        }
        self.window_focused = focused;
        if let Some(sid) = self.tabs.try_active_session() {
            if let Some(sess) = self.sessions.get(&sid) {
                if sess.screen.lock().modes.contains(TermModes::FOCUS_EVENTS) {
                    let _ = sess.write_input(if focused { b"\x1b[I" } else { b"\x1b[O" });
                }
            }
        }
        self.wake_flag.store(true, Ordering::Release);
    }

    /// Render a frame if any session is dirty or an animation/toast is active.
    pub fn render_if_needed(&mut self, vp_w: f32, vp_h: f32) {
        if self.should_close || self.tabs.is_empty() {
            return;
        }
        let Some(active_tab) = self.tabs.try_active_tab() else {
            return;
        };
        let mut any_dirty = self.wake_flag.swap(false, Ordering::AcqRel);
        let active_tab_sessions = active_tab.panes.sessions();
        for &sid in &active_tab_sessions {
            if let Some(sess) = self.sessions.get(&sid) {
                if sess.take_dirty() {
                    any_dirty = true;
                }
            }
        }
        if let Some(until) = self.visual_bell_until {
            if Instant::now() < until {
                any_dirty = true;
            } else {
                self.visual_bell_until = None;
                any_dirty = true;
            }
        }
        if !any_dirty {
            return;
        }

        // Adaptive 0-delay-idle / exact-120Hz-flood render pacing via single-shot GCD `after()`!
        let now_present = Instant::now();
        let min_interval = Duration::from_micros(7500);
        let since_last = now_present.duration_since(self.last_present);
        if since_last < min_interval {
            self.wake_flag.store(true, Ordering::Release);
            schedule_main_wakeup_after(min_interval - since_last);
            return;
        }

        // Check DEC 2026 Synchronized Output BEFORE clearing/submitting to Metal!
        let any_synced = active_tab_sessions.iter().any(|sid| {
            self.sessions
                .get(sid)
                .is_some_and(|sess| sess.screen.lock().modes.contains(TermModes::SYNC_OUTPUT))
        });
        if any_synced {
            let now = Instant::now();
            let started = *self.sync_defer_since.get_or_insert(now);
            let rem = Duration::from_millis(16).saturating_sub(now.duration_since(started));
            if !rem.is_zero() {
                self.wake_flag.store(true, Ordering::Release);
                schedule_main_wakeup_after(rem.min(Duration::from_millis(4)));
                return;
            }
        } else {
            self.sync_defer_since = None;
        }

        let target_dw = (vp_w.round() as u32).max(1);
        let target_dh = (vp_h.round() as u32).max(1);
        if self.last_drawable_size != (target_dw, target_dh) {
            self.metal_layer.setDrawableSize(CGSize {
                width: target_dw as f64,
                height: target_dh as f64,
            });
            self.last_drawable_size = (target_dw, target_dh);
        }

        self.batches.clear();
        let bounds = self.pane_area_rect(vp_w, vp_h);
        let div_px = (DIVIDER_WIDTH_PT * self.scale_factor).max(1.0);
        let (panes, dividers) = self.tabs.active_tab().panes.layout(bounds, div_px);
        let active_sid = self.tabs.active_session();
        let multi_pane = panes.len() > 1;
        let dim_amount = if self.app_cfg.dim_inactive_split_panes && multi_pane {
            self.app_cfg.split_pane_dimming_amount
        } else {
            0.0
        };

        let elapsed = self.start_time.elapsed().as_secs_f32();
        let blink_on = ((elapsed * 2.0) as u64).is_multiple_of(2);
        let mut new_images: FxHashMap<u32, InlineImage> = FxHashMap::default();
        let mut live_image_ids: rustc_hash::FxHashSet<u32> = rustc_hash::FxHashSet::default();

        // Phase A: Capture lightweight lock-free snapshots under `screen.lock()` (~10 µs/pane),
        // then drop the lock IMMEDIATELY so `pty-reader` never contends with CoreText / Metal!
        let cpu_prep_start = Instant::now();
        for (sid, _) in &panes {
            let Some(sess) = self.sessions.get(sid) else {
                continue;
            };
            let snap = self.snapshots.entry(*sid).or_default();
            let screen = sess.screen.lock();
            snap.capture(&screen);
            if !screen.images.is_empty() {
                for (&img_id, img) in &screen.images {
                    live_image_ids.insert(img_id);
                    if !self.renderer.has_image_texture(img_id) {
                        new_images.insert(img_id, img.clone());
                    }
                }
            }
        }
        // Release GPU textures for any inline images cleared/GC'd from sessions
        self.renderer.retain_image_textures(&live_image_ids);

        // Phase B: Shape text, lookup/rasterize glyphs, and build Metal batches 100% LOCK-FREE!
        // Retry once cleanly if either glyph atlas plane resets mid-frame so rows 0..K never sample blank pixels!
        for attempt in 0..2 {
            let pre_epoch = self.atlas.epoch();
            self.batches.clear();
            for (sid, rect) in &panes {
                let Some(snap) = self.snapshots.get(sid) else {
                    continue;
                };
                let is_focused = *sid == active_sid;
                let dim = if is_focused { 0.0 } else { dim_amount };
                let cache = self
                    .render_caches
                    .entry(*sid)
                    .or_insert_with(PaneRenderCache::new);

                cache.prepare_snapshot(
                    snap,
                    &self.profile,
                    &mut self.shaper,
                    &self.atlas,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    is_focused,
                    dim,
                    blink_on,
                    &mut self.batches,
                );
            }
            if self.atlas.epoch() == pre_epoch || attempt == 1 {
                break;
            }
            for rc in self.render_caches.values_mut() {
                rc.invalidate_all();
            }
        }

        // Draw crisp split-pane dividers
        for div in dividers {
            self.batches.overlay_quads.push(QuadInstance {
                rect: [div.rect.x, div.rect.y, div.rect.width, div.rect.height],
                color: [0.35, 0.36, 0.42, 0.9],
                mode: 0,
                param: 0.0,
                _pad: [0.0; 2],
            });
        }

        self.last_cpu_prep_ms = cpu_prep_start.elapsed().as_secs_f32() * 1000.0;
        self.render_window_chrome(vp_w, vp_h, &panes, active_sid);

        // Clear to transparent black so each pane's `pane_bg` quad renders exact transparency ONCE
        // (eliminating double-blended opacity mismatches between panes and colored htop cells!)
        self.last_present = Instant::now();
        let presented = self.renderer.render_frame(
            &self.metal_layer,
            &self.atlas,
            &self.batches,
            &new_images,
            vp_w,
            vp_h,
            self.scale_factor,
            elapsed,
            [0.0, 0.0, 0.0, 0.0],
        );
        if !presented {
            self.wake_flag.store(true, Ordering::Release);
            schedule_main_wakeup_after(Duration::from_millis(4));
        }
    }
}
