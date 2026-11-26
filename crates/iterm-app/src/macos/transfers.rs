//! File transfers, paste/drag-and-drop SSH SCP uploads, and Semantic History Cmd+Click actions.

use crate::macos::app::schedule_main_wakeup;
use crate::macos::chrome::extract_word_at;
use crate::macos::window::{AsyncPasteResult, TerminalWindowState};
use iterm_profile::{resolve_semantic_target, SemanticTarget};
use iterm_pty::{
    download_artifact_async, download_remote_async, shell_escape_path, upload_file_async,
    DownloadStatus, SessionId, UploadStatus,
};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
                let cleanup_path = if cleanup_after_remote_upload {
                    Some(local.clone())
                } else {
                    None
                };
                let label = local
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_string();
                self.status_toast = Some((
                    format!("Uploading {label} -> {remote_host}..."),
                    Instant::now() + Duration::from_secs(5),
                ));
                self.wake_flag.store(true, Ordering::Release);
                upload_file_async(local, remote_host.clone(), None, move |st| match st {
                    UploadStatus::Completed { remote_path, bytes } => {
                        if let Some(ref p) = cleanup_path {
                            let _ = std::fs::remove_file(p);
                        }
                        let _ = tx.send(AsyncPasteResult::Uploaded {
                            sid,
                            remote_path,
                            bytes,
                        });
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    }
                    UploadStatus::Failed { local, error } => {
                        if let Some(ref p) = cleanup_path {
                            let _ = std::fs::remove_file(p);
                        }
                        let lbl = local
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file")
                            .to_string();
                        let _ = tx.send(AsyncPasteResult::Failed { label: lbl, error });
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    }
                    UploadStatus::Started { .. } => {}
                });
            }
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
                let path_label = art.abs_path.clone();
                let dl_dir = self.app_cfg.download_dir.clone();
                let tx = self.upload_tx.clone();
                let wf = Arc::clone(&self.wake_flag);
                self.status_toast = Some((
                    format!("Downloading {}...", path_label),
                    Instant::now() + Duration::from_secs(3),
                ));
                download_artifact_async(dl_dir, art, move |st| match st {
                    DownloadStatus::Completed { dest, bytes } => {
                        let fname = dest
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file");
                        log::info!("Artifact downloaded: {} ({bytes} bytes)", dest.display());
                        let _ = tx.send(AsyncPasteResult::Toast {
                            msg: format!("Downloaded {fname} ({bytes} B) -> ~/Downloads"),
                        });
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    }
                    DownloadStatus::Failed { label, error } => {
                        log::error!("Artifact download failed ({label}): {error}");
                        let _ = tx.send(AsyncPasteResult::Failed { label, error });
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    }
                    DownloadStatus::Started { .. } => {}
                });
            }
            SemanticTarget::InferredRemoteFile {
                host, remote_path, ..
            } => {
                let dl_dir = self.app_cfg.download_dir.clone();
                let tx = self.upload_tx.clone();
                let wf = Arc::clone(&self.wake_flag);
                self.status_toast = Some((
                    format!("SCP {}:{}...", host, remote_path),
                    Instant::now() + Duration::from_secs(3),
                ));
                download_remote_async(dl_dir, host, None, remote_path, move |st| match st {
                    DownloadStatus::Completed { dest, bytes } => {
                        let fname = dest
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file");
                        log::info!("Remote SCP downloaded: {} ({bytes} bytes)", dest.display());
                        let _ = tx.send(AsyncPasteResult::Toast {
                            msg: format!("Downloaded {fname} ({bytes} B) -> ~/Downloads"),
                        });
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    }
                    DownloadStatus::Failed { label, error } => {
                        log::error!("Remote SCP failed ({label}): {error}");
                        let _ = tx.send(AsyncPasteResult::Failed { label, error });
                        wf.store(true, Ordering::Release);
                        schedule_main_wakeup();
                    }
                    DownloadStatus::Started { .. } => {}
                });
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
