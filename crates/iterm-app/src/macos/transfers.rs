//! File transfers, paste/drag-and-drop SSH SCP uploads, and Semantic History Cmd+Click actions.

use crate::macos::app::schedule_main_wakeup;
use crate::macos::chrome::extract_word_at;
use crate::macos::window::{AsyncPasteResult, TerminalWindowState};
use iterm_profile::{resolve_semantic_target, SemanticTarget};
use iterm_pty::{shell_escape_path, SessionId, TransferState};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;

impl TerminalWindowState {
    /// Detect if `sid` is currently connected to a remote SSH host via shell integration `RemoteHost`.
    pub(crate) fn session_remote_host(&self, sid: SessionId) -> Option<String> {
        let sess = self.sessions.get(&sid)?;
        let s = sess.screen.lock();
        let rh = s.current_host()?;
        let host_part = rh.split('@').next_back().unwrap_or(rh);
        if !host_part.is_empty()
            && host_part != "localhost"
            && host_part != "127.0.0.1"
            && !host_part.eq_ignore_ascii_case(&self.local_hostname)
        {
            Some(rh.to_string())
        } else {
            None
        }
    }

    /// Handle pasted or drag-and-dropped local files:
    /// - Local session: pastes shell-escaped local paths directly into the PTY.
    /// - Remote SSH session: uploads each file via OpenSSH ControlMaster `/usr/bin/scp` to `/tmp/`
    ///   and pastes the remote path straight into the PTY!
    pub fn handle_local_files_attach(&mut self, sid: SessionId, paths: Vec<PathBuf>) {
        self.handle_local_files_attach_inner(sid, paths, false);
    }

    /// Attach pasted temporary screenshot PNGs (automatically deleted after remote SCP upload completes!).
    pub fn handle_temp_files_attach(&mut self, sid: SessionId, paths: Vec<PathBuf>) {
        self.handle_local_files_attach_inner(sid, paths, true);
    }

    fn handle_local_files_attach_inner(
        &mut self,
        sid: SessionId,
        paths: Vec<PathBuf>,
        cleanup_after_remote_upload: bool,
    ) {
        if paths.is_empty() {
            return;
        }
        if let Some(remote_host) = self.session_remote_host(sid) {
            for local in paths {
                let tx = self.upload_tx.clone();
                let wf = Arc::clone(&self.wake_flag);
                self.transfers.enqueue_upload(
                    local,
                    remote_host.clone(),
                    None,
                    cleanup_after_remote_upload,
                    Some(Box::new(move |snap| {
                        match snap.state {
                            TransferState::Completed => {
                                let _ = tx.send(AsyncPasteResult::Uploaded {
                                    sid,
                                    remote_path: snap.dest_or_remote,
                                    bytes: snap.bytes_done,
                                });
                            }
                            TransferState::Failed(err) => {
                                let _ = tx.send(AsyncPasteResult::Failed {
                                    label: snap.label,
                                    error: err,
                                });
                            }
                            _ => {}
                        }
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    })),
                );
            }
            self.wake_flag.store(true, Ordering::Release);
        } else if let Some(sess) = self.sessions.get(&sid) {
            let joined = paths
                .iter()
                .map(|p| shell_escape_path(&p.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = sess.write_paste(&joined, None);
        }
    }

    /// Handle Cmd+Click Semantic History: explicit RemoteArtifact -> OSC 8 URL -> SmartSelection -> Local/SCP!
    pub(crate) fn handle_semantic_cmd_click(&mut self, sid: SessionId, vis_row: usize, col: usize) {
        let Some(sess) = self.sessions.get(&sid) else {
            return;
        };
        let target = {
            let s = sess.screen.lock();
            let mut scratch = Vec::new();
            let (cells, _, host_id, cwd_id, artifacts) = s.read_visible_row(vis_row, &mut scratch);
            let explicit = artifacts.and_then(|arts| {
                arts.iter()
                    .find(|a| col >= a.start_col && col < a.end_col)
                    .cloned()
            });
            let osc8 = cells
                .get(col)
                .and_then(|c| s.cell_hyperlink_url(c).map(ToOwned::to_owned));
            let line_text = iterm_vt::row_cells_to_string(cells, false);
            let byte_off = line_text
                .char_indices()
                .nth(col)
                .map(|(b, _)| b)
                .unwrap_or(line_text.len().saturating_sub(1));
            let token = self
                .smart_selector
                .match_at(&line_text, byte_off)
                .map(|m| line_text[m.start_byte..m.end_byte].to_string())
                .unwrap_or_else(|| {
                    extract_word_at(&line_text, col, &self.app_cfg.word_characters)
                });
            let remote_host = s.strings.get(host_id).or_else(|| s.current_host());
            let cwd = s.strings.get(cwd_id).or_else(|| s.current_cwd());
            resolve_semantic_target(
                &token,
                osc8.as_deref(),
                explicit.as_ref(),
                remote_host,
                &self.local_hostname,
                cwd,
            )
        };

        let Some(target) = target else {
            return;
        };

        match target {
            SemanticTarget::Url(url) => {
                if let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(&url)) {
                    let ws = NSWorkspace::sharedWorkspace();
                    ws.openURL(&ns_url);
                }
            }
            SemanticTarget::ExplicitRemoteArtifact(art) => {
                let dl_dir = self.app_cfg.download_dir.clone();
                self.transfers.enqueue_artifact(
                    dl_dir,
                    art,
                    Some(Box::new(|snap| {
                        match snap.state {
                            TransferState::Completed => {
                                log::info!(
                                    "Artifact downloaded: {} ({} bytes)",
                                    snap.dest_or_remote,
                                    snap.bytes_done
                                );
                            }
                            TransferState::Failed(ref err) => {
                                log::error!("Artifact download failed ({}): {err}", snap.label);
                            }
                            _ => {}
                        }
                    })),
                );
            }
            SemanticTarget::InferredRemoteFile {
                host, remote_path, ..
            } => {
                let dl_dir = self.app_cfg.download_dir.clone();
                self.transfers.enqueue_download(
                    dl_dir,
                    host,
                    None,
                    remote_path,
                    None,
                    Some(Box::new(|snap| {
                        match snap.state {
                            TransferState::Completed => {
                                log::info!(
                                    "Remote SCP downloaded: {} ({} bytes)",
                                    snap.dest_or_remote,
                                    snap.bytes_done
                                );
                            }
                            TransferState::Failed(ref err) => {
                                log::error!("Remote SCP failed ({}): {err}", snap.label);
                            }
                            _ => {}
                        }
                    })),
                );
            }
            SemanticTarget::LocalFile { path, .. } => {
                if path.exists() {
                    let mut cmd = Command::new("/usr/bin/open");
                    if let Some(ref bundle) = self.profile.semantic_history.editor_bundle_id {
                        cmd.arg("-b").arg(bundle);
                    }
                    let _ = cmd.arg(&path).spawn();
                }
            }
        }
        self.wake_flag.store(true, Ordering::Release);
    }
}
