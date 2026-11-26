//! Global hotkey registration via Carbon `RegisterEventHotKey` (works across all Spaces/apps,
//! matching iTerm2's global hotkey window toggle).

use iterm_profile::{modifiers, HotkeyConfig};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type OSStatus = i32;

type EventHandlerProcPtr = unsafe extern "C" fn(
    next_handler: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> OSStatus;

// Carbon constants
const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const CMD_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        in_target: EventTargetRef,
        in_handler: EventHandlerProcPtr,
        in_num_types: u32,
        in_list: *const EventTypeSpec,
        in_user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        in_hot_key_code: u32,
        in_hot_key_modifiers: u32,
        in_hot_key_id: EventHotKeyID,
        in_target: EventTargetRef,
        in_options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(in_hot_key: EventHotKeyRef) -> OSStatus;
}

/// Set to `true` by the Carbon event handler whenever the user presses the global hotkey.
/// Polled/consumed immediately on the main AppKit runloop to toggle the hotkey window!
pub static HOTKEY_TRIGGERED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn hotkey_callback(
    _next_handler: EventHandlerCallRef,
    _event: EventRef,
    _user_data: *mut c_void,
) -> OSStatus {
    HOTKEY_TRIGGERED.store(true, Ordering::Release);
    crate::macos::app::schedule_main_wakeup();
    0
}

fn appkit_mods_to_carbon(mods: u32) -> u32 {
    let mut out = 0u32;
    if (mods & modifiers::COMMAND) != 0 {
        out |= CMD_KEY;
    }
    if (mods & modifiers::OPTION) != 0 {
        out |= OPTION_KEY;
    }
    if (mods & modifiers::CONTROL) != 0 {
        out |= CONTROL_KEY;
    }
    if (mods & modifiers::SHIFT) != 0 {
        out |= SHIFT_KEY;
    }
    out
}

pub struct GlobalHotkey {
    hotkey_ref: EventHotKeyRef,
    _handler_ref: EventHandlerRef,
}

impl GlobalHotkey {
    pub fn register(cfg: &HotkeyConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        unsafe {
            let target = GetApplicationEventTarget();
            if target.is_null() {
                return None;
            }
            let spec = EventTypeSpec {
                event_class: K_EVENT_CLASS_KEYBOARD,
                event_kind: K_EVENT_HOT_KEY_PRESSED,
            };
            let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
            let st = InstallEventHandler(
                target,
                hotkey_callback,
                1,
                &spec,
                std::ptr::null_mut(),
                &mut handler_ref,
            );
            if st != 0 {
                log::warn!("InstallEventHandler failed with OSStatus {st}");
                return None;
            }

            let hk_id = EventHotKeyID {
                signature: u32::from_be_bytes(*b"ITMN"),
                id: 1,
            };
            let carbon_mods = appkit_mods_to_carbon(cfg.modifiers);
            let mut hotkey_ref: EventHotKeyRef = std::ptr::null_mut();
            let reg = RegisterEventHotKey(
                cfg.key_code as u32,
                carbon_mods,
                hk_id,
                target,
                0,
                &mut hotkey_ref,
            );
            if reg != 0 {
                log::warn!("RegisterEventHotKey failed with OSStatus {reg}");
                return None;
            }
            log::info!(
                "Registered global hotkey: key_code={} carbon_mods=0x{:x}",
                cfg.key_code,
                carbon_mods
            );
            Some(Self {
                hotkey_ref,
                _handler_ref: handler_ref,
            })
        }
    }
}

impl Drop for GlobalHotkey {
    fn drop(&mut self) {
        if !self.hotkey_ref.is_null() {
            unsafe {
                let _ = UnregisterEventHotKey(self.hotkey_ref);
            }
        }
    }
}
