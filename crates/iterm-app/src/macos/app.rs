//! Native macOS `NSApplication`, Menu Bar, multi-window lifecycle, and 120 Hz request-driven frame loop.

use crate::input_logic::UiCommand;
use crate::macos::hotkey::{GlobalHotkey, HOTKEY_TRIGGERED};
use crate::macos::prefs::PreferencesController;
use crate::macos::view::{create_terminal_window, TerminalMetalView};
use dispatch2::{DispatchQueue, DispatchTime};
use iterm_profile::AppConfig;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSEventModifierFlags,
    NSMenu, NSMenuItem, NSWindow, NSWindowDelegate, NSWindowOcclusionState, NSWorkspace,
    NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSProcessInfo, NSString};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

thread_local! {
    /// Main-thread-owned retained handle to the singleton `AppController`.
    /// Eliminates raw pointer casts and guarantees lifetime safety across GCD main-queue callbacks.
    static MAIN_CONTROLLER: RefCell<Option<Retained<AppController>>> = const { RefCell::new(None) };
}

/// Monotonic timebase for deadline-coalesced one-shot GCD wakeups (zero permanent NSTimer!).
static WAKE_EPOCH: OnceLock<Instant> = OnceLock::new();
/// Coalesces concurrent PTY reader wakeups into a single zero-delay main-queue GCD render pass!
static MAIN_WAKE_SCHEDULED: AtomicBool = AtomicBool::new(false);
/// Earliest currently scheduled delayed wakeup (nanoseconds since `WAKE_EPOCH`; `0` = none).
/// Allows a sooner deadline (e.g. 7.5 ms flood pacing) to preempt a later one (e.g. 4 s toast expiry)!
static NEXT_DELAYED_WAKE_NS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn monotonic_now_ns() -> u64 {
    let origin = WAKE_EPOCH.get_or_init(Instant::now);
    origin.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn dispatch_main_tick() {
    let ctrl = MAIN_CONTROLLER.with(|slot| slot.borrow().clone());
    if let Some(ctrl) = ctrl {
        ctrl.process_frame_tick();
    }
}

/// Wake the macOS main runloop in < 0.1 ms via GCD (even in Low Power Mode where NSTimer throttles!).
pub fn schedule_main_wakeup() {
    if MAIN_WAKE_SCHEDULED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        DispatchQueue::main().exec_async(|| {
            MAIN_WAKE_SCHEDULED.store(false, Ordering::Release);
            dispatch_main_tick();
        });
    }
}

/// Schedule a single exact-deadline GCD wakeup after `delay`, preempting any later scheduled deadline.
/// When no animations, toasts, or active blinking cursors exist, ZERO timers are armed (0.00 wakeups/sec!).
pub fn schedule_main_wakeup_after(delay: Duration) {
    let delay = delay.max(Duration::from_micros(500));
    let now = monotonic_now_ns();
    let target = now.saturating_add(delay.as_nanos().min(u64::MAX as u128) as u64).max(1);

    let mut cur = NEXT_DELAYED_WAKE_NS.load(Ordering::Acquire);
    loop {
        // If an earlier (or within 0.5 ms) wakeup is already pending, coalesce into it!
        if cur != 0 && cur <= target.saturating_add(500_000) && cur >= now {
            return;
        }
        match NEXT_DELAYED_WAKE_NS.compare_exchange_weak(
            cur,
            target,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }

    let when = DispatchTime::try_from(delay).unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(when, move || {
        // Clear deadline slot if ours was the earliest scheduled wakeup
        let _ = NEXT_DELAYED_WAKE_NS.compare_exchange(
            target,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        dispatch_main_tick();
    });
}

struct ManagedWindow {
    window: Retained<NSWindow>,
    view: Retained<TerminalMetalView>,
    is_hotkey: bool,
}

pub struct AppControllerIvars {
    app_cfg: RefCell<AppConfig>,
    windows: RefCell<Vec<ManagedWindow>>,
    prefs: Retained<PreferencesController>,
    _hotkey: RefCell<Option<GlobalHotkey>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppControllerIvars]
    pub struct AppController;

    impl AppController {
        #[unsafe(method(menuPreferences:))]
        fn menu_preferences(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let cfg = self.ivars().app_cfg.borrow().clone();
            self.ivars().prefs.show(mtm, &cfg);
        }

        #[unsafe(method(menuRequestScreenRecording:))]
        fn menu_request_screen_recording(&self, _sender: Option<&AnyObject>) {
            let granted = unsafe { CGPreflightScreenCaptureAccess() };
            let msg = if granted {
                "Screen Recording permission is already granted for JTerm".to_string()
            } else {
                let now_granted = unsafe { CGRequestScreenCaptureAccess() };
                if now_granted {
                    "Screen Recording permission granted for JTerm".to_string()
                } else {
                    "Screen Recording requested — enable JTerm in System Settings & restart"
                        .to_string()
                }
            };
            let view = {
                let wins = self.ivars().windows.borrow();
                wins.iter()
                    .find(|w| w.window.isKeyWindow())
                    .or_else(|| wins.first())
                    .map(|mw| mw.view.clone())
            };
            if let Some(v) = view {
                v.show_toast(msg);
            }
        }

        #[unsafe(method(menuNewWindow:))]
        fn menu_new_window(&self, _sender: Option<&AnyObject>) {
            self.spawn_window(MainThreadMarker::from(self), false);
        }

        #[unsafe(method(menuNewTab:))]
        fn menu_new_tab(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::NewTab);
        }

        #[unsafe(method(menuSplitVertical:))]
        fn menu_split_vertical(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::SplitVertical);
        }

        #[unsafe(method(menuSplitHorizontal:))]
        fn menu_split_horizontal(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::SplitHorizontal);
        }

        #[unsafe(method(menuClosePane:))]
        fn menu_close_pane(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::ClosePaneOrTab);
        }

        #[unsafe(method(menuCopy:))]
        fn menu_copy(&self, sender: Option<&AnyObject>) {
            if !self.send_to_key_terminal_only(UiCommand::Copy) {
                let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
                unsafe {
                    let _: bool = app.sendAction_to_from(sel!(copy:), None, sender);
                }
            }
        }

        #[unsafe(method(menuPaste:))]
        fn menu_paste(&self, sender: Option<&AnyObject>) {
            if !self.send_to_key_terminal_only(UiCommand::Paste) {
                let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
                unsafe {
                    let _: bool = app.sendAction_to_from(sel!(paste:), None, sender);
                }
            }
        }

        #[unsafe(method(menuAdvancedPaste:))]
        fn menu_advanced_paste(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::AdvancedPaste);
        }

        #[unsafe(method(menuFind:))]
        fn menu_find(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::ToggleFind);
        }

        #[unsafe(method(menuClearBuffer:))]
        fn menu_clear_buffer(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::ClearBuffer);
        }

        #[unsafe(method(menuFontIncrease:))]
        fn menu_font_increase(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::FontIncrease);
        }

        #[unsafe(method(menuFontDecrease:))]
        fn menu_font_decrease(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::FontDecrease);
        }

        #[unsafe(method(menuFontReset:))]
        fn menu_font_reset(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::FontReset);
        }

        #[unsafe(method(menuZoomPane:))]
        fn menu_zoom_pane(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::ToggleZoomPane);
        }

        #[unsafe(method(menuToggleTransparency:))]
        fn menu_toggle_transparency(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::ToggleTransparency);
        }

        #[unsafe(method(menuToggleTransfers:))]
        fn menu_toggle_transfers(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::ToggleTransfers);
        }

        #[unsafe(method(menuTogglePerfHud:))]
        fn menu_toggle_perf_hud(&self, _sender: Option<&AnyObject>) {
            self.send_to_key_window(UiCommand::TogglePerfHud);
        }

        #[unsafe(method(onAccessibilityDisplayChanged:))]
        fn on_accessibility_display_changed(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }
    }

    unsafe impl NSObjectProtocol for AppController {}

    // Native macOS NSApplicationDelegate: clicking Dock icon deminiaturizes minimized windows,
    // or spawns a fresh window when no windows exist!
    unsafe impl NSApplicationDelegate for AppController {
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn application_should_handle_reopen(
            &self,
            _sender: &NSApplication,
            has_visible_windows: bool,
        ) -> bool {
            if !has_visible_windows {
                let minimized: Vec<Retained<NSWindow>> = self
                    .ivars()
                    .windows
                    .borrow()
                    .iter()
                    .filter(|mw| mw.window.isMiniaturized())
                    .map(|mw| mw.window.clone())
                    .collect();
                if !minimized.is_empty() {
                    for win in minimized {
                        win.deminiaturize(None);
                    }
                    schedule_main_wakeup();
                } else if self.ivars().windows.borrow().is_empty() {
                    self.spawn_window(MainThreadMarker::from(self), false);
                }
            }
            true
        }

        #[unsafe(method(applicationDidHide:))]
        fn application_did_hide(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }

        #[unsafe(method(applicationDidUnhide:))]
        fn application_did_unhide(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }
    }

    // Native macOS NSWindowDelegate: 0 ms event-driven FocusIn/FocusOut (`\x1b[I`/`\x1b[O`),
    // occlusion/minimize GPU pause/resume, and immediate PTY cleanup on red close-button click!
    unsafe impl NSWindowDelegate for AppController {
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }

        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn window_did_change_occlusion_state(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            schedule_main_wakeup();
        }
    }
);

impl AppController {
    fn new(mtm: MainThreadMarker, cfg: AppConfig) -> Retained<Self> {
        let hotkey = GlobalHotkey::register(&cfg.hotkey);
        let prefs = PreferencesController::new(mtm, cfg.clone());
        let this = mtm.alloc::<Self>().set_ivars(AppControllerIvars {
            app_cfg: RefCell::new(cfg),
            windows: RefCell::new(Vec::new()),
            prefs,
            _hotkey: RefCell::new(hotkey),
        });
        unsafe { msg_send![super(this), init] }
    }

    pub fn process_frame_tick(&self) {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::from(self);
            let app = NSApplication::sharedApplication(mtm);
            let app_hidden = app.isHidden();

            // Check Carbon global hotkey trigger
            if HOTKEY_TRIGGERED.swap(false, Ordering::AcqRel) {
                self.toggle_hotkey_window(mtm);
            }

            // Apply live Preferences panel edits to all open windows (never hold RefCell across AppKit calls)
            if let Some(updated_cfg) = self.ivars().prefs.take_pending_config() {
                *self.ivars().app_cfg.borrow_mut() = updated_cfg.clone();
                let views: Vec<Retained<TerminalMetalView>> = self
                    .ivars()
                    .windows
                    .borrow()
                    .iter()
                    .map(|mw| mw.view.clone())
                    .collect();
                for view in views {
                    view.apply_config_update(updated_cfg.clone());
                }
            }

            // Snapshot window handles so no RefCell borrow is held during tick()/AppKit calls
            let snapshot: Vec<(Retained<NSWindow>, Retained<TerminalMetalView>, bool)> = self
                .ivars()
                .windows
                .borrow()
                .iter()
                .map(|mw| (mw.window.clone(), mw.view.clone(), mw.is_hotkey))
                .collect();

            let mut need_new_window = false;
            let mut need_open_prefs = false;
            let mut close_windows: Vec<Retained<NSWindow>> = Vec::new();
            let mut next_wakeup: Option<Duration> = None;
            let low_power = NSProcessInfo::processInfo().isLowPowerModeEnabled();
            let reduce_transparency =
                NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency();

            for (win, view, is_hotkey) in snapshot {
                let is_mini = win.isMiniaturized();
                let is_visible = !app_hidden
                    && !is_mini
                    && win.occlusionState().contains(NSWindowOcclusionState::Visible);
                let (should_close, req_new, req_prefs, new_title, win_wake) =
                    view.tick(is_visible, low_power, reduce_transparency);
                if let Some(d) = win_wake {
                    next_wakeup = Some(match next_wakeup {
                        Some(cur) => cur.min(d),
                        None => d,
                    });
                }
                if let Some(title) = new_title {
                    win.setTitle(&NSString::from_str(&title));
                }
                if req_new {
                    need_new_window = true;
                }
                if req_prefs {
                    need_open_prefs = true;
                }
                let user_closed = !is_hotkey
                    && !app_hidden
                    && !is_mini
                    && win.windowNumber() <= 0;
                if should_close || user_closed {
                    close_windows.push(win);
                }
            }

            let mut to_close = Vec::new();
            if !close_windows.is_empty() {
                let mut wins = self.ivars().windows.borrow_mut();
                let mut i = 0;
                while i < wins.len() {
                    if close_windows
                        .iter()
                        .any(|cw| &**cw as *const NSWindow == &*wins[i].window as *const NSWindow)
                    {
                        to_close.push(wins.remove(i));
                    } else {
                        i += 1;
                    }
                }
            }
            for mw in to_close {
                mw.window.orderOut(None);
                mw.window.close();
            }

            if need_new_window {
                self.spawn_window(mtm, false);
            }
            if need_open_prefs {
                let cfg = self.ivars().app_cfg.borrow().clone();
                self.ivars().prefs.show(mtm, &cfg);
            }

            let quit_empty = self.ivars().app_cfg.borrow().quit_when_all_windows_closed;
            if quit_empty && self.ivars().windows.borrow().is_empty() {
                app.terminate(None);
                return;
            }

            // Purely demand-driven one-shot GCD timer: ONLY armed when an active blink, toast,
            // visual bell, or transfer auto-hide actually needs a future tick -> 0.00 wakeups/sec at idle!
            if let Some(delay) = next_wakeup {
                schedule_main_wakeup_after(delay);
            }
        });
    }

    fn spawn_window(&self, mtm: MainThreadMarker, is_hotkey: bool) {
        let cfg = self.ivars().app_cfg.borrow().clone();
        let profile = if is_hotkey {
            cfg.hotkey
                .profile_guid
                .as_deref()
                .and_then(|g| cfg.profile_by_guid(g))
                .unwrap_or_else(|| cfg.default_profile())
                .clone()
        } else {
            cfg.default_profile().clone()
        };
        match create_terminal_window(mtm, cfg, profile, is_hotkey) {
            Ok((window, view)) => {
                window.setDelegate(Some(ProtocolObject::from_ref(self)));
                self.ivars().windows.borrow_mut().push(ManagedWindow {
                    window,
                    view,
                    is_hotkey,
                });
            }
            Err(e) => {
                log::error!("Failed to create terminal window: {e}");
            }
        }
    }

    fn toggle_hotkey_window(&self, mtm: MainThreadMarker) {
        let app = NSApplication::sharedApplication(mtm);
        let has_dedicated_profile = self.ivars().app_cfg.borrow().hotkey.profile_guid.is_some();

        // Standard iTerm2 global hotkey ("Show/hide all windows with a system-wide hotkey"):
        // toggles app visibility without spawning a sticky `CanJoinAllSpaces` window!
        if !has_dedicated_profile {
            let has_windows = !self.ivars().windows.borrow().is_empty();
            if !has_windows {
                #[allow(deprecated)]
                app.activateIgnoringOtherApps(true);
                self.spawn_window(mtm, false);
                return;
            }
            if app.isActive() && !app.isHidden() {
                app.hide(None);
            } else {
                app.unhide(None);
                #[allow(deprecated)]
                app.activateIgnoringOtherApps(true);
            }
            return;
        }

        // Dedicated hotkey profile window (only when `hotkey.profile_guid` is explicitly configured)
        let existing_win: Option<Retained<NSWindow>> = self
            .ivars()
            .windows
            .borrow()
            .iter()
            .find(|mw| mw.is_hotkey)
            .map(|mw| mw.window.clone());
        if let Some(win) = existing_win {
            if win.isVisible() && app.isActive() {
                win.orderOut(None);
            } else {
                #[allow(deprecated)]
                app.activateIgnoringOtherApps(true);
                win.makeKeyAndOrderFront(None);
            }
        } else {
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            self.spawn_window(mtm, true);
        }
    }

    fn send_to_key_window(&self, cmd: UiCommand) {
        let view: Option<Retained<TerminalMetalView>> = {
            let wins = self.ivars().windows.borrow();
            wins.iter()
                .find(|w| w.window.isKeyWindow())
                .or_else(|| wins.first())
                .map(|mw| mw.view.clone())
        };
        if let Some(v) = view {
            v.send_command(cmd);
        }
    }

    fn send_to_key_terminal_only(&self, cmd: UiCommand) -> bool {
        let view: Option<Retained<TerminalMetalView>> = {
            let wins = self.ivars().windows.borrow();
            wins.iter()
                .find(|w| w.window.isKeyWindow())
                .map(|mw| mw.view.clone())
        };
        if let Some(v) = view {
            v.send_command(cmd);
            true
        } else {
            false
        }
    }
}

fn build_menu_bar(
    mtm: MainThreadMarker,
    target: &AppController,
) -> (Retained<NSMenu>, Retained<NSMenu>) {
    let main_menu = NSMenu::new(mtm);

    let add_submenu = |title: &str| -> Retained<NSMenu> {
        let item = NSMenuItem::new(mtm);
        let sub = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
        item.setSubmenu(Some(&sub));
        main_menu.addItem(&item);
        sub
    };

    let add_item = |menu: &NSMenu,
                    title: &str,
                    action: objc2::runtime::Sel,
                    key: &str,
                    mods: NSEventModifierFlags,
                    t: Option<&AnyObject>| {
        unsafe {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            );
            item.setKeyEquivalentModifierMask(mods);
            item.setTarget(t);
            menu.addItem(&item);
        }
    };

    let cmd = NSEventModifierFlags::Command;
    let cmd_shift = NSEventModifierFlags::Command | NSEventModifierFlags::Shift;
    let cmd_opt = NSEventModifierFlags::Command | NSEventModifierFlags::Option;
    let ctrl_cmd = NSEventModifierFlags::Control | NSEventModifierFlags::Command;
    let none = NSEventModifierFlags::empty();
    let tgt: Option<&AnyObject> = Some(target);

    // App menu
    let app_menu = add_submenu("JTerm");
    add_item(&app_menu, "Settings…", sel!(menuPreferences:), ",", cmd, tgt);
    add_item(
        &app_menu,
        "Request Screen Recording Access…",
        sel!(menuRequestScreenRecording:),
        "",
        none,
        tgt,
    );
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&app_menu, "Hide JTerm", sel!(hide:), "h", cmd, None);
    add_item(
        &app_menu,
        "Hide Others",
        sel!(hideOtherApplications:),
        "h",
        cmd_opt,
        None,
    );
    add_item(
        &app_menu,
        "Show All",
        sel!(unhideAllApplications:),
        "",
        none,
        None,
    );
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&app_menu, "Quit JTerm", sel!(terminate:), "q", cmd, None);

    // Shell menu
    let shell_menu = add_submenu("Shell");
    add_item(&shell_menu, "New Window", sel!(menuNewWindow:), "n", cmd, tgt);
    add_item(&shell_menu, "New Tab", sel!(menuNewTab:), "t", cmd, tgt);
    add_item(&shell_menu, "Split Vertically", sel!(menuSplitVertical:), "d", cmd, tgt);
    add_item(&shell_menu, "Split Horizontally", sel!(menuSplitHorizontal:), "d", cmd_shift, tgt);
    add_item(&shell_menu, "Close", sel!(menuClosePane:), "w", cmd, tgt);

    // Edit menu
    let edit_menu = add_submenu("Edit");
    add_item(&edit_menu, "Copy", sel!(menuCopy:), "c", cmd, tgt);
    add_item(&edit_menu, "Paste", sel!(menuPaste:), "v", cmd, tgt);
    add_item(&edit_menu, "Advanced Paste", sel!(menuAdvancedPaste:), "v", cmd_shift, tgt);
    add_item(&edit_menu, "Find...", sel!(menuFind:), "f", cmd, tgt);
    add_item(&edit_menu, "Clear Buffer", sel!(menuClearBuffer:), "k", cmd, tgt);

    // View menu
    let view_menu = add_submenu("View");
    add_item(&view_menu, "Make Text Bigger", sel!(menuFontIncrease:), "+", cmd, tgt);
    add_item(&view_menu, "Make Text Smaller", sel!(menuFontDecrease:), "-", cmd, tgt);
    add_item(&view_menu, "Make Text Normal Size", sel!(menuFontReset:), "0", cmd, tgt);
    add_item(&view_menu, "Maximize Active Pane", sel!(menuZoomPane:), "\r", cmd_shift, tgt);
    add_item(&view_menu, "Use Transparency", sel!(menuToggleTransparency:), "u", cmd, tgt);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&view_menu, "Transfers", sel!(menuToggleTransfers:), "l", cmd_opt, tgt);
    add_item(&view_menu, "Toggle Performance HUD", sel!(menuTogglePerfHud:), "i", cmd_opt, tgt);

    // Window menu (standard macOS Minimize Cmd+M, Minimize All Cmd+Opt+M, Zoom, Full Screen)
    let window_menu = add_submenu("Window");
    add_item(&window_menu, "Minimize", sel!(performMiniaturize:), "m", cmd, None);
    add_item(&window_menu, "Minimize All", sel!(miniaturizeAll:), "m", cmd_opt, None);
    add_item(&window_menu, "Zoom", sel!(performZoom:), "", none, None);
    add_item(&window_menu, "Enter Full Screen", sel!(toggleFullScreen:), "f", ctrl_cmd, None);
    window_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&window_menu, "Bring All to Front", sel!(arrangeInFront:), "", none, None);

    (main_menu, window_menu)
}

/// Launch the native macOS AppKit + Metal application event loop.
pub fn run_macos_app(cfg: AppConfig) {
    let mtm = MainThreadMarker::new().expect("run_macos_app must run on macOS main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let controller = AppController::new(mtm, cfg);
    MAIN_CONTROLLER.with(|slot| *slot.borrow_mut() = Some(controller.clone()));
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    // Observe live macOS System Settings -> Accessibility -> Reduce Transparency toggles!
    unsafe {
        let ws_nc = NSWorkspace::sharedWorkspace().notificationCenter();
        ws_nc.addObserver_selector_name_object(
            &controller,
            sel!(onAccessibilityDisplayChanged:),
            Some(NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification),
            None,
        );
    }
    let (menu, window_menu) = build_menu_bar(mtm, &controller);
    app.setMainMenu(Some(&menu));
    app.setWindowsMenu(Some(&window_menu));

    // Open initial terminal window immediately (100% event-driven — ZERO permanent repeating NSTimer!
    // PTY output, AppKit delegates, hotkeys, and exact one-shot deadlines drive all wakeups so
    // macOS achieves 0.00 idle wakeups/sec and full Apple Silicon App Nap energy efficiency!)
    controller.spawn_window(mtm, false);
    schedule_main_wakeup();

    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
    MAIN_CONTROLLER.with(|slot| *slot.borrow_mut() = None);
}
