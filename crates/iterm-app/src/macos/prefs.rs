//! Native macOS Preferences window (`Cmd+,`) with live Apply & Save across all open windows,
//! plus one-click import of iTerm2 `com.googlecode.iterm2.plist` and `.itermcolors` presets.

use iterm_profile::{AppConfig, CursorStyle};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSModalResponseOK, NSOpenPanel, NSPopUpButton, NSSlider, NSTextField, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::path::PathBuf;

pub struct PrefsIvars {
    window: RefCell<Option<Retained<NSWindow>>>,
    pending_config: RefCell<Option<AppConfig>>,
    working_config: RefCell<AppConfig>,

    font_name_field: RefCell<Option<Retained<NSTextField>>>,
    font_size_field: RefCell<Option<Retained<NSTextField>>>,
    non_ascii_font_field: RefCell<Option<Retained<NSTextField>>>,
    use_non_ascii_check: RefCell<Option<Retained<NSButton>>>,
    ligatures_check: RefCell<Option<Retained<NSButton>>>,
    bright_bold_check: RefCell<Option<Retained<NSButton>>>,
    cursor_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    blink_cursor_check: RefCell<Option<Retained<NSButton>>>,
    transparency_slider: RefCell<Option<Retained<NSSlider>>>,
    transparency_value_label: RefCell<Option<Retained<NSTextField>>>,
    min_contrast_slider: RefCell<Option<Retained<NSSlider>>>,
    min_contrast_value_label: RefCell<Option<Retained<NSTextField>>>,
    copy_selection_check: RefCell<Option<Retained<NSButton>>>,
    osc52_check: RefCell<Option<Retained<NSButton>>>,
    dim_inactive_check: RefCell<Option<Retained<NSButton>>>,
    hide_single_tab_check: RefCell<Option<Retained<NSButton>>>,
    unlimited_sb_check: RefCell<Option<Retained<NSButton>>>,
    mouse_reporting_check: RefCell<Option<Retained<NSButton>>>,
    status_label: RefCell<Option<Retained<NSTextField>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PrefsIvars]
    pub struct PreferencesController;

    impl PreferencesController {
        #[unsafe(method(onSliderChanged:))]
        fn on_slider_changed(&self, _sender: Option<&AnyObject>) {
            if let (Some(sl), Some(lbl)) = (
                self.ivars().transparency_slider.borrow().as_ref(),
                self.ivars().transparency_value_label.borrow().as_ref(),
            ) {
                let pct = (sl.doubleValue() * 100.0).round() as i32;
                lbl.setStringValue(&NSString::from_str(&format!("{pct}%")));
            }
            if let (Some(sl), Some(lbl)) = (
                self.ivars().min_contrast_slider.borrow().as_ref(),
                self.ivars().min_contrast_value_label.borrow().as_ref(),
            ) {
                let pct = (sl.doubleValue() * 100.0).round() as i32;
                lbl.setStringValue(&NSString::from_str(&format!("{pct}%")));
            }
        }

        #[unsafe(method(onApplyAndSave:))]
        fn on_apply_and_save(&self, _sender: Option<&AnyObject>) {
            let updated = self.read_controls_into_config();
            let path = AppConfig::native_config_path();
            match updated.save_json(&path) {
                Ok(()) => {
                    self.set_status(&format!("Saved & applied ({})", path.display()));
                }
                Err(e) => {
                    self.set_status(&format!("Applied (save failed: {e})"));
                }
            }
            *self.ivars().working_config.borrow_mut() = updated.clone();
            *self.ivars().pending_config.borrow_mut() = Some(updated);
            crate::macos::app::schedule_main_wakeup();
        }

        #[unsafe(method(onImportPlist:))]
        fn on_import_plist(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(true);
            panel.setCanChooseDirectories(false);
            panel.setAllowsMultipleSelection(false);
            if panel.runModal() == NSModalResponseOK {
                if let Some(url) = panel.URL() {
                    if let Some(p) = url.path() {
                        let pb = PathBuf::from(p.to_string());
                        match AppConfig::from_iterm2_plist(&pb) {
                            Ok(imported) => {
                                let dest = AppConfig::native_config_path();
                                let _ = imported.save_json(&dest);
                                self.populate_controls(&imported);
                                self.set_status(&format!(
                                    "Imported {} profile(s), {} key bindings from plist",
                                    imported.profiles.len(),
                                    imported.default_profile().keymap.len()
                                ));
                                *self.ivars().working_config.borrow_mut() = imported.clone();
                                *self.ivars().pending_config.borrow_mut() = Some(imported);
                                crate::macos::app::schedule_main_wakeup();
                            }
                            Err(e) => {
                                self.set_status(&format!("Import plist failed: {e}"));
                            }
                        }
                    }
                }
            }
        }

        #[unsafe(method(onImportColors:))]
        fn on_import_colors(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(true);
            panel.setCanChooseDirectories(false);
            panel.setAllowsMultipleSelection(false);
            if panel.runModal() == NSModalResponseOK {
                if let Some(url) = panel.URL() {
                    if let Some(p) = url.path() {
                        let pb = PathBuf::from(p.to_string());
                        match AppConfig::load_itermcolors(&pb) {
                            Ok(scheme) => {
                                let mut cfg = self.read_controls_into_config();
                                cfg.default_profile_mut().colors = scheme;
                                let _ = cfg.save_json(&AppConfig::native_config_path());
                                self.set_status(&format!(
                                    "Applied color preset from {}",
                                    pb.file_name().and_then(|s| s.to_str()).unwrap_or("file")
                                ));
                                *self.ivars().working_config.borrow_mut() = cfg.clone();
                                *self.ivars().pending_config.borrow_mut() = Some(cfg);
                                crate::macos::app::schedule_main_wakeup();
                            }
                            Err(e) => {
                                self.set_status(&format!("Import .itermcolors failed: {e}"));
                            }
                        }
                    }
                }
            }
        }
    }
);

impl PreferencesController {
    pub fn new(mtm: MainThreadMarker, cfg: AppConfig) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(PrefsIvars {
            window: RefCell::new(None),
            pending_config: RefCell::new(None),
            working_config: RefCell::new(cfg),
            font_name_field: RefCell::new(None),
            font_size_field: RefCell::new(None),
            non_ascii_font_field: RefCell::new(None),
            use_non_ascii_check: RefCell::new(None),
            ligatures_check: RefCell::new(None),
            bright_bold_check: RefCell::new(None),
            cursor_popup: RefCell::new(None),
            blink_cursor_check: RefCell::new(None),
            transparency_slider: RefCell::new(None),
            transparency_value_label: RefCell::new(None),
            min_contrast_slider: RefCell::new(None),
            min_contrast_value_label: RefCell::new(None),
            copy_selection_check: RefCell::new(None),
            osc52_check: RefCell::new(None),
            dim_inactive_check: RefCell::new(None),
            hide_single_tab_check: RefCell::new(None),
            unlimited_sb_check: RefCell::new(None),
            mouse_reporting_check: RefCell::new(None),
            status_label: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    pub fn take_pending_config(&self) -> Option<AppConfig> {
        self.ivars().pending_config.borrow_mut().take()
    }

    pub fn show(&self, mtm: MainThreadMarker, current_cfg: &AppConfig) {
        *self.ivars().working_config.borrow_mut() = current_cfg.clone();
        if self.ivars().window.borrow().is_none() {
            self.build_window(mtm);
        }
        self.populate_controls(current_cfg);
        if let Some(ref win) = *self.ivars().window.borrow() {
            win.makeKeyAndOrderFront(None);
        }
    }

    fn set_status(&self, msg: &str) {
        if let Some(ref lbl) = *self.ivars().status_label.borrow() {
            lbl.setStringValue(&NSString::from_str(msg));
        }
    }

    fn build_window(&self, mtm: MainThreadMarker) {
        let width = 520.0;
        let height = 500.0;
        let frame = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(width, height));
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;

        let win = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        win.setTitle(&NSString::from_str("JTerm Preferences"));
        unsafe { win.setReleasedWhenClosed(false) };
        win.center();

        let content = NSView::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
        );

        let add_label = |text: &str, x: f64, y: f64, w: f64, bold: bool| -> Retained<NSTextField> {
            let lbl = NSTextField::labelWithString(&NSString::from_str(text), mtm);
            lbl.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, 20.0)));
            if bold {
                lbl.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            }
            content.addSubview(&lbl);
            lbl
        };

        let add_field = |x: f64, y: f64, w: f64| -> Retained<NSTextField> {
            let f = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
            f.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, 24.0)));
            f.setEditable(true);
            f.setBezeled(true);
            f.setDrawsBackground(true);
            content.addSubview(&f);
            f
        };

        let add_check = |title: &str, x: f64, y: f64, w: f64| -> Retained<NSButton> {
            let b = unsafe {
                NSButton::checkboxWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
            };
            b.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, 22.0)));
            content.addSubview(&b);
            b
        };

        // ---- Typography section ----
        let mut y = height - 38.0;
        add_label("Typography & Rendering", 20.0, y, 300.0, true);
        y -= 30.0;

        add_label("Normal Font:", 20.0, y + 2.0, 115.0, false);
        let font_name = add_field(140.0, y, 220.0);
        add_label("Size (pt):", 372.0, y + 2.0, 62.0, false);
        let font_size = add_field(438.0, y, 60.0);
        y -= 32.0;

        add_label("Non-ASCII Font:", 20.0, y + 2.0, 115.0, false);
        let non_ascii = add_field(140.0, y, 220.0);
        let use_na = add_check("Use Non-ASCII", 372.0, y + 1.0, 130.0);
        y -= 30.0;

        let ligatures = add_check("Enable ASCII Ligatures (FiraCode / JetBrainsMono)", 140.0, y, 355.0);
        y -= 26.0;
        let bright_bold = add_check("Use bright colors for bold text", 140.0, y, 320.0);
        y -= 36.0;

        // ---- Appearance section ----
        add_label("Appearance & Cursor", 20.0, y, 300.0, true);
        y -= 30.0;

        add_label("Cursor Style:", 20.0, y + 2.0, 115.0, false);
        let cursor_popup = NSPopUpButton::initWithFrame_pullsDown(
            mtm.alloc(),
            NSRect::new(NSPoint::new(140.0, y), NSSize::new(150.0, 26.0)),
            false,
        );
        cursor_popup.addItemWithTitle(&NSString::from_str("Block"));
        cursor_popup.addItemWithTitle(&NSString::from_str("Vertical Bar"));
        cursor_popup.addItemWithTitle(&NSString::from_str("Underline"));
        content.addSubview(&cursor_popup);
        let blink_cursor = add_check("Blinking cursor", 310.0, y + 2.0, 160.0);
        y -= 32.0;

        add_label("Transparency:", 20.0, y + 2.0, 115.0, false);
        let trans_slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                0.0,
                0.0,
                0.85,
                Some(self),
                Some(sel!(onSliderChanged:)),
                mtm,
            )
        };
        trans_slider.setFrame(NSRect::new(NSPoint::new(140.0, y + 2.0), NSSize::new(280.0, 20.0)));
        content.addSubview(&trans_slider);
        let trans_lbl = add_label("0%", 432.0, y + 2.0, 60.0, false);
        y -= 30.0;

        add_label("Min Contrast:", 20.0, y + 2.0, 115.0, false);
        let mc_slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                0.0,
                0.0,
                1.0,
                Some(self),
                Some(sel!(onSliderChanged:)),
                mtm,
            )
        };
        mc_slider.setFrame(NSRect::new(NSPoint::new(140.0, y + 2.0), NSSize::new(280.0, 20.0)));
        content.addSubview(&mc_slider);
        let mc_lbl = add_label("0%", 432.0, y + 2.0, 60.0, false);
        y -= 36.0;

        // ---- Behavior section ----
        add_label("Behavior & iTerm2 Compatibility", 20.0, y, 300.0, true);
        y -= 28.0;

        let copy_sel = add_check("Copy to clipboard on selection", 20.0, y, 235.0);
        let osc52 = add_check("Allow OSC 52 clipboard access", 265.0, y, 235.0);
        y -= 26.0;

        let dim_inactive = add_check("Dim inactive split panes", 20.0, y, 235.0);
        let hide_single_tab = add_check("Hide tab bar when single tab", 265.0, y, 235.0);
        y -= 26.0;

        let unlimited_sb = add_check("Unlimited scrollback (1M cap)", 20.0, y, 235.0);
        let mouse_rep = add_check("Enable mouse reporting (tmux)", 265.0, y, 235.0);

        // ---- Bottom action bar ----
        let status = add_label("Changes apply immediately to all open windows.", 20.0, 56.0, 480.0, false);

        let import_plist_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Import iTerm2 Plist…"),
                Some(self),
                Some(sel!(onImportPlist:)),
                mtm,
            )
        };
        import_plist_btn.setFrame(NSRect::new(NSPoint::new(20.0, 16.0), NSSize::new(155.0, 30.0)));
        content.addSubview(&import_plist_btn);

        let import_colors_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Import .itermcolors…"),
                Some(self),
                Some(sel!(onImportColors:)),
                mtm,
            )
        };
        import_colors_btn.setFrame(NSRect::new(NSPoint::new(182.0, 16.0), NSSize::new(155.0, 30.0)));
        content.addSubview(&import_colors_btn);

        let apply_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Apply & Save"),
                Some(self),
                Some(sel!(onApplyAndSave:)),
                mtm,
            )
        };
        apply_btn.setFrame(NSRect::new(NSPoint::new(375.0, 16.0), NSSize::new(125.0, 30.0)));
        apply_btn.setKeyEquivalent(&NSString::from_str("\r"));
        content.addSubview(&apply_btn);

        win.setContentView(Some(&content));

        *self.ivars().font_name_field.borrow_mut() = Some(font_name);
        *self.ivars().font_size_field.borrow_mut() = Some(font_size);
        *self.ivars().non_ascii_font_field.borrow_mut() = Some(non_ascii);
        *self.ivars().use_non_ascii_check.borrow_mut() = Some(use_na);
        *self.ivars().ligatures_check.borrow_mut() = Some(ligatures);
        *self.ivars().bright_bold_check.borrow_mut() = Some(bright_bold);
        *self.ivars().cursor_popup.borrow_mut() = Some(cursor_popup);
        *self.ivars().blink_cursor_check.borrow_mut() = Some(blink_cursor);
        *self.ivars().transparency_slider.borrow_mut() = Some(trans_slider);
        *self.ivars().transparency_value_label.borrow_mut() = Some(trans_lbl);
        *self.ivars().min_contrast_slider.borrow_mut() = Some(mc_slider);
        *self.ivars().min_contrast_value_label.borrow_mut() = Some(mc_lbl);
        *self.ivars().copy_selection_check.borrow_mut() = Some(copy_sel);
        *self.ivars().osc52_check.borrow_mut() = Some(osc52);
        *self.ivars().dim_inactive_check.borrow_mut() = Some(dim_inactive);
        *self.ivars().hide_single_tab_check.borrow_mut() = Some(hide_single_tab);
        *self.ivars().unlimited_sb_check.borrow_mut() = Some(unlimited_sb);
        *self.ivars().mouse_reporting_check.borrow_mut() = Some(mouse_rep);
        *self.ivars().status_label.borrow_mut() = Some(status);
        *self.ivars().window.borrow_mut() = Some(win);
    }

    fn populate_controls(&self, cfg: &AppConfig) {
        let p = cfg.default_profile();
        let set_check = |cell: &RefCell<Option<Retained<NSButton>>>, val: bool| {
            if let Some(ref b) = *cell.borrow() {
                b.setState(if val { NSControlStateValueOn } else { NSControlStateValueOff });
            }
        };

        if let Some(ref f) = *self.ivars().font_name_field.borrow() {
            f.setStringValue(&NSString::from_str(&p.normal_font.name));
        }
        if let Some(ref f) = *self.ivars().font_size_field.borrow() {
            f.setStringValue(&NSString::from_str(&format!("{:.1}", p.normal_font.size)));
        }
        if let Some(ref f) = *self.ivars().non_ascii_font_field.borrow() {
            f.setStringValue(&NSString::from_str(&p.non_ascii_font.name));
        }
        set_check(&self.ivars().use_non_ascii_check, p.use_non_ascii_font);
        set_check(&self.ivars().ligatures_check, p.ascii_ligatures);
        set_check(&self.ivars().bright_bold_check, p.use_bright_bold);
        if let Some(ref pop) = *self.ivars().cursor_popup.borrow() {
            let idx = match p.cursor_style {
                CursorStyle::Block => 0,
                CursorStyle::VerticalBar => 1,
                CursorStyle::Underline => 2,
            };
            pop.selectItemAtIndex(idx);
        }
        set_check(&self.ivars().blink_cursor_check, p.blinking_cursor);
        if let Some(ref sl) = *self.ivars().transparency_slider.borrow() {
            sl.setDoubleValue(p.transparency as f64);
        }
        if let Some(ref lbl) = *self.ivars().transparency_value_label.borrow() {
            lbl.setStringValue(&NSString::from_str(&format!(
                "{}%",
                (p.transparency * 100.0).round() as i32
            )));
        }
        if let Some(ref sl) = *self.ivars().min_contrast_slider.borrow() {
            sl.setDoubleValue(p.minimum_contrast as f64);
        }
        if let Some(ref lbl) = *self.ivars().min_contrast_value_label.borrow() {
            lbl.setStringValue(&NSString::from_str(&format!(
                "{}%",
                (p.minimum_contrast * 100.0).round() as i32
            )));
        }
        set_check(&self.ivars().copy_selection_check, cfg.copy_selection);
        set_check(&self.ivars().osc52_check, cfg.allow_clipboard_access);
        set_check(&self.ivars().dim_inactive_check, cfg.dim_inactive_split_panes);
        set_check(&self.ivars().hide_single_tab_check, cfg.hide_tab_bar_when_single);
        set_check(&self.ivars().unlimited_sb_check, p.unlimited_scrollback);
        set_check(&self.ivars().mouse_reporting_check, p.mouse_reporting);
    }

    fn read_controls_into_config(&self) -> AppConfig {
        let mut cfg = self.ivars().working_config.borrow().clone();
        let get_check = |cell: &RefCell<Option<Retained<NSButton>>>, default: bool| -> bool {
            cell.borrow()
                .as_ref()
                .map(|b| b.state() == NSControlStateValueOn)
                .unwrap_or(default)
        };

        cfg.copy_selection = get_check(&self.ivars().copy_selection_check, cfg.copy_selection);
        cfg.allow_clipboard_access = get_check(&self.ivars().osc52_check, cfg.allow_clipboard_access);
        cfg.dim_inactive_split_panes = get_check(&self.ivars().dim_inactive_check, cfg.dim_inactive_split_panes);
        cfg.hide_tab_bar_when_single =
            get_check(&self.ivars().hide_single_tab_check, cfg.hide_tab_bar_when_single);

        let default_guid = cfg.default_profile_guid.clone();
        let target_idx = default_guid
            .as_deref()
            .and_then(|g| cfg.profiles.iter().position(|p| p.guid == g))
            .unwrap_or(0);
        if let Some(p) = cfg.profiles.get_mut(target_idx) {
            if let Some(ref f) = *self.ivars().font_name_field.borrow() {
                let s = f.stringValue().to_string().trim().to_string();
                if !s.is_empty() {
                    p.normal_font.name = s;
                }
            }
            if let Some(ref f) = *self.ivars().font_size_field.borrow() {
                if let Ok(sz) = f.stringValue().to_string().trim().parse::<f32>() {
                    p.normal_font.size = sz.clamp(8.0, 72.0);
                    p.non_ascii_font.size = p.normal_font.size;
                }
            }
            if let Some(ref f) = *self.ivars().non_ascii_font_field.borrow() {
                let s = f.stringValue().to_string().trim().to_string();
                if !s.is_empty() {
                    p.non_ascii_font.name = s;
                }
            }
            p.use_non_ascii_font = get_check(&self.ivars().use_non_ascii_check, p.use_non_ascii_font);
            p.ascii_ligatures = get_check(&self.ivars().ligatures_check, p.ascii_ligatures);
            p.use_bright_bold = get_check(&self.ivars().bright_bold_check, p.use_bright_bold);
            if let Some(ref pop) = *self.ivars().cursor_popup.borrow() {
                p.cursor_style = match pop.indexOfSelectedItem() {
                    1 => CursorStyle::VerticalBar,
                    2 => CursorStyle::Underline,
                    _ => CursorStyle::Block,
                };
            }
            p.blinking_cursor = get_check(&self.ivars().blink_cursor_check, p.blinking_cursor);
            if let Some(ref sl) = *self.ivars().transparency_slider.borrow() {
                p.transparency = (sl.doubleValue() as f32).clamp(0.0, 0.85);
            }
            if let Some(ref sl) = *self.ivars().min_contrast_slider.borrow() {
                p.minimum_contrast = (sl.doubleValue() as f32).clamp(0.0, 1.0);
            }
            p.unlimited_scrollback = get_check(&self.ivars().unlimited_sb_check, p.unlimited_scrollback);
            p.scrollback_lines = if p.unlimited_scrollback { 1_000_000 } else { 10_000 };
            p.mouse_reporting = get_check(&self.ivars().mouse_reporting_check, p.mouse_reporting);
        }
        cfg
    }
}
