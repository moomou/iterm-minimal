pub mod colors;
pub mod keymap;
pub mod plist_import;
pub mod semantic_history;
pub mod smart_selection;

pub use colors::{ColorScheme, Rgba};
pub use keymap::{modifiers, HotkeyConfig, KeyAction, KeyCombo, KeyMap, OptionKeyMode};
pub use plist_import::{AppConfig, CursorStyle, FontSpec, InitialDirectory, Profile, ProfileError};
pub use semantic_history::{
    resolve_semantic_target, split_path_line_col, RemoteArtifact, SemanticHistoryAction,
    SemanticHistoryConfig, SemanticTarget,
};
pub use smart_selection::{
    default_smart_selection_rules, is_word_char, SmartSelectionMatch, SmartSelectionPrecision,
    SmartSelectionRule, SmartSelector,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_inline_iterm2_xml_plist_and_optional_env_plist() {
        let sample_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CopySelection</key><true/>
  <key>AllowClipboardAccess</key><true/>
  <key>DimInactiveSplitPanes</key><true/>
  <key>HideScrollbar</key><true/>
  <key>Hotkey</key><true/>
  <key>HotkeyCode</key><integer>34</integer>
  <key>WordCharacters</key><string>/-+\~_.</string>
  <key>Default Bookmark Guid</key><string>
    268B44BF-9E3A-4218-92E0-C8ED002F2571
  </string>
  <key>New Bookmarks</key>
  <array>
    <dict>
      <key>Guid</key><string>268B44BF-9E3A-4218-92E0-C8ED002F2571</string>
      <key>Name</key><string>Default</string>
      <key>Normal Font</key><string>FiraCode-Retina 20</string>
      <key>Non Ascii Font</key><string>Monaco 20</string>
      <key>Use Non-ASCII Font</key><true/>
      <key>ASCII Ligatures</key><true/>
      <key>Unlimited Scrollback</key><true/>
      <key>Mouse Reporting</key><true/>
      <key>Silence Bell</key><true/>
      <key>Visual Bell</key><true/>
      <key>Transparency</key><real>0.242032</real>
      <key>Keyboard Map</key>
      <dict>
        <key>0xf702-0x280000</key>
        <dict><key>Action</key><integer>10</integer><key>Text</key><string>b</string></dict>
        <key>0x7f-0x100000</key>
        <dict><key>Action</key><integer>11</integer><key>Text</key><string>0x15</string></dict>
        <key>0x7f-0x80000</key>
        <dict><key>Action</key><integer>11</integer><key>Text</key><string>0x1b 0x7f</string></dict>
      </dict>
    </dict>
  </array>
</dict>
</plist>"#;
        let tmp = std::env::temp_dir().join(format!("iterm-minimal-test-{}.plist", std::process::id()));
        std::fs::write(&tmp, sample_plist).expect("write temp plist");
        let cfg = AppConfig::from_iterm2_plist(&tmp).expect("must parse XML iTerm2 plist");
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(
            cfg.default_profile_guid.as_deref(),
            Some("268B44BF-9E3A-4218-92E0-C8ED002F2571")
        );
        assert!(cfg.copy_selection);
        assert!(cfg.allow_clipboard_access);
        assert!(cfg.dim_inactive_split_panes);
        assert!(cfg.hide_scrollbar);
        assert!(cfg.hotkey.enabled);
        assert_eq!(cfg.hotkey.key_code, 34);
        assert_eq!(cfg.word_characters, "/-+\\~_.");

        let prof = cfg.default_profile();
        assert_eq!(prof.name, "Default");
        assert!(prof.unlimited_scrollback);
        assert!(prof.mouse_reporting);
        assert!(prof.silence_bell);
        assert!(prof.visual_bell);
        assert!((prof.transparency - 0.242032).abs() < 0.001);
        assert_eq!(
            prof.keymap.lookup(0xF702, modifiers::OPTION, None),
            Some(&KeyAction::SendEscapeSequence("b".into()))
        );
        assert_eq!(
            prof.keymap.lookup(0x7F, modifiers::COMMAND, None),
            Some(&KeyAction::SendHexCode(vec![0x15]))
        );
        assert_eq!(
            prof.keymap.lookup(0x7F, modifiers::OPTION, None),
            Some(&KeyAction::SendHexCode(vec![0x1B, 0x7F]))
        );

        // Optional local override for validating a real macOS plist via `ITERM2_TEST_PLIST=/path/to/plist`
        if let Ok(env_path) = std::env::var("ITERM2_TEST_PLIST") {
            let p = Path::new(&env_path);
            if p.exists() {
                let real = AppConfig::from_iterm2_plist(p).expect("must parse ITERM2_TEST_PLIST");
                assert!(!real.profiles.is_empty());
            }
        }
    }

    #[test]
    fn smart_selector_prefers_url_over_generic_word() {
        let sel = SmartSelector::new(&default_smart_selection_rules());
        let line = "see https://example.com/foo/bar.png for details";
        let m = sel.match_at(line, 12).expect("must match URL");
        assert_eq!(&line[m.start_byte..m.end_byte], "https://example.com/foo/bar.png");
        assert_eq!(m.precision, SmartSelectionPrecision::VeryHigh);
    }

    #[test]
    fn semantic_history_resolves_explicit_artifact_and_remote_fallback() {
        let art = RemoteArtifact {
            host: "alice@gpu-box".into(),
            port: None,
            abs_path: "/data/plot.png".into(),
            conn_id: Some("ssh-1".into()),
            kind: Some("image".into()),
            size: Some(4096),
            start_col: 0,
            end_col: 14,
        };
        let t = resolve_semantic_target(
            "plot.png",
            None,
            Some(&art),
            Some("alice@gpu-box"),
            "laptop.local",
            Some("/data"),
        );
        assert_eq!(t, Some(SemanticTarget::ExplicitRemoteArtifact(art)));

        let inferred = resolve_semantic_target(
            "./src/main.rs:42:7",
            None,
            None,
            Some("alice@gpu-box"),
            "laptop.local",
            Some("/home/alice/project"),
        );
        assert_eq!(
            inferred,
            Some(SemanticTarget::InferredRemoteFile {
                host: "alice@gpu-box".into(),
                remote_path: "/home/alice/project/src/main.rs".into(),
                line: Some(42),
                col: Some(7),
            })
        );
    }

    #[test]
    fn min_contrast_pushes_dark_on_dark_lighter() {
        let bg = Rgba::from_u8(20, 20, 20, 255);
        let fg = Rgba::from_u8(30, 30, 30, 255);
        let adj = fg.enforce_min_contrast(bg, 0.5);
        assert!(adj.luma() - bg.luma() >= 0.49);
    }
}
