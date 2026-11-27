#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use iterm_profile::{modifiers, AppConfig, KeyAction, OptionKeyMode, Profile};
use iterm_vt::TermModes;

/// High-level UI / window command triggered by a key shortcut or iTerm2 KeyAction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    SendBytes(Vec<u8>),
    NewWindow,
    NewTab,
    ClosePaneOrTab,
    SplitVertical,
    SplitHorizontal,
    ToggleZoomPane,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    NextPane,
    PrevPane,
    NextTab,
    PrevTab,
    SelectTab(usize),
    SelectLastTab,
    MoveTabLeft,
    MoveTabRight,
    Copy,
    Paste,
    AdvancedPaste,
    ClearBuffer,
    ToggleFind,
    FindNext,
    FindPrev,
    FontIncrease,
    FontDecrease,
    FontReset,
    ToggleFullscreen,
    ToggleMouseReporting,
    ToggleTransparency,
    ToggleTransfers,
    TogglePerfHud,
    JumpPrevPrompt,
    JumpNextPrompt,
    ScrollPageUp,
    ScrollPageDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollHome,
    ScrollEnd,
    OpenPreferences,
    None,
}

/// Logical key event extracted from macOS `NSEvent`.
#[derive(Debug, Clone)]
pub struct KeyEventInput {
    /// First UTF-16 code unit from `charactersIgnoringModifiers` (matches iTerm2's hex key codes, e.g. 0xF700 for Up).
    pub key_code_char: u32,
    /// Actual typed text from `characters` (after keyboard layout / shift / dead keys).
    pub characters: String,
    /// macOS hardware virtual key code (`NSEvent.keyCode`).
    pub virtual_key: u16,
    /// Masked AppKit modifier flags (`iterm_profile::modifiers`).
    pub mods: u32,
    /// True if Right Option was pressed instead of Left Option.
    pub is_right_option: bool,
}

/// Resolve a macOS key event into a `UiCommand` honoring:
/// 1. User's iTerm2 `GlobalKeyMap`
/// 2. Active profile's iTerm2 `Keyboard Map` (all 59+ user bindings!)
/// 3. Standard iTerm2 macOS `Cmd+*` shortcuts
/// 4. Terminal key encoding (DECCKM arrows, F1-F12, Ctrl+key, Option Meta/Esc+, UTF-8 text)
#[allow(dead_code)]
pub fn resolve_key_event(
    ev: &KeyEventInput,
    app_cfg: &AppConfig,
    profile: &Profile,
    modes: TermModes,
) -> UiCommand {
    resolve_key_event_with_kitty(ev, app_cfg, profile, modes, 0)
}

/// Resolve a macOS key event with optional Kitty keyboard protocol flags (`CSI > u`).
pub fn resolve_key_event_with_kitty(
    ev: &KeyEventInput,
    app_cfg: &AppConfig,
    profile: &Profile,
    modes: TermModes,
    kitty_flags: u8,
) -> UiCommand {
    let mods = ev.mods & modifiers::RELEVANT_MASK;

    // 1. GlobalKeyMap lookup
    if let Some(action) = app_cfg
        .global_keymap
        .lookup(ev.key_code_char, mods, Some(ev.virtual_key))
    {
        if let Some(cmd) = key_action_to_command(action) {
            return cmd;
        }
    }

    // 2. Profile Keyboard Map lookup (e.g. Opt+Left -> ESC b, Cmd+Backspace -> 0x15, Opt+Backspace -> 0x1b 0x7f)
    if let Some(action) = profile
        .keymap
        .lookup(ev.key_code_char, mods, Some(ev.virtual_key))
    {
        if let Some(cmd) = key_action_to_command(action) {
            return cmd;
        }
    }

    // 3. Built-in macOS Command shortcuts (matching iTerm2 muscle memory)
    let cmd = (mods & modifiers::COMMAND) != 0;
    let shift = (mods & modifiers::SHIFT) != 0;
    let opt = (mods & modifiers::OPTION) != 0;
    let ctrl = (mods & modifiers::CONTROL) != 0;

    if cmd && !ctrl {
        let ch = char::from_u32(ev.key_code_char)
            .map(|c| c.to_ascii_lowercase())
            .unwrap_or('\0');
        match (ch, shift, opt) {
            ('n', false, false) => return UiCommand::NewWindow,
            ('t', false, false) => return UiCommand::NewTab,
            ('w', false, false) => return UiCommand::ClosePaneOrTab,
            ('d', false, false) => return UiCommand::SplitVertical,
            ('d', true, false) => return UiCommand::SplitHorizontal,
            ('\r', true, false) => return UiCommand::ToggleZoomPane,
            (']', false, false) => return UiCommand::NextPane,
            ('[', false, false) => return UiCommand::PrevPane,
            ('}', _, false) | (']', true, false) => return UiCommand::NextTab,
            ('{', _, false) | ('[', true, false) => return UiCommand::PrevTab,
            ('c', false, false) => return UiCommand::Copy,
            ('v', false, false) => return UiCommand::Paste,
            ('v', true, false) => return UiCommand::AdvancedPaste,
            ('k', false, false) => return UiCommand::ClearBuffer,
            ('f', false, false) => return UiCommand::ToggleFind,
            ('g', false, false) => return UiCommand::FindNext,
            ('g', true, false) => return UiCommand::FindPrev,
            ('+', _, false) | ('=', _, false) => return UiCommand::FontIncrease,
            ('-', false, false) => return UiCommand::FontDecrease,
            ('0', false, false) => return UiCommand::FontReset,
            (',', false, false) => return UiCommand::OpenPreferences,
            ('u', false, false) => return UiCommand::ToggleTransparency,
            ('l', false, true) => return UiCommand::ToggleTransfers,
            ('i', false, true) => return UiCommand::TogglePerfHud,
            ('1'..='8', false, false) => {
                let idx = (ch as u8 - b'1') as usize;
                return UiCommand::SelectTab(idx);
            }
            ('9', false, false) => return UiCommand::SelectLastTab,
            _ => {}
        }
        // Cmd+Opt+Arrow pane navigation
        if opt && !shift {
            match ev.key_code_char {
                0xF700 => return UiCommand::FocusPaneUp,
                0xF701 => return UiCommand::FocusPaneDown,
                0xF702 => return UiCommand::FocusPaneLeft,
                0xF703 => return UiCommand::FocusPaneRight,
                _ => {}
            }
        }
        // Cmd+Shift+Up/Down: jump between shell integration prompts (OSC 133)
        if shift && !opt {
            match ev.key_code_char {
                0xF700 => return UiCommand::JumpPrevPrompt,
                0xF701 => return UiCommand::JumpNextPrompt,
                _ => {}
            }
        }
        // Cmd+Enter: toggle fullscreen
        if ev.virtual_key == 36 && !shift && !opt {
            return UiCommand::ToggleFullscreen;
        }
        return UiCommand::None;
    }

    // Shift+PageUp / PageDown / Home / End scrollback navigation
    if shift && !ctrl && !opt {
        match ev.key_code_char {
            0xF72C => return UiCommand::ScrollPageUp,
            0xF72D => return UiCommand::ScrollPageDown,
            0xF729 => return UiCommand::ScrollHome,
            0xF72B => return UiCommand::ScrollEnd,
            _ => {}
        }
    }

    // 4. Kitty keyboard protocol disambiguation (`CSI > 1 u` — neovim/helix/fish/Copilot/Claude CLI)
    if (kitty_flags & 1) != 0 {
        if let Some(bytes) = encode_kitty_key(ev, kitty_flags) {
            return UiCommand::SendBytes(bytes);
        }
    }

    // 5. Standard terminal key encoding
    if let Some(bytes) = encode_terminal_key(ev, profile, modes) {
        return UiCommand::SendBytes(bytes);
    }

    UiCommand::None
}

/// Encode keys using the Kitty keyboard protocol (`CSI <unicode-codepoint>;<modifiers> u`)
/// when disambiguate mode (flag bit 0) is active on the terminal screen.
fn encode_kitty_key(ev: &KeyEventInput, kitty_flags: u8) -> Option<Vec<u8>> {
    let mods = ev.mods & modifiers::RELEVANT_MASK;
    let mod_param = xterm_mod_param(mods);
    let report_all = (kitty_flags & 8) != 0;

    // Special disambiguated keys (Esc, Enter, Tab, Backspace)
    let special_cp: Option<u32> = if ev.virtual_key == 53 || ev.key_code_char == 0x1B {
        Some(27)
    } else if ev.virtual_key == 36 || ev.key_code_char == 0x0D {
        if mod_param > 1 || report_all { Some(13) } else { None }
    } else if ev.key_code_char == 0x09 || ev.key_code_char == 0x19 {
        if mod_param > 1 || report_all { Some(9) } else { None }
    } else if ev.virtual_key == 51 || ev.key_code_char == 0x7F {
        if mod_param > 1 || report_all { Some(127) } else { None }
    } else {
        None
    };

    if let Some(cp) = special_cp {
        return Some(if mod_param > 1 {
            format!("\x1b[{cp};{mod_param}u").into_bytes()
        } else {
            format!("\x1b[{cp}u").into_bytes()
        });
    }

    // Disambiguate Ctrl / Alt modified Unicode keys (e.g. Ctrl+I vs Tab, Ctrl+[ vs Esc, Ctrl+Shift+A),
    // or all printable keys when Kitty `report_all` (flag bit 3) is active!
    let has_ctrl_or_alt = (mods & (modifiers::CONTROL | modifiers::OPTION)) != 0;
    if (has_ctrl_or_alt || report_all) && (0x20..=0x7E).contains(&ev.key_code_char) {
        let base_cp = char::from_u32(ev.key_code_char)
            .map(|c| c.to_ascii_lowercase() as u32)
            .unwrap_or(ev.key_code_char);
        return Some(if mod_param > 1 {
            format!("\x1b[{base_cp};{mod_param}u").into_bytes()
        } else {
            format!("\x1b[{base_cp}u").into_bytes()
        });
    }
    None
}

fn key_action_to_command(action: &KeyAction) -> Option<UiCommand> {
    if let Some(bytes) = action.to_pty_bytes() {
        return Some(UiCommand::SendBytes(bytes));
    }
    match action {
        KeyAction::Ignore => Some(UiCommand::None),
        KeyAction::Bypass => None,
        KeyAction::NextSession => Some(UiCommand::NextTab),
        KeyAction::PreviousSession => Some(UiCommand::PrevTab),
        KeyAction::NextPane => Some(UiCommand::NextPane),
        KeyAction::PreviousPane => Some(UiCommand::PrevPane),
        KeyAction::SelectPaneLeft => Some(UiCommand::FocusPaneLeft),
        KeyAction::SelectPaneRight => Some(UiCommand::FocusPaneRight),
        KeyAction::SelectPaneAbove => Some(UiCommand::FocusPaneUp),
        KeyAction::SelectPaneBelow => Some(UiCommand::FocusPaneDown),
        KeyAction::ScrollEnd => Some(UiCommand::ScrollEnd),
        KeyAction::ScrollHome => Some(UiCommand::ScrollHome),
        KeyAction::ScrollLineDown => Some(UiCommand::ScrollLineDown),
        KeyAction::ScrollLineUp => Some(UiCommand::ScrollLineUp),
        KeyAction::ScrollPageDown => Some(UiCommand::ScrollPageDown),
        KeyAction::ScrollPageUp => Some(UiCommand::ScrollPageUp),
        KeyAction::ToggleFullscreen => Some(UiCommand::ToggleFullscreen),
        KeyAction::NewWindowWithProfile(_) => Some(UiCommand::NewWindow),
        KeyAction::NewTabWithProfile(_) => Some(UiCommand::NewTab),
        KeyAction::SplitHorizontallyWithProfile(_) => Some(UiCommand::SplitHorizontal),
        KeyAction::SplitVerticallyWithProfile(_) => Some(UiCommand::SplitVertical),
        KeyAction::MoveTabLeft => Some(UiCommand::MoveTabLeft),
        KeyAction::MoveTabRight => Some(UiCommand::MoveTabRight),
        KeyAction::FindAgainDown => Some(UiCommand::FindNext),
        KeyAction::FindAgainUp => Some(UiCommand::FindPrev),
        KeyAction::ToggleMouseReporting => Some(UiCommand::ToggleMouseReporting),
        KeyAction::PasteSpecial => Some(UiCommand::AdvancedPaste),
        _ => None,
    }
}

/// Compute xterm modifier parameter (`1 + shift + 2*alt + 4*ctrl`).
#[inline]
fn xterm_mod_param(mods: u32) -> u8 {
    let mut m = 1u8;
    if (mods & modifiers::SHIFT) != 0 {
        m += 1;
    }
    if (mods & modifiers::OPTION) != 0 {
        m += 2;
    }
    if (mods & modifiers::CONTROL) != 0 {
        m += 4;
    }
    m
}

fn encode_terminal_key(
    ev: &KeyEventInput,
    profile: &Profile,
    modes: TermModes,
) -> Option<Vec<u8>> {
    let mods = ev.mods & modifiers::RELEVANT_MASK;
    let ctrl = (mods & modifiers::CONTROL) != 0;
    let opt = (mods & modifiers::OPTION) != 0;
    let shift = (mods & modifiers::SHIFT) != 0;
    let app_cursor = modes.contains(TermModes::APPLICATION_CURSOR);
    let mod_param = xterm_mod_param(mods);

    // Special AppKit function/navigation keys (0xF700..0xF73F)
    match ev.key_code_char {
        // Arrows
        0xF700 => return Some(encode_arrow(b'A', app_cursor, mod_param)),
        0xF701 => return Some(encode_arrow(b'B', app_cursor, mod_param)),
        0xF703 => return Some(encode_arrow(b'C', app_cursor, mod_param)),
        0xF702 => return Some(encode_arrow(b'D', app_cursor, mod_param)),
        // Home / End
        0xF729 => return Some(encode_arrow(b'H', app_cursor, mod_param)),
        0xF72B => return Some(encode_arrow(b'F', app_cursor, mod_param)),
        // Page Up / Down / Delete / Insert
        0xF72C => return Some(encode_tilde_key(5, mod_param)),
        0xF72D => return Some(encode_tilde_key(6, mod_param)),
        0xF728 => return Some(encode_tilde_key(3, mod_param)),
        0xF727 => return Some(encode_tilde_key(2, mod_param)),
        // F1..F4
        0xF704 => return Some(encode_ss3_fn(b'P', mod_param)),
        0xF705 => return Some(encode_ss3_fn(b'Q', mod_param)),
        0xF706 => return Some(encode_ss3_fn(b'R', mod_param)),
        0xF707 => return Some(encode_ss3_fn(b'S', mod_param)),
        // F5..F12
        0xF708 => return Some(encode_tilde_key(15, mod_param)),
        0xF709 => return Some(encode_tilde_key(17, mod_param)),
        0xF70A => return Some(encode_tilde_key(18, mod_param)),
        0xF70B => return Some(encode_tilde_key(19, mod_param)),
        0xF70C => return Some(encode_tilde_key(20, mod_param)),
        0xF70D => return Some(encode_tilde_key(21, mod_param)),
        0xF70E => return Some(encode_tilde_key(23, mod_param)),
        0xF70F => return Some(encode_tilde_key(24, mod_param)),
        _ => {}
    }

    // Backtab (Shift+Tab)
    if (ev.key_code_char == 0x19 || (ev.key_code_char == 0x09 && shift)) && !ctrl {
        return Some(b"\x1b[Z".to_vec());
    }
    // Enter / Return
    if ev.virtual_key == 36 || ev.key_code_char == 0x0D || ev.key_code_char == 0x03 {
        return Some(if modes.contains(TermModes::LINEFEED_NEWLINE) {
            b"\r\n".to_vec()
        } else {
            b"\r".to_vec()
        });
    }
    // Backspace (macOS sends 0x7F)
    if ev.virtual_key == 51 || ev.key_code_char == 0x7F {
        if opt {
            return Some(vec![0x1B, 0x7F]);
        }
        return Some(vec![0x7F]);
    }
    // Escape
    if ev.virtual_key == 53 || ev.key_code_char == 0x1B {
        return Some(vec![0x1B]);
    }

    // Control + ASCII key (critical for tmux `C-a`, vim `C-v`, bash `C-c`, `C-d`, `C-r`, etc.)
    if ctrl && ev.key_code_char <= 0x7F {
        let kc = ev.key_code_char as u8;
        let ctrl_byte: Option<u8> = match kc {
            b'@' | b' ' | b'2' => Some(0x00),
            b'a'..=b'z' => Some(kc - b'a' + 1),
            b'A'..=b'Z' => Some(kc - b'A' + 1),
            b'[' | b'3' => Some(0x1B),
            b'\\' | b'4' => Some(0x1C),
            b']' | b'5' => Some(0x1D),
            b'^' | b'6' => Some(0x1E),
            b'_' | b'-' | b'7' => Some(0x1F),
            b'?' | b'8' => Some(0x7F),
            _ => None,
        };
        if let Some(b) = ctrl_byte {
            return Some(if opt { vec![0x1B, b] } else { vec![b] });
        }
    }

    // Option key handling (`Option Key Sends`: Normal / Meta / Esc+)
    if opt {
        let opt_mode = if ev.is_right_option {
            profile.right_option_key_sends
        } else {
            profile.option_key_sends
        };
        match opt_mode {
            OptionKeyMode::EscPlus => {
                if let Some(ch) = char::from_u32(ev.key_code_char) {
                    let mut buf = vec![0x1B];
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                    return Some(buf);
                }
            }
            OptionKeyMode::Meta => {
                if ev.key_code_char < 0x80 {
                    return Some(vec![(ev.key_code_char as u8) | 0x80]);
                }
            }
            OptionKeyMode::Normal => {}
        }
    }

    if !ev.characters.is_empty() {
        return Some(ev.characters.as_bytes().to_vec());
    }
    None
}

fn encode_arrow(code: u8, app_cursor: bool, mod_param: u8) -> Vec<u8> {
    if mod_param > 1 {
        format!("\x1b[1;{mod_param}{}", code as char).into_bytes()
    } else if app_cursor {
        vec![0x1B, b'O', code]
    } else {
        vec![0x1B, b'[', code]
    }
}

fn encode_ss3_fn(code: u8, mod_param: u8) -> Vec<u8> {
    if mod_param > 1 {
        format!("\x1b[1;{mod_param}{}", code as char).into_bytes()
    } else {
        vec![0x1B, b'O', code]
    }
}

fn encode_tilde_key(num: u8, mod_param: u8) -> Vec<u8> {
    if mod_param > 1 {
        format!("\x1b[{num};{mod_param}~").into_bytes()
    } else {
        format!("\x1b[{num}~").into_bytes()
    }
}

/// Encode mouse press/release/motion/wheel event for tmux/vim/TUI mouse reporting (`SGR 1006` / `VT200` / `X10`).
pub fn encode_mouse_event(
    modes: TermModes,
    button: u8,
    pressed: bool,
    motion: bool,
    col_0: usize,
    row_0: usize,
    mods: u32,
) -> Option<Vec<u8>> {
    let want_any = modes.contains(TermModes::MOUSE_ANY);
    let want_btn = modes.contains(TermModes::MOUSE_BUTTON);
    let want_vt200 = modes.contains(TermModes::MOUSE_VT200);
    let want_x10 = modes.contains(TermModes::MOUSE_X10);
    if !want_any && !want_btn && !want_vt200 && !want_x10 {
        return None;
    }
    if motion && !want_any && !want_btn {
        return None;
    }
    if !pressed && !motion && want_x10 && !want_vt200 && !want_btn && !want_any {
        return None;
    }

    let mut cb = button;
    if motion {
        cb |= 32;
    }
    if (mods & modifiers::SHIFT) != 0 {
        cb |= 4;
    }
    if (mods & modifiers::OPTION) != 0 {
        cb |= 8;
    }
    if (mods & modifiers::CONTROL) != 0 {
        cb |= 16;
    }

    let x = col_0 + 1;
    let y = row_0 + 1;

    if modes.contains(TermModes::MOUSE_SGR) {
        let suffix = if pressed || motion { 'M' } else { 'm' };
        return Some(format!("\x1b[<{cb};{x};{y}{suffix}").into_bytes());
    }

    // Legacy X10/VT200 encoding (1-based + 32, clamped to 223)
    let enc_cb = if !pressed && !motion { 3 } else { cb };
    if x <= 223 && y <= 223 {
        return Some(vec![0x1B, b'[', b'M', enc_cb + 32, (x as u8) + 32, (y as u8) + 32]);
    }
    None
}
