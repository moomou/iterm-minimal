use iterm_profile::RemoteArtifact;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

/// Unique identifier for a tracked file transfer in `TransferQueue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(pub u64);

static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

/// Live UI-ready snapshot of one queued, active, or completed transfer.
#[derive(Debug, Clone)]
pub struct TransferSnapshot {
    pub id: TransferId,
    pub direction: TransferDirection,
    pub label: String,
    pub host: String,
    pub dest_or_remote: String,
    pub percent: Option<u8>,
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
    pub speed: String,
    pub eta: String,
    pub state: TransferState,
    pub finished_at: Option<Instant>,
}

/// Parsed live progress sample from OpenSSH `scp` TTY progress meter output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpProgress {
    pub percent: u8,
    pub bytes_done: u64,
    pub speed: String,
    pub eta: String,
}

/// Format raw byte count cleanly for compact HUD display (`512 B`, `12.4 KB`, `18.5 MB`, `1.24 GB`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2}GB", b / GB)
    } else if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{bytes}B")
    }
}

/// Parse OpenSSH human-readable byte sizes (`512`, `1248KB`, `18.5MB`, `1.2GB`, `4.0TB`).
pub fn parse_human_bytes(tok: &str) -> Option<u64> {
    let t = tok.trim();
    if t.is_empty() {
        return None;
    }
    let (num_str, mult): (&str, f64) = if let Some(s) = t.strip_suffix("TB") {
        (s, 1024.0 * 1024.0 * 1024.0 * 1024.0)
    } else if let Some(s) = t.strip_suffix("GB") {
        (s, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(s) = t.strip_suffix("MB") {
        (s, 1024.0 * 1024.0)
    } else if let Some(s) = t.strip_suffix("KB") {
        (s, 1024.0)
    } else if let Some(s) = t.strip_suffix('B') {
        (s, 1.0)
    } else {
        (t, 1.0)
    };
    let val: f64 = num_str.trim().parse().ok()?;
    if val < 0.0 || !val.is_finite() {
        return None;
    }
    Some((val * mult).round() as u64)
}

/// Parse one `\r`/`\n`-delimited OpenSSH `scp` TTY progress line, e.g.:
/// - `dataset.tar.gz   42%   42MB  12.4MB/s   00:04 ETA`
/// - `my file.png     100% 1248KB  15.2MB/s   00:00`
/// - `huge.bin         12%   12MB   0.0KB/s - stalled -`
pub fn parse_scp_progress_line(raw: &str) -> Option<ScpProgress> {
    // Strip any ANSI CSI/OSC escape sequences if present
    let mut clean = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for ch in chars.by_ref() {
                    if (0x40..=0x7E).contains(&(ch as u32)) {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_control() {
            clean.push(c);
        }
    }

    let tokens: Vec<&str> = clean.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }
    // Scan from the right for `<0..=100>%` token (robust even if filename contains `%`!)
    let mut pct_idx = None;
    let mut pct_val = 0u8;
    for (idx, tok) in tokens.iter().enumerate().rev() {
        if let Some(num_s) = tok.strip_suffix('%') {
            if let Ok(v) = num_s.parse::<u8>() {
                if v <= 100 && idx + 1 < tokens.len() {
                    pct_idx = Some(idx);
                    pct_val = v;
                    break;
                }
            }
        }
    }
    let idx = pct_idx?;
    let bytes_done = parse_human_bytes(tokens.get(idx + 1)?)?;
    let speed = tokens
        .get(idx + 2)
        .copied()
        .filter(|s| s.contains("/s"))
        .unwrap_or("")
        .to_string();
    let eta_start = if speed.is_empty() { idx + 2 } else { idx + 3 };
    let eta = if eta_start < tokens.len() {
        let joined = tokens[eta_start..].join(" ");
        joined
            .trim_end_matches("ETA")
            .trim()
            .trim_matches('-')
            .trim()
            .to_string()
    } else {
        String::new()
    };
    Some(ScpProgress {
        percent: pct_val,
        bytes_done,
        speed,
        eta,
    })
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

// ---------------------------------------------------------------------------
// PTY-Wrapped Live-Progress `scp` Runner & Bounded `TransferQueue`
// ---------------------------------------------------------------------------

/// Build `/usr/bin/scp` args WITH live TTY progress enabled (omits `-q` so OpenSSH emits
/// live `%`, transferred bytes, speed, and ETA over our dedicated 120-col POSIX PTY!).
fn build_scp_progress_args(
    src: &str,
    dst: &str,
    port: Option<u16>,
) -> Vec<String> {
    let mut args = vec![
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
    args.push(src.to_string());
    args.push(dst.to_string());
    args
}

/// Execute `/usr/bin/scp` attached to a dedicated 120x24 POSIX PTY as session leader
/// (`setsid` + `TIOCSCTTY`), so OpenSSH's `can_output()` (`getpgrp() == tcgetpgrp(STDOUT_FILENO)`)
/// enables its native live progress meter with ZERO external `pv`/`script` dependencies!
fn run_scp_with_pty_progress<F>(
    args: &[String],
    local_dest_for_stat: Option<&Path>,
    total_bytes: Option<u64>,
    cancel_flag: &AtomicBool,
    active_pid: &AtomicI32,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(Option<u8>, u64, String, String),
{
    if cancel_flag.load(Ordering::Acquire) {
        return Err("Cancelled".into());
    }

    let mut master_fd: RawFd = -1;
    let mut slave_fd: RawFd = -1;
    let mut ws = libc::winsize {
        ws_row: 24,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(format!("openpty failed: {}", std::io::Error::last_os_error()));
    }

    let prog_c = CString::new("/usr/bin/scp").unwrap();
    let mut argv_c = Vec::with_capacity(args.len() + 1);
    argv_c.push(prog_c.clone());
    for a in args {
        argv_c.push(CString::new(a.as_str()).map_err(|_| "invalid NUL in scp arg".to_string())?);
    }
    let mut argv_ptrs: Vec<*const libc::c_char> = argv_c.iter().map(|s| s.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(format!("fork failed: {}", std::io::Error::last_os_error()));
    }

    if pid == 0 {
        unsafe {
            libc::close(master_fd);
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave_fd, libc::STDIN_FILENO);
            libc::dup2(slave_fd, libc::STDOUT_FILENO);
            libc::dup2(slave_fd, libc::STDERR_FILENO);
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::execv(prog_c.as_ptr(), argv_ptrs.as_ptr());
            libc::_exit(127);
        }
    }

    // Parent: close slave, set master non-blocking + CLOEXEC, record child PID for cancellation
    unsafe {
        libc::close(slave_fd);
        let fdfl = libc::fcntl(master_fd, libc::F_GETFD);
        if fdfl >= 0 {
            libc::fcntl(master_fd, libc::F_SETFD, fdfl | libc::FD_CLOEXEC);
        }
        let fl = libc::fcntl(master_fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(master_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    active_pid.store(pid, Ordering::Release);

    let mut line_buf = Vec::<u8>::with_capacity(256);
    let mut read_buf = [0u8; 4096];
    let mut last_error_line = String::new();
    let mut last_bytes_reported = 0u64;
    let mut last_stat_check = Instant::now();
    let started_at = Instant::now();
    let mut child_status: Option<libc::c_int> = None;

    loop {
        if cancel_flag.load(Ordering::Acquire) {
            unsafe {
                if pid > 1 {
                    libc::kill(-pid, libc::SIGTERM);
                }
                libc::kill(pid, libc::SIGTERM);
            }
        }

        let mut pfd = libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let prc = unsafe { libc::poll(&mut pfd, 1, 150) };
        if prc > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP)) != 0 {
            loop {
                let n = unsafe {
                    libc::read(master_fd, read_buf.as_mut_ptr().cast(), read_buf.len())
                };
                if n > 0 {
                    for &b in &read_buf[..n as usize] {
                        if b == b'\r' || b == b'\n' {
                            if !line_buf.is_empty() {
                                let line = String::from_utf8_lossy(&line_buf);
                                let trimmed = line.trim();
                                if let Some(prog) = parse_scp_progress_line(trimmed) {
                                    last_bytes_reported = last_bytes_reported.max(prog.bytes_done);
                                    on_progress(
                                        Some(prog.percent),
                                        last_bytes_reported,
                                        prog.speed,
                                        prog.eta,
                                    );
                                } else if !trimmed.is_empty() {
                                    last_error_line = trimmed.to_string();
                                }
                                line_buf.clear();
                            }
                        } else if line_buf.len() < 1024 {
                            line_buf.push(b);
                        }
                    }
                } else {
                    break;
                }
            }
        }

        // Supplement with partial local file stat for downloads so progress updates even before first TTY flush
        if let Some(dest) = local_dest_for_stat {
            if last_stat_check.elapsed() >= Duration::from_millis(200) {
                last_stat_check = Instant::now();
                if let Ok(md) = std::fs::metadata(dest) {
                    let cur = md.len();
                    if cur > last_bytes_reported {
                        last_bytes_reported = cur;
                        let elapsed = started_at.elapsed().as_secs_f64().max(0.05);
                        let bps = (cur as f64 / elapsed) as u64;
                        let pct = total_bytes
                            .filter(|&t| t > 0)
                            .map(|t| ((cur.saturating_mul(100) / t).min(100)) as u8);
                        on_progress(pct, cur, format!("{}/s", format_bytes(bps)), String::new());
                    }
                }
            }
        }

        let mut st: libc::c_int = 0;
        let w = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if w == pid {
            child_status = Some(st);
            break;
        }
        if (pfd.revents & libc::POLLHUP) != 0 && prc > 0 {
            // Drain exit status blocking briefly after PTY EOF
            let w2 = unsafe { libc::waitpid(pid, &mut st, 0) };
            if w2 == pid {
                child_status = Some(st);
            }
            break;
        }
    }

    active_pid.store(0, Ordering::Release);
    unsafe {
        libc::close(master_fd);
    }

    if cancel_flag.load(Ordering::Acquire) {
        return Err("Cancelled".into());
    }

    match child_status {
        Some(st) if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0 => Ok(()),
        Some(st) => {
            let code = if libc::WIFEXITED(st) {
                libc::WEXITSTATUS(st)
            } else {
                -1
            };
            if last_error_line.is_empty() {
                Err(format!("scp exited with code {code}"))
            } else {
                Err(last_error_line)
            }
        }
        None => Err("scp process terminated unexpectedly".into()),
    }
}

/// Callback delivered when a queued transfer finishes (for pasting uploaded remote path or logging).
pub type TransferCompletionHook = Box<dyn FnOnce(TransferSnapshot) + Send + 'static>;

enum TransferJobKind {
    Download {
        download_dir: PathBuf,
        host: String,
        port: Option<u16>,
        remote_path: String,
        expected_bytes: Option<u64>,
    },
    Upload {
        local_path: PathBuf,
        host: String,
        port: Option<u16>,
        remote_path: String,
        cleanup_local: bool,
    },
}

struct QueuedJob {
    id: TransferId,
    kind: TransferJobKind,
    on_done: Option<TransferCompletionHook>,
}

struct TransferEntry {
    snap: TransferSnapshot,
    cancel_flag: Arc<AtomicBool>,
    active_pid: Arc<AtomicI32>,
}

struct QueueInner {
    entries: Vec<TransferEntry>,
    pending: VecDeque<QueuedJob>,
    running: usize,
    max_concurrent: usize,
}

/// Bounded-concurrency SSH/SCP file transfer queue with live PTY progress tracking and cancellation.
#[derive(Clone)]
pub struct TransferQueue {
    inner: Arc<Mutex<QueueInner>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl TransferQueue {
    pub fn new<F>(max_concurrent: usize, on_change: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(Mutex::new(QueueInner {
                entries: Vec::new(),
                pending: VecDeque::new(),
                running: 0,
                max_concurrent: max_concurrent.max(1),
            })),
            on_change: Arc::new(on_change),
        }
    }

    /// Enqueue a remote `scp` download with live progress tracking.
    pub fn enqueue_download(
        &self,
        download_dir: PathBuf,
        host: String,
        port: Option<u16>,
        remote_path: String,
        expected_bytes: Option<u64>,
        on_done: Option<TransferCompletionHook>,
    ) -> TransferId {
        let id = TransferId(NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed));
        let label = sanitize_filename(&remote_path);
        let entry = TransferEntry {
            snap: TransferSnapshot {
                id,
                direction: TransferDirection::Download,
                label,
                host: host.clone(),
                dest_or_remote: download_dir.to_string_lossy().into_owned(),
                percent: Some(0),
                bytes_done: 0,
                total_bytes: expected_bytes,
                speed: String::new(),
                eta: String::new(),
                state: TransferState::Queued,
                finished_at: None,
            },
            cancel_flag: Arc::new(AtomicBool::new(false)),
            active_pid: Arc::new(AtomicI32::new(0)),
        };
        {
            let mut g = self.inner.lock();
            g.entries.push(entry);
            g.pending.push_back(QueuedJob {
                id,
                kind: TransferJobKind::Download {
                    download_dir,
                    host,
                    port,
                    remote_path,
                    expected_bytes,
                },
                on_done,
            });
            Self::trim_history(&mut g.entries);
        }
        (self.on_change)();
        self.pump();
        id
    }

    /// Enqueue an explicit `OSC 1337;Artifact=` download (preserves `artifact.size` for instant `%`!).
    pub fn enqueue_artifact(
        &self,
        download_dir: PathBuf,
        artifact: RemoteArtifact,
        on_done: Option<TransferCompletionHook>,
    ) -> TransferId {
        self.enqueue_download(
            download_dir,
            artifact.host,
            artifact.port,
            artifact.abs_path,
            artifact.size,
            on_done,
        )
    }

    /// Enqueue a local -> remote `scp` upload with live progress tracking.
    pub fn enqueue_upload(
        &self,
        local_path: PathBuf,
        host: String,
        port: Option<u16>,
        cleanup_local: bool,
        on_done: Option<TransferCompletionHook>,
    ) -> TransferId {
        let id = TransferId(NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed));
        let fname = local_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(sanitize_filename)
            .unwrap_or_else(|| "upload.bin".into());
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
        let total_bytes = std::fs::metadata(&local_path).ok().map(|m| m.len());

        let entry = TransferEntry {
            snap: TransferSnapshot {
                id,
                direction: TransferDirection::Upload,
                label: fname,
                host: host.clone(),
                dest_or_remote: remote_path.clone(),
                percent: Some(0),
                bytes_done: 0,
                total_bytes,
                speed: String::new(),
                eta: String::new(),
                state: TransferState::Queued,
                finished_at: None,
            },
            cancel_flag: Arc::new(AtomicBool::new(false)),
            active_pid: Arc::new(AtomicI32::new(0)),
        };
        {
            let mut g = self.inner.lock();
            g.entries.push(entry);
            g.pending.push_back(QueuedJob {
                id,
                kind: TransferJobKind::Upload {
                    local_path,
                    host,
                    port,
                    remote_path,
                    cleanup_local,
                },
                on_done,
            });
            Self::trim_history(&mut g.entries);
        }
        (self.on_change)();
        self.pump();
        id
    }

    /// Record an already-completed inline `it2dl` transfer so all downloads appear in one unified queue.
    pub fn record_inline_completed(&self, label: String, dest: PathBuf, bytes: u64) -> TransferId {
        let id = TransferId(NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed));
        let entry = TransferEntry {
            snap: TransferSnapshot {
                id,
                direction: TransferDirection::Download,
                label,
                host: "inline".into(),
                dest_or_remote: dest.to_string_lossy().into_owned(),
                percent: Some(100),
                bytes_done: bytes,
                total_bytes: Some(bytes),
                speed: String::new(),
                eta: String::new(),
                state: TransferState::Completed,
                finished_at: Some(Instant::now()),
            },
            cancel_flag: Arc::new(AtomicBool::new(false)),
            active_pid: Arc::new(AtomicI32::new(0)),
        };
        {
            let mut g = self.inner.lock();
            g.entries.push(entry);
            Self::trim_history(&mut g.entries);
        }
        (self.on_change)();
        id
    }

    /// Cancel a queued or running transfer immediately (`SIGTERM` to its `scp` process group).
    pub fn cancel(&self, id: TransferId) -> bool {
        let mut changed = false;
        {
            let mut g = self.inner.lock();
            g.pending.retain(|j| j.id != id);
            if let Some(entry) = g.entries.iter_mut().find(|e| e.snap.id == id) {
                if matches!(entry.snap.state, TransferState::Queued | TransferState::Running) {
                    entry.cancel_flag.store(true, Ordering::Release);
                    let pid = entry.active_pid.load(Ordering::Acquire);
                    if pid > 1 {
                        unsafe {
                            libc::kill(-pid, libc::SIGTERM);
                            libc::kill(pid, libc::SIGTERM);
                        }
                    }
                    entry.snap.state = TransferState::Cancelled;
                    entry.snap.finished_at = Some(Instant::now());
                    changed = true;
                }
            }
        }
        if changed {
            (self.on_change)();
        }
        changed
    }

    /// Remove all completed/failed/cancelled entries while preserving queued/running transfers.
    pub fn clear_finished(&self) {
        let mut g = self.inner.lock();
        g.entries
            .retain(|e| matches!(e.snap.state, TransferState::Queued | TransferState::Running));
        drop(g);
        (self.on_change)();
    }

    /// Return current transfer snapshots ordered active/queued first, then newest completed first.
    pub fn snapshots(&self) -> Vec<TransferSnapshot> {
        let g = self.inner.lock();
        let mut out: Vec<TransferSnapshot> = g.entries.iter().map(|e| e.snap.clone()).collect();
        out.sort_by(|a, b| {
            let rank = |s: &TransferState| match s {
                TransferState::Running => 0u8,
                TransferState::Queued => 1u8,
                _ => 2u8,
            };
            rank(&a.state)
                .cmp(&rank(&b.state))
                .then_with(|| b.id.cmp(&a.id))
        });
        out
    }

    pub fn active_count(&self) -> usize {
        self.inner
            .lock()
            .entries
            .iter()
            .filter(|e| matches!(e.snap.state, TransferState::Queued | TransferState::Running))
            .count()
    }

    fn trim_history(entries: &mut Vec<TransferEntry>) {
        // Keep up to 32 total entries, never dropping Queued/Running transfers
        if entries.len() <= 32 {
            return;
        }
        let mut to_drop = entries.len() - 32;
        entries.retain(|e| {
            if to_drop > 0 && !matches!(e.snap.state, TransferState::Queued | TransferState::Running) {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
    }

    fn pump(&self) {
        loop {
            let job_opt = {
                let mut g = self.inner.lock();
                if g.running >= g.max_concurrent {
                    None
                } else if let Some(job) = g.pending.pop_front() {
                    if let Some(entry) = g.entries.iter_mut().find(|e| e.snap.id == job.id) {
                        if entry.cancel_flag.load(Ordering::Acquire) {
                            continue;
                        }
                        entry.snap.state = TransferState::Running;
                        let cf = Arc::clone(&entry.cancel_flag);
                        let ap = Arc::clone(&entry.active_pid);
                        g.running += 1;
                        Some((job, cf, ap))
                    } else {
                        continue;
                    }
                } else {
                    None
                }
            };

            let Some((job, cancel_flag, active_pid)) = job_opt else {
                break;
            };
            let q = self.clone();
            (self.on_change)();
            thread::Builder::new()
                .name(format!("scp-transfer-{}", job.id.0))
                .spawn(move || {
                    q.execute_job(job, cancel_flag, active_pid);
                })
                .ok();
        }
    }

    fn execute_job(
        &self,
        job: QueuedJob,
        cancel_flag: Arc<AtomicBool>,
        active_pid: Arc<AtomicI32>,
    ) {
        let id = job.id;
        let on_change = Arc::clone(&self.on_change);
        let inner = Arc::clone(&self.inner);

        let update_prog = |pct: Option<u8>, bytes: u64, speed: String, eta: String| {
            {
                let mut g = inner.lock();
                if let Some(e) = g.entries.iter_mut().find(|e| e.snap.id == id) {
                    if let Some(p) = pct {
                        e.snap.percent = Some(p);
                    }
                    e.snap.bytes_done = bytes;
                    if !speed.is_empty() {
                        e.snap.speed = speed;
                    }
                    if !eta.is_empty() {
                        e.snap.eta = eta;
                    }
                }
            }
            on_change();
        };

        let result_snap: TransferSnapshot = match job.kind {
            TransferJobKind::Download {
                download_dir,
                host,
                port,
                remote_path,
                expected_bytes,
            } => {
                let res = if let Err(e) = std::fs::create_dir_all(&download_dir) {
                    Err(e.to_string())
                } else {
                    let dest = unique_dest_path(&download_dir, &remote_path);
                    {
                        let mut g = inner.lock();
                        if let Some(e) = g.entries.iter_mut().find(|e| e.snap.id == id) {
                            e.snap.dest_or_remote = dest.to_string_lossy().into_owned();
                        }
                    }
                    let src = format!("{host}:{remote_path}");
                    let dst_str = dest.to_string_lossy().into_owned();
                    let args = build_scp_progress_args(&src, &dst_str, port);
                    match run_scp_with_pty_progress(
                        &args,
                        Some(&dest),
                        expected_bytes,
                        &cancel_flag,
                        &active_pid,
                        update_prog,
                    ) {
                        Ok(()) => {
                            let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                            Ok((dest.to_string_lossy().into_owned(), bytes))
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&dest);
                            Err(e)
                        }
                    }
                };
                self.finish_entry(id, res, &cancel_flag)
            }
            TransferJobKind::Upload {
                local_path,
                host,
                port,
                remote_path,
                cleanup_local,
            } => {
                let total = std::fs::metadata(&local_path).ok().map(|m| m.len());
                let src_str = local_path.to_string_lossy().into_owned();
                let dst = format!("{host}:{remote_path}");
                let args = build_scp_progress_args(&src_str, &dst, port);
                let res = match run_scp_with_pty_progress(
                    &args,
                    None,
                    total,
                    &cancel_flag,
                    &active_pid,
                    update_prog,
                ) {
                    Ok(()) => Ok((remote_path, total.unwrap_or(0))),
                    Err(e) => Err(e),
                };
                if cleanup_local {
                    let _ = std::fs::remove_file(&local_path);
                }
                self.finish_entry(id, res, &cancel_flag)
            }
        };

        if let Some(cb) = job.on_done {
            cb(result_snap);
        }
        (self.on_change)();
        self.pump();
    }

    fn finish_entry(
        &self,
        id: TransferId,
        res: Result<(String, u64), String>,
        cancel_flag: &AtomicBool,
    ) -> TransferSnapshot {
        let mut g = self.inner.lock();
        g.running = g.running.saturating_sub(1);
        if let Some(e) = g.entries.iter_mut().find(|e| e.snap.id == id) {
            e.snap.finished_at = Some(Instant::now());
            if cancel_flag.load(Ordering::Acquire) {
                e.snap.state = TransferState::Cancelled;
            } else {
                match res {
                    Ok((dest_or_remote, bytes)) => {
                        e.snap.dest_or_remote = dest_or_remote;
                        e.snap.bytes_done = bytes;
                        e.snap.total_bytes = Some(bytes);
                        e.snap.percent = Some(100);
                        e.snap.eta.clear();
                        e.snap.state = TransferState::Completed;
                    }
                    Err(err) => {
                        e.snap.state = TransferState::Failed(err);
                    }
                }
            }
            return e.snap.clone();
        }
        TransferSnapshot {
            id,
            direction: TransferDirection::Download,
            label: String::new(),
            host: String::new(),
            dest_or_remote: String::new(),
            percent: None,
            bytes_done: 0,
            total_bytes: None,
            speed: String::new(),
            eta: String::new(),
            state: TransferState::Cancelled,
            finished_at: Some(Instant::now()),
        }
    }
}
