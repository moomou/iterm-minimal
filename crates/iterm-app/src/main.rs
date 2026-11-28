mod bench_runner;
mod input_logic;
#[cfg(target_os = "macos")]
mod macos;

use iterm_profile::AppConfig;
use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Log panics with file/line to ~/Library/Logs/JTerm.panic.log before aborting
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("JTerm panic: {info}\n");
        eprintln!("{msg}");
        if let Ok(home) = std::env::var("HOME") {
            let log_dir = PathBuf::from(home).join("Library/Logs");
            let _ = std::fs::create_dir_all(&log_dir);
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join("JTerm.panic.log"))
                .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
        }
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut plist_path: Option<PathBuf> = None;
    let mut import_plist: Option<PathBuf> = None;
    let mut run_bench = false;
    let mut run_find_bench = false;
    let mut dump_profile = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--benchmark" | "--bench" => run_bench = true,
            "--benchmark-find" | "--bench-find" => run_find_bench = true,
            "--dump-profile" => dump_profile = true,
            "--plist" => {
                if let Some(p) = args.get(i + 1) {
                    plist_path = Some(PathBuf::from(p));
                    i += 1;
                }
            }
            "--import-plist" => {
                if let Some(p) = args.get(i + 1) {
                    import_plist = Some(PathBuf::from(p));
                    i += 1;
                }
            }
            "--version" | "-V" => {
                println!("jterm {} (Rust + Metal + CoreText)", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    "jterm — High-performance macOS terminal with iTerm2 profile parity\n\n\
                     USAGE:\n  jterm [OPTIONS]\n\n\
                     OPTIONS:\n  \
                     --plist <PATH>         Launch directly using an iTerm2 com.googlecode.iterm2.plist\n  \
                     --import-plist <PATH>  One-time import iTerm2 plist to ~/.config/jterm/config.json\n  \
                     --dump-profile         Print resolved active profile & keymap summary\n  \
                     --benchmark            Run VT, dirty-row frame prep, scrollback, PTY, and Find benchmarks\n  \
                     --benchmark-find       Run Find (Cmd+F) 100k-line scrollback benchmarks only\n  \
                     -V, --version          Print version\n  \
                     -h, --help             Print help"
                );
                return;
            }
            _ => {}
        }
        i += 1;
    }

    if let Some(src) = import_plist {
        match AppConfig::from_iterm2_plist(&src) {
            Ok(cfg) => {
                let dest = AppConfig::native_config_path();
                match cfg.save_json(&dest) {
                    Ok(()) => {
                        println!(
                            "Imported {} profile(s), {} color preset(s), {} keymap binding(s) -> {}",
                            cfg.profiles.len(),
                            cfg.color_presets.len(),
                            cfg.default_profile().keymap.len(),
                            dest.display()
                        );
                    }
                    Err(e) => {
                        eprintln!("Failed to save native config: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to import plist {}: {e}", src.display());
                std::process::exit(1);
            }
        }
        return;
    }

    if run_bench {
        bench_runner::run_all_benchmarks();
        return;
    }

    if run_find_bench {
        bench_runner::run_find_benchmark_suite();
        return;
    }

    let cfg = match AppConfig::load_auto(plist_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Config load warning ({e}); falling back to built-in Dracula+ profile");
            AppConfig::default()
        }
    };

    if dump_profile {
        let p = cfg.default_profile();
        println!("Active Profile: {} (GUID={})", p.name, p.guid);
        println!("  Normal Font:      {} {:.1}pt", p.normal_font.name, p.normal_font.size);
        println!("  Non-ASCII Font:   {} {:.1}pt", p.non_ascii_font.name, p.non_ascii_font.size);
        println!("  ASCII Ligatures:  {}", p.ascii_ligatures);
        println!("  Transparency:     {:.2}", p.transparency);
        println!("  Scrollback:       unlimited={} max_lines={}", p.unlimited_scrollback, p.scrollback_lines);
        println!("  Keyboard Map:     {} bindings", p.keymap.len());
        println!("  Smart Selection:  {} rules", p.smart_selection_rules.len());
        println!("  Global Hotkey:    enabled={} keycode={} mods=0x{:x}", cfg.hotkey.enabled, cfg.hotkey.key_code, cfg.hotkey.modifiers);
        println!("  CopySelection:    {} | AllowClipboard(OSC52): {}", cfg.copy_selection, cfg.allow_clipboard_access);
        println!("  Color Presets:    {:?}", cfg.color_presets.keys().collect::<Vec<_>>());
        return;
    }

    #[cfg(target_os = "macos")]
    {
        macos::app::run_macos_app(cfg);
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!(
            "jterm GUI targets macOS (AppKit + Metal + CoreText).\n\
             On Linux, use --benchmark, --dump-profile, --plist <path>, or scripts/build-macos.sh."
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::input_logic::{
        encode_mouse_event, pop_find_word, resolve_key_event, resolve_key_event_with_kitty,
        sanitize_find_paste, KeyEventInput, UiCommand,
    };
    use iterm_profile::{modifiers, AppConfig};
    use iterm_vt::TermModes;
    use std::path::Path;

    #[test]
    fn resolves_user_iterm2_keymap_and_shortcuts() {
        let cfg = std::env::var("ITERM2_TEST_PLIST")
            .ok()
            .filter(|p| Path::new(p).exists())
            .and_then(|p| AppConfig::from_iterm2_plist(Path::new(&p)).ok())
            .unwrap_or_default();
        let prof = cfg.default_profile();

        // Opt+Left -> ESC b
        let ev = KeyEventInput {
            key_code_char: 0xF702,
            characters: String::new(),
            virtual_key: 123,
            mods: modifiers::OPTION | modifiers::NUMERIC_PAD,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event(&ev, &cfg, prof, TermModes::default()),
            UiCommand::SendBytes(b"\x1bb".to_vec())
        );

        // Cmd+D -> SplitVertical
        let split_ev = KeyEventInput {
            key_code_char: 'd' as u32,
            characters: "d".into(),
            virtual_key: 2,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event(&split_ev, &cfg, prof, TermModes::default()),
            UiCommand::SplitVertical
        );

        // Cmd+, -> OpenPreferences
        let prefs_ev = KeyEventInput {
            key_code_char: ',' as u32,
            characters: ",".into(),
            virtual_key: 43,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event(&prefs_ev, &cfg, prof, TermModes::default()),
            UiCommand::OpenPreferences
        );

        // Ctrl+A -> 0x01 (tmux prefix!)
        let ctrl_a = KeyEventInput {
            key_code_char: 'a' as u32,
            characters: "\x01".into(),
            virtual_key: 0,
            mods: modifiers::CONTROL,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event(&ctrl_a, &cfg, prof, TermModes::default()),
            UiCommand::SendBytes(vec![0x01])
        );
    }

    #[test]
    fn encodes_sgr_1006_mouse_for_tmux() {
        let modes = TermModes::MOUSE_VT200 | TermModes::MOUSE_SGR;
        assert_eq!(
            encode_mouse_event(modes, 0, true, false, 9, 4, 0),
            Some(b"\x1b[<0;10;5M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(modes, 0, false, false, 9, 4, 0),
            Some(b"\x1b[<0;10;5m".to_vec())
        );
    }

    #[test]
    fn encodes_kitty_keyboard_protocol_when_enabled() {
        let cfg = AppConfig::default();
        let prof = cfg.default_profile();
        // Shift+Enter under Kitty disambiguate mode (flags=1) -> CSI 13;2u
        let shift_enter = KeyEventInput {
            key_code_char: 0x0D,
            characters: "\r".into(),
            virtual_key: 36,
            mods: modifiers::SHIFT,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&shift_enter, &cfg, prof, TermModes::default(), 1),
            UiCommand::SendBytes(b"\x1b[13;2u".to_vec())
        );
        // Ctrl+I under Kitty mode -> CSI 105;5u (disambiguated from Tab!)
        let ctrl_i = KeyEventInput {
            key_code_char: 'i' as u32,
            characters: "\t".into(),
            virtual_key: 34,
            mods: modifiers::CONTROL,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&ctrl_i, &cfg, prof, TermModes::default(), 1),
            UiCommand::SendBytes(b"\x1b[105;5u".to_vec())
        );
        // Kitty report_all (flags = 9) encodes plain 'a' as CSI 97u
        let plain_a = KeyEventInput {
            key_code_char: 'a' as u32,
            characters: "a".into(),
            virtual_key: 0,
            mods: 0,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&plain_a, &cfg, prof, TermModes::default(), 9),
            UiCommand::SendBytes(b"\x1b[97u".to_vec())
        );
        // Right Option (Normal) vs Left Option (EscPlus)
        let mut p2 = prof.clone();
        p2.option_key_sends = iterm_profile::OptionKeyMode::EscPlus;
        p2.right_option_key_sends = iterm_profile::OptionKeyMode::Normal;
        let left_opt_f = KeyEventInput {
            key_code_char: 'f' as u32,
            characters: "ƒ".into(),
            virtual_key: 3,
            mods: modifiers::OPTION,
            is_right_option: false,
        };
        let right_opt_f = KeyEventInput {
            is_right_option: true,
            ..left_opt_f.clone()
        };
        assert_eq!(
            resolve_key_event_with_kitty(&left_opt_f, &cfg, &p2, TermModes::default(), 0),
            UiCommand::SendBytes(b"\x1bf".to_vec())
        );
        assert_eq!(
            resolve_key_event_with_kitty(&right_opt_f, &cfg, &p2, TermModes::default(), 0),
            UiCommand::SendBytes("ƒ".as_bytes().to_vec())
        );
        // Cmd+U toggles transparency (matching iTerm2's View -> Use Transparency)
        let cmd_u = KeyEventInput {
            key_code_char: 'u' as u32,
            characters: "u".into(),
            virtual_key: 32,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&cmd_u, &cfg, prof, TermModes::default(), 0),
            UiCommand::ToggleTransparency
        );
        // Cmd+Opt+L toggles the live Transfers queue overlay
        let cmd_opt_l = KeyEventInput {
            key_code_char: 'l' as u32,
            characters: "l".into(),
            virtual_key: 37,
            mods: modifiers::COMMAND | modifiers::OPTION,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&cmd_opt_l, &cfg, prof, TermModes::default(), 0),
            UiCommand::ToggleTransfers
        );
        // Cmd+9 jumps to last tab; Cmd+1..8 jump to 0..7
        let cmd_9 = KeyEventInput {
            key_code_char: '9' as u32,
            characters: "9".into(),
            virtual_key: 25,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&cmd_9, &cfg, prof, TermModes::default(), 0),
            UiCommand::SelectLastTab
        );
        let cmd_3 = KeyEventInput {
            key_code_char: '3' as u32,
            characters: "3".into(),
            virtual_key: 20,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&cmd_3, &cfg, prof, TermModes::default(), 0),
            UiCommand::SelectTab(2)
        );
        // Cmd+M minimizes active window on macOS
        let cmd_m = KeyEventInput {
            key_code_char: 'm' as u32,
            characters: "m".into(),
            virtual_key: 46,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&cmd_m, &cfg, prof, TermModes::default(), 0),
            UiCommand::MinimizeWindow
        );
        // Cmd+E uses current selection for Find
        let cmd_e = KeyEventInput {
            key_code_char: 'e' as u32,
            characters: "e".into(),
            virtual_key: 14,
            mods: modifiers::COMMAND,
            is_right_option: false,
        };
        assert_eq!(
            resolve_key_event_with_kitty(&cmd_e, &cfg, prof, TermModes::default(), 0),
            UiCommand::UseSelectionForFind
        );
        // Find bar paste sanitization & word-delete helpers
        assert_eq!(sanitize_find_paste("error_code\r\n"), "error_code");
        assert_eq!(sanitize_find_paste("line1\r\nline2\nline3\n"), "line1 line2 line3");
        assert_eq!(sanitize_find_paste("\x1bfoo\tbar\x00"), "foo bar");
        let mut q = "fatal_timeout status".to_string();
        pop_find_word(&mut q);
        assert_eq!(q, "fatal_timeout ");
        pop_find_word(&mut q);
        assert_eq!(q, "");
    }
}
