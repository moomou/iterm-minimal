use crate::pty::{PtyError, PtyHandle, PtySize, PtySpawnConfig};
use iterm_profile::Profile;
use iterm_vt::{TerminalEvent, TerminalScreen};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Unique session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum bytes gathered per coalesced PTY read pass (256 KiB — amortizes mutex & macOS 1 KiB reads).
const GATHER_BUF_SIZE: usize = 256 * 1024;
/// Maximum queued outbound PTY write bytes (16 MiB) preventing unbounded growth on paused shells.
const MAX_WRITE_QUEUE_BYTES: usize = 16 * 1024 * 1024;

/// Wakeup hook invoked when a session produces new screen output or events.
pub type WakeCallback = Arc<dyn Fn(SessionId) + Send + Sync>;

/// Live terminal session combining a POSIX PTY, coalesced I/O reader/writer thread, and `TerminalScreen`.
pub struct TerminalSession {
    pub id: SessionId,
    pub iterm_session_uuid: String,
    pub profile: Profile,
    pub screen: Arc<Mutex<TerminalScreen>>,
    pty: Arc<PtyHandle>,
    write_queue: Arc<Mutex<VecDeque<u8>>>,
    has_pending_writes: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    wake_pipe_write: RawFd,
    reader_thread: Option<JoinHandle<()>>,
}

impl TerminalSession {
    pub fn spawn(
        profile: Profile,
        size: PtySize,
        cwd_override: Option<String>,
        on_wake: Option<WakeCallback>,
    ) -> Result<Self, PtyError> {
        let numeric_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let id = SessionId(numeric_id);
        let iterm_session_uuid = format!("w0t0p{numeric_id}:ITERM-MINIMAL-{numeric_id:08X}");

        let mut spawn_cfg = if let Some(ref custom) = profile.custom_command {
            let parts = split_command_line(custom);
            let program = parts.first().cloned().unwrap_or_else(|| "/bin/zsh".into());
            let args = parts.into_iter().skip(1).collect();
            PtySpawnConfig {
                program,
                args,
                cwd: cwd_override,
                term: profile.terminal_type.clone(),
                session_id: iterm_session_uuid.clone(),
                profile_name: profile.name.clone(),
                size,
            }
        } else {
            let mut cfg = PtySpawnConfig::default_login_shell(
                iterm_session_uuid.clone(),
                profile.name.clone(),
                profile.terminal_type.clone(),
                size,
            );
            if cwd_override.is_some() {
                cfg.cwd = cwd_override;
            }
            cfg
        };

        if spawn_cfg.cwd.is_none() {
            if let iterm_profile::InitialDirectory::Custom(ref dir) = profile.initial_directory {
                if !dir.is_empty() {
                    spawn_cfg.cwd = Some(dir.clone());
                }
            }
        }

        let pty = Arc::new(PtyHandle::spawn(&spawn_cfg)?);
        let max_sb = if profile.unlimited_scrollback {
            1_000_000
        } else {
            profile.scrollback_lines
        };
        let mut screen_val = TerminalScreen::new(
            size.cols as usize,
            size.rows as usize,
            max_sb,
        );
        let to_rgb = |c: iterm_profile::Rgba| -> (u8, u8, u8) {
            (
                (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            )
        };
        screen_val.default_fg_rgb = to_rgb(profile.colors.foreground);
        screen_val.default_bg_rgb = to_rgb(profile.colors.background);
        screen_val.default_cursor_rgb = to_rgb(profile.colors.cursor);
        let screen = Arc::new(Mutex::new(screen_val));

        let write_queue = Arc::new(Mutex::new(VecDeque::new()));
        let has_pending_writes = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));

        // Self-pipe to cleanly wake the I/O thread for pending writes or shutdown
        let mut fds = [-1 as RawFd; 2];
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) == 0 {
                for fd in fds {
                    let fl = libc::fcntl(fd, libc::F_GETFL);
                    if fl >= 0 {
                        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
                    }
                    let fdfl = libc::fcntl(fd, libc::F_GETFD);
                    if fdfl >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, fdfl | libc::FD_CLOEXEC);
                    }
                }
            }
        }
        let wake_read = fds[0];
        let wake_write = fds[1];

        let t_pty = Arc::clone(&pty);
        let t_screen = Arc::clone(&screen);
        let t_wq = Arc::clone(&write_queue);
        let t_hpw = Arc::clone(&has_pending_writes);
        let t_dirty = Arc::clone(&dirty);
        let t_exited = Arc::clone(&exited);
        let t_stop = Arc::clone(&stop_flag);

        let reader_thread = thread::Builder::new()
            .name(format!("pty-reader-{numeric_id}"))
            .spawn(move || {
                run_coalesced_reader(
                    id,
                    t_pty,
                    t_screen,
                    t_wq,
                    t_hpw,
                    t_dirty,
                    t_exited,
                    t_stop,
                    wake_read,
                    on_wake,
                );
            })
            .ok();

        Ok(Self {
            id,
            iterm_session_uuid,
            profile,
            screen,
            pty,
            write_queue,
            has_pending_writes,
            dirty,
            exited,
            stop_flag,
            wake_pipe_write: wake_write,
            reader_thread,
        })
    }

    /// Enqueue outbound input/paste bytes WITHOUT EVER blocking the UI main thread!
    /// Fast-paths an immediate non-blocking `libc::write` when the queue is empty; any remaining
    /// bytes drain asynchronously via `POLLOUT` on the session's PTY I/O thread.
    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut q = self.write_queue.lock();
        let mut remaining = bytes;
        if q.is_empty() {
            match self.pty.write_nonblocking(remaining) {
                Ok(n) => remaining = &remaining[n..],
                Err(e) => return Err(e),
            }
        }
        if !remaining.is_empty() {
            let avail = MAX_WRITE_QUEUE_BYTES.saturating_sub(q.len());
            let to_push = remaining.len().min(avail);
            q.extend(&remaining[..to_push]);
            self.has_pending_writes.store(true, Ordering::Release);
            drop(q);
            if self.wake_pipe_write >= 0 {
                let b: u8 = 2;
                unsafe {
                    let _ = libc::write(self.wake_pipe_write, (&b as *const u8).cast(), 1);
                }
            }
        }
        Ok(())
    }

    /// Write pasted text, respecting bracketed paste mode and stripping raw ESC bytes for safety.
    pub fn write_paste(&self, text: &str, force_bracketed: Option<bool>) -> io::Result<()> {
        let bracketed = force_bracketed.unwrap_or_else(|| {
            self.screen
                .lock()
                .modes
                .contains(iterm_vt::TermModes::BRACKETED_PASTE)
        });
        if bracketed {
            // Strip ESC (0x1B) from pasted text so untrusted paste cannot break out of bracketed paste!
            let sanitized: String = text.chars().filter(|&c| c != '\x1b').collect();
            let mut out = Vec::with_capacity(sanitized.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(sanitized.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            self.write_input(&out)
        } else {
            self.write_input(text.as_bytes())
        }
    }

    pub fn resize(&self, size: PtySize, cell_w_px: f32, cell_h_px: f32) -> io::Result<()> {
        {
            let mut s = self.screen.lock();
            s.cell_width_px = cell_w_px;
            s.cell_height_px = cell_h_px;
            s.resize(size.cols as usize, size.rows as usize);
        }
        self.dirty.store(true, Ordering::Release);
        self.pty.resize(size)
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    #[inline]
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    #[inline]
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    #[inline]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    pub fn child_pid(&self) -> libc::pid_t {
        self.pty.child_pid()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Non-blocking teardown: signal SIGHUP + wake the background I/O thread without
        // blocking the macOS UI main thread on `reader_thread.join()` (prevents stalls when
        // closing hung SSH tabs!). The background reader thread cleanly reaps the child PID.
        self.stop_flag.store(true, Ordering::Release);
        self.pty.terminate();
        if self.wake_pipe_write >= 0 {
            let b: u8 = 1;
            unsafe {
                let _ = libc::write(self.wake_pipe_write, (&b as *const u8).cast(), 1);
                libc::close(self.wake_pipe_write);
            }
        }
        // Drop JoinHandle (detaches cleanly; reader exits in microseconds and reaps child PID)
        let _ = self.reader_thread.take();
    }
}

/// Drain as much of `write_queue` as the kernel PTY input buffer will accept without blocking.
fn drain_write_queue(
    pty: &PtyHandle,
    write_queue: &Mutex<VecDeque<u8>>,
    has_pending_writes: &AtomicBool,
) -> bool {
    let mut q = write_queue.lock();
    while !q.is_empty() {
        let (front, _) = q.as_slices();
        if front.is_empty() {
            q.make_contiguous();
            continue;
        }
        match pty.write_nonblocking(front) {
            Ok(0) => break, // Kernel buffer full; wait for POLLOUT
            Ok(n) => {
                q.drain(..n);
            }
            Err(_) => {
                q.clear();
                break;
            }
        }
    }
    let rem = !q.is_empty();
    has_pending_writes.store(rem, Ordering::Release);
    rem
}

/// High-throughput coalesced PTY reader + non-blocking async writer loop.
#[allow(clippy::too_many_arguments)]
fn run_coalesced_reader(
    id: SessionId,
    pty: Arc<PtyHandle>,
    screen: Arc<Mutex<TerminalScreen>>,
    write_queue: Arc<Mutex<VecDeque<u8>>>,
    has_pending_writes: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    wake_read_fd: RawFd,
    on_wake: Option<WakeCallback>,
) {
    let master_fd = pty.master_fd();
    let mut buf = vec![0u8; GATHER_BUF_SIZE];

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        // Lock-free fast check: zero mutex overhead on hot read loops when outbound queue is empty!
        let want_write = if has_pending_writes.load(Ordering::Acquire) {
            drain_write_queue(&pty, &write_queue, &has_pending_writes)
        } else {
            false
        };

        let mut pfds = [
            libc::pollfd {
                fd: master_fd,
                events: if want_write {
                    libc::POLLIN | libc::POLLOUT
                } else {
                    libc::POLLIN
                },
                revents: 0,
            },
            libc::pollfd {
                fd: wake_read_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds = if wake_read_fd >= 0 { 2 } else { 1 };
        // True zero-wakeup infinite kernel sleep (-1): wakes strictly on POLLIN / POLLHUP / POLLOUT / wake_pipe!
        // Gives 0.00 idle CPU wakeups/sec even with dozens of open tabs and split panes.
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue;
        }
        if nfds == 2 && (pfds[1].revents & libc::POLLIN) != 0 {
            let mut drain_buf = [0u8; 64];
            unsafe {
                while libc::read(wake_read_fd, drain_buf.as_mut_ptr().cast(), drain_buf.len()) > 0 {}
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            if has_pending_writes.load(Ordering::Acquire) {
                drain_write_queue(&pty, &write_queue, &has_pending_writes);
            }
        }

        let revents = pfds[0].revents;
        if (revents & libc::POLLOUT) != 0 && has_pending_writes.load(Ordering::Acquire) {
            drain_write_queue(&pty, &write_queue, &has_pending_writes);
        }
        if (revents & (libc::POLLIN | libc::POLLHUP)) == 0 {
            continue;
        }

        let mut total = 0usize;
        let mut eof = false;

        // Drain ALL immediately available bytes from kernel PTY buffer up to 256 KiB
        // with ZERO artificial poll sleeps — maximum throughput AND sub-ms interactive wakeup!
        while total < GATHER_BUF_SIZE {
            let n = unsafe {
                libc::read(
                    master_fd,
                    buf[total..].as_mut_ptr().cast(),
                    GATHER_BUF_SIZE - total,
                )
            };
            if n > 0 {
                total += n as usize;
                continue;
            } else if n == 0 {
                eof = true;
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                eof = true;
                break;
            }
        }

        if total > 0 {
            let writebacks: Vec<Vec<u8>> = {
                let mut s = screen.lock();
                s.feed(&buf[..total]);
                if s.events.is_empty() {
                    Vec::new()
                } else {
                    let mut wb = Vec::new();
                    s.events.retain(|ev| {
                        if let TerminalEvent::WriteBack(bytes) = ev {
                            wb.push(bytes.clone());
                            false
                        } else {
                            true
                        }
                    });
                    wb
                }
            };
            // Enqueue WriteBack replies (`DSR`/`DA`/`OSC 11`/`DECRQM`) asynchronously so
            // a child flooding queries without draining stdin can NEVER deadlock the reader!
            if !writebacks.is_empty() {
                {
                    let mut q = write_queue.lock();
                    for bytes in writebacks {
                        let avail = MAX_WRITE_QUEUE_BYTES.saturating_sub(q.len());
                        let take = bytes.len().min(avail);
                        q.extend(&bytes[..take]);
                    }
                    has_pending_writes.store(true, Ordering::Release);
                }
                drain_write_queue(&pty, &write_queue, &has_pending_writes);
            }
            dirty.store(true, Ordering::Release);
            if let Some(ref cb) = on_wake {
                cb(id);
            }
        }

        if eof || (revents & libc::POLLHUP) != 0 {
            if total == 0 {
                exited.store(true, Ordering::Release);
                if let Some(ref cb) = on_wake {
                    cb(id);
                }
                break;
            }
        }
    }

    if wake_read_fd >= 0 {
        unsafe {
            libc::close(wake_read_fd);
        }
    }

    // Best-effort non-blocking child reap off the main thread so closed tabs leave zero zombies
    if stop.load(Ordering::Acquire) {
        for _ in 0..10 {
            if pty.try_wait().is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// POSIX quote-aware command splitter for iTerm2 profile custom commands.
fn split_command_line(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            other => cur.push(other),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
