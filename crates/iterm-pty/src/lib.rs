pub mod download;
pub mod pane_tree;
pub mod pty;
pub mod session;
pub mod tabs;

pub use download::{
    build_scp_args, build_scp_upload_args, download_artifact_async, download_remote_async,
    format_bytes, parse_human_bytes, parse_scp_progress_line, sanitize_filename,
    save_inline_download, shell_escape_path, unique_dest_path, upload_file_async, DownloadStatus,
    ScpProgress, TransferDirection, TransferId, TransferQueue, TransferSnapshot, TransferState,
    UploadStatus,
};
pub use pane_tree::{DividerRect, FocusDirection, PaneNode, PaneRect, PaneTree, SplitAxis};
pub use pty::{PtyError, PtyHandle, PtySize, PtySpawnConfig};
pub use session::{SessionId, TerminalSession, WakeCallback};
pub use tabs::{TabId, TabManager, TabState};

#[cfg(test)]
mod tests {
    use super::*;
    use iterm_profile::Profile;
    use std::path::{Path, PathBuf};
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
        tm.select_last_tab();
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

        // Cmd+9 jumps to last tab whether there are 3 tabs or 12 tabs
        let mut tm2 = TabManager::new(SessionId(1), "T1".into());
        tm2.add_tab(SessionId(2), "T2".into());
        tm2.add_tab(SessionId(3), "T3".into());
        tm2.select_tab(0);
        tm2.select_last_tab();
        assert_eq!(tm2.active_index, 2);
        for i in 4..=12u64 {
            tm2.add_tab(SessionId(i), format!("T{i}"));
        }
        tm2.select_tab(0);
        tm2.select_last_tab();
        assert_eq!(tm2.active_index, 11);
    }

    #[test]
    fn parses_openssh_scp_live_pty_progress_lines_and_human_bytes() {
        let p1 = parse_scp_progress_line("dataset.tar.gz       42%   42MB  12.4MB/s   00:04 ETA")
            .expect("must parse active progress");
        assert_eq!(p1.percent, 42);
        assert_eq!(p1.bytes_done, 42 * 1024 * 1024);
        assert_eq!(p1.speed, "12.4MB/s");
        assert_eq!(p1.eta, "00:04");

        let p2 = parse_scp_progress_line("my file 50%.png     100% 1248KB  15.2MB/s   00:00")
            .expect("must handle % inside filename");
        assert_eq!(p2.percent, 100);
        assert_eq!(p2.bytes_done, 1248 * 1024);
        assert_eq!(p2.speed, "15.2MB/s");
        assert_eq!(p2.eta, "00:00");

        let p3 = parse_scp_progress_line("huge.bin  12% 12MB 0.0KB/s - stalled -")
            .expect("must parse stalled line");
        assert_eq!(p3.percent, 12);
        assert_eq!(p3.eta, "stalled");

        assert!(parse_scp_progress_line("scp: /missing: No such file or directory").is_none());
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0MB");
    }

    #[test]
    fn transfer_queue_tracks_inline_and_clears_finished() {
        let q = TransferQueue::new(2, || {});
        let id = q.record_inline_completed("plot.png".into(), PathBuf::from("/tmp/plot.png"), 4096);
        let snaps = q.snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, id);
        assert_eq!(snaps[0].percent, Some(100));
        assert_eq!(snaps[0].state, TransferState::Completed);
        assert_eq!(q.active_count(), 0);
        q.clear_finished();
        assert!(q.snapshots().is_empty());
    }
}
