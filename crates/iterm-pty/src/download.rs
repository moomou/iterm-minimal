use iterm_profile::RemoteArtifact;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    Started { label: String, dest: PathBuf },
    Completed { dest: PathBuf, bytes: u64 },
    Failed { label: String, error: String },
}

#[derive(Debug, Clone)]
pub enum UploadStatus {
    Started { local: PathBuf, remote: String },
    Completed { remote_path: String, bytes: u64 },
    Failed { local: PathBuf, error: String },
}

/// POSIX single-quote shell-escape for pasting file paths safely into shells / CLIs.
pub fn shell_escape_path(path: &str) -> String {
    if !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+' | b'@' | b':'))
    {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('\'');
    for ch in path.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build safe `argv` arguments for uploading a local file to `host:remote_path` via OpenSSH ControlMaster `/usr/bin/scp`.
pub fn build_scp_upload_args(
    local_path: &Path,
    host: &str,
    port: Option<u16>,
    remote_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "-q".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        "ControlPersist=60".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if let Some(p) = port {
        args.push("-P".to_string());
        args.push(p.to_string());
    }
    args.push("--".to_string());
    args.push(local_path.to_string_lossy().into_owned());
    args.push(format!("{host}:{remote_path}"));
    args
}

/// Upload a local file (dropped or pasted) to a remote SSH host's `/tmp/` directory in a background thread.
pub fn upload_file_async<F>(local_path: PathBuf, host: String, port: Option<u16>, on_status: F)
where
    F: Fn(UploadStatus) + Send + 'static,
{
    thread::Builder::new()
        .name("remote-upload".into())
        .spawn(move || {
            let fname = local_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(sanitize_filename)
                .unwrap_or_else(|| "upload.bin".into());
            // Replace spaces with underscores in remote /tmp filename for frictionless CLI attachment
            let safe_remote_name: String = fname
                .chars()
                .map(|c| if c.is_whitespace() { '_' } else { c })
                .collect();
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let remote_path = format!("/tmp/iterm-upload-{pid}-{ts:x}-{safe_remote_name}");
            let remote_label = format!("{host}:{remote_path}");
            on_status(UploadStatus::Started {
                local: local_path.clone(),
                remote: remote_label.clone(),
            });
            let bytes = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
            let args = build_scp_upload_args(&local_path, &host, port, &remote_path);
            match Command::new("/usr/bin/scp").args(&args).output() {
                Ok(out) if out.status.success() => {
                    on_status(UploadStatus::Completed { remote_path, bytes });
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    on_status(UploadStatus::Failed {
                        local: local_path,
                        error: if stderr.is_empty() {
                            format!("scp upload exited with {:?}", out.status.code())
                        } else {
                            stderr
                        },
                    });
                }
                Err(e) => {
                    on_status(UploadStatus::Failed {
                        local: local_path,
                        error: e.to_string(),
                    });
                }
            }
        })
        .ok();
}

/// Sanitize a remote or OSC-supplied filename so it can never escape `download_dir`.
pub fn sanitize_filename(raw: &str) -> String {
    let base = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download.bin");
    let cleaned: String = base
        .chars()
        .filter(|&c| c != '\0' && c != '/' && c != '\\')
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "download.bin".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Pick a non-colliding file path inside `download_dir` (`foo.png`, `foo (1).png`, ...).
pub fn unique_dest_path(download_dir: &Path, raw_name: &str) -> PathBuf {
    let clean = sanitize_filename(raw_name);
    let candidate = download_dir.join(&clean);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(&clean)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let ext = Path::new(&clean)
        .extension()
        .and_then(|s| s.to_str());
    for n in 1..10_000u32 {
        let name = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let p = download_dir.join(name);
        if !p.exists() {
            return p;
        }
    }
    download_dir.join(format!("{stem}-latest"))
}

/// Save an inline-protocol (`it2dl` / `OSC 1337;File=`) payload safely into `download_dir`.
pub fn save_inline_download(download_dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(download_dir).map_err(|e| e.to_string())?;
    let dest = unique_dest_path(download_dir, name);
    std::fs::write(&dest, data).map_err(|e| e.to_string())?;
    Ok(dest)
}

/// Build safe `argv` arguments for `/usr/bin/scp` using OpenSSH ControlMaster reuse.
/// Never passes arguments through a shell string — prevents all shell injection!
pub fn build_scp_args(
    host: &str,
    port: Option<u16>,
    remote_path: &str,
    dest_path: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-q".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        "ControlPersist=60".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if let Some(p) = port {
        args.push("-P".to_string());
        args.push(p.to_string());
    }
    // End of options guard (`--`) protects against remote hosts or paths starting with `-`
    args.push("--".to_string());
    args.push(format!("{host}:{remote_path}"));
    args.push(dest_path.to_string_lossy().into_owned());
    args
}

/// Download a remote file deterministically in a background thread using `/usr/bin/scp`.
pub fn download_remote_async<F>(
    download_dir: PathBuf,
    host: String,
    port: Option<u16>,
    remote_path: String,
    on_status: F,
) where
    F: Fn(DownloadStatus) + Send + 'static,
{
    thread::Builder::new()
        .name("remote-download".into())
        .spawn(move || {
            if let Err(e) = std::fs::create_dir_all(&download_dir) {
                on_status(DownloadStatus::Failed {
                    label: remote_path,
                    error: e.to_string(),
                });
                return;
            }
            let dest = unique_dest_path(&download_dir, &remote_path);
            let label = format!("{host}:{remote_path}");
            on_status(DownloadStatus::Started {
                label: label.clone(),
                dest: dest.clone(),
            });
            let args = build_scp_args(&host, port, &remote_path, &dest);
            let status = Command::new("/usr/bin/scp").args(&args).output();
            match status {
                Ok(out) if out.status.success() => {
                    let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                    on_status(DownloadStatus::Completed { dest, bytes });
                }
                Ok(out) => {
                    let _ = std::fs::remove_file(&dest);
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    on_status(DownloadStatus::Failed {
                        label,
                        error: if stderr.is_empty() {
                            format!("scp exited with {:?}", out.status.code())
                        } else {
                            stderr
                        },
                    });
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&dest);
                    on_status(DownloadStatus::Failed {
                        label,
                        error: e.to_string(),
                    });
                }
            }
        })
        .ok();
}

/// Download an explicit `RemoteArtifact` emitted via `OSC 1337;Artifact=...`.
pub fn download_artifact_async<F>(download_dir: PathBuf, artifact: RemoteArtifact, on_status: F)
where
    F: Fn(DownloadStatus) + Send + 'static,
{
    download_remote_async(
        download_dir,
        artifact.host,
        artifact.port,
        artifact.abs_path,
        on_status,
    );
}
