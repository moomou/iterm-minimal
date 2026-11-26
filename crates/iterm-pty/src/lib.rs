pub mod download;
pub mod pane_tree;
pub mod pty;
pub mod session;
pub mod tabs;

pub use download::{
    build_scp_args, build_scp_upload_args, download_artifact_async, download_remote_async,
    sanitize_filename, save_inline_download, shell_escape_path, unique_dest_path,
    upload_file_async, DownloadStatus, UploadStatus,
};
pub use pane_tree::{DividerRect, FocusDirection, PaneNode, PaneRect, PaneTree, SplitAxis};
pub use pty::{PtyError, PtyHandle, PtySize, PtySpawnConfig};
pub use session::{SessionId, TerminalSession, WakeCallback};
pub use tabs::{TabId, TabManager, TabState};

#[cfg(test)]
mod tests {
    use super::*;
    use iterm_profile::Profile;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn spawns_real_pty_with_iterm2_env_and_coalesced_reader() {
        let mut prof = Profile::default();
        prof.custom_command = Some(
            "/bin/sh -c 'printf \"TP=%s|ID=%s|DONE\\n\" \"$TERM_PROGRAM\" \"$ITERM_SESSION_ID\"'"
                .to_string(),
        );
        let session = TerminalSession::spawn(prof, PtySize::default(), None, None)
            .expect("spawn pty session");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found = String::new();
        while Instant::now() < deadline {
            {
                let s = session.screen.lock();
                let line0 = s.primary.row_text(0);
                if line0.contains("DONE") {
                    found = line0;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            found.contains("TP=iTerm.app") && found.contains("ID=w0t0p"),
            "unexpected PTY output: {found:?}"
        );
    }

    #[test]
    fn pane_tree_split_layout_focus_and_close() {
        let s1 = SessionId(1);
        let s2 = SessionId(2);
        let s3 = SessionId(3);
        let mut tree = PaneTree::new(s1);
        assert!(tree.split(s1, SplitAxis::Vertical, s2));
        assert!(tree.split(s2, SplitAxis::Horizontal, s3));
        assert_eq!(tree.pane_count(), 3);
        assert_eq!(tree.active, s3);

        let bounds = PaneRect { x: 0.0, y: 0.0, width: 1000.0, height: 600.0 };
        let (panes, divs) = tree.layout(bounds, 2.0);
        assert_eq!(panes.len(), 3);
        assert_eq!(divs.len(), 2);

        // Navigating left from right-bottom s3 should focus left pane s1
        let focused = tree.focus_direction(FocusDirection::Left, bounds);
        assert_eq!(focused, s1);

        // Zoom focuses only active pane
        tree.toggle_zoom();
        let (zpanes, zdivs) = tree.layout(bounds, 2.0);
        assert_eq!(zpanes.len(), 1);
        assert_eq!(zpanes[0].0, s1);
        assert!(zdivs.is_empty());

        // Closing s2 keeps s1 & s3
        assert!(tree.close(s2).is_some());
        assert_eq!(tree.pane_count(), 2);
    }

    #[test]
    fn scp_args_are_shell_injection_safe() {
        let args = build_scp_args("user@host", Some(2222), "/tmp/my file;rm -rf /.png", Path::new("/Users/moomou/Downloads/out.png"));
        assert!(args.contains(&"--".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert_eq!(args[args.len() - 2], "user@host:/tmp/my file;rm -rf /.png");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");

        let up = build_scp_upload_args(Path::new("/tmp/a b.png"), "moomou@moomoutu", None, "/tmp/out.png");
        assert!(up.contains(&"--".to_string()));
        assert_eq!(up[up.len() - 2], "/tmp/a b.png");
        assert_eq!(up[up.len() - 1], "moomou@moomoutu:/tmp/out.png");
        assert_eq!(shell_escape_path("/tmp/clean-file.png"), "/tmp/clean-file.png");
        assert_eq!(
            shell_escape_path("/Users/moomou/Screenshot 2026-11-26.png"),
            "'/Users/moomou/Screenshot 2026-11-26.png'"
        );
    }

    #[test]
    fn tab_manager_closing_last_tab_is_safe() {
        let s1 = SessionId(1);
        let mut tm = TabManager::new(s1, "Default".into());
        assert!(tm.close_session(s1));
        assert!(tm.is_empty());
        assert_eq!(tm.try_active_session(), None);
        assert!(tm.try_active_tab().is_none());
        tm.next_tab();
        tm.prev_tab();
        tm.select_tab(0);
        tm.move_active_tab(1);
        assert_eq!(
            tm.focus_direction(
                FocusDirection::Left,
                PaneRect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0
                }
            ),
            None
        );
    }
}
