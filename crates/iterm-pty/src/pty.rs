use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("openpty failed: {0}")]
    OpenPty(io::Error),
    #[error("fork failed: {0}")]
    Fork(io::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            cols: 100,
            rows: 30,
            pixel_width: 1000,
            pixel_height: 600,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PtySpawnConfig {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub term: String,
    pub session_id: String,
    pub profile_name: String,
    pub size: PtySize,
}

impl PtySpawnConfig {
    pub fn default_login_shell(session_id: String, profile_name: String, term: String, size: PtySize) -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        Self {
            program: shell,
            args: vec!["-l".into()],
            cwd: std::env::var("HOME").ok(),
            term,
            session_id,
            profile_name,
            size,
        }
    }
}

/// Native POSIX pseudoterminal owning the master FD and child PID.
pub struct PtyHandle {
    master: OwnedFd,
    child_pid: libc::pid_t,
}

impl PtyHandle {
    pub fn spawn(cfg: &PtySpawnConfig) -> Result<Self, PtyError> {
        let mut master_fd: RawFd = -1;
        let mut slave_fd: RawFd = -1;
        let mut ws = libc::winsize {
            ws_row: cfg.size.rows,
            ws_col: cfg.size.cols,
            ws_xpixel: cfg.size.pixel_width,
            ws_ypixel: cfg.size.pixel_height,
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
            return Err(PtyError::OpenPty(io::Error::last_os_error()));
        }

        // Pre-allocate CStrings BEFORE fork() so child only calls async-signal-safe functions!
        let prog_c = CString::new(cfg.program.as_str()).unwrap_or_else(|_| CString::new("/bin/sh").unwrap());
        let mut argv_c = Vec::with_capacity(cfg.args.len() + 1);
        // Login shell argv[0] convention: prepend '-' if launching with -l or keep program name
        let prog_basename = std::path::Path::new(&cfg.program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("sh");
        let argv0 = if cfg.args.first().map(|s| s.as_str()) == Some("-l") {
            format!("-{prog_basename}")
        } else {
            cfg.program.clone()
        };
        argv_c.push(CString::new(argv0).unwrap_or_else(|_| prog_c.clone()));
        let skip_dash_l = cfg.args.first().map(|s| s.as_str()) == Some("-l");
        for arg in cfg.args.iter().skip(usize::from(skip_dash_l)) {
            if let Ok(c) = CString::new(arg.as_str()) {
                argv_c.push(c);
            }
        }
        let mut argv_ptrs: Vec<*const libc::c_char> = argv_c.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let cwd_c = cfg.cwd.as_deref().and_then(|s| CString::new(s).ok());

        // Build iTerm2-compatible environment variables before fork.
        // Also ensure UTF-8 locale fallback when launched from macOS Finder/Spotlight (`launchd` omits LANG!).
        let mut raw_env: Vec<(&str, &str)> = vec![
            ("TERM", cfg.term.as_str()),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.7.0"),
            ("LC_TERMINAL", "iTerm2"),
            ("LC_TERMINAL_VERSION", "3.7.0"),
            ("ITERM_SESSION_ID", cfg.session_id.as_str()),
            ("ITERM_PROFILE", cfg.profile_name.as_str()),
            ("ITERM_MINIMAL", "1"),
        ];
        let has_locale = std::env::var_os("LANG").is_some_and(|v| !v.is_empty())
            || std::env::var_os("LC_ALL").is_some_and(|v| !v.is_empty())
            || std::env::var_os("LC_CTYPE").is_some_and(|v| !v.is_empty());
        if !has_locale {
            raw_env.push(("LANG", "en_US.UTF-8"));
            raw_env.push(("LC_CTYPE", "UTF-8"));
        }
        let env_pairs: Vec<(CString, CString)> = raw_env
            .into_iter()
            .filter_map(|(k, v)| Some((CString::new(k).ok()?, CString::new(v).ok()?)))
            .collect();

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return Err(PtyError::Fork(io::Error::last_os_error()));
        }

        if pid == 0 {
            // Child process: async-signal-safe setup only
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
                // Reset default signal handlers
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);

                if let Some(ref dir) = cwd_c {
                    let _ = libc::chdir(dir.as_ptr());
                }
                for (k, v) in &env_pairs {
                    libc::setenv(k.as_ptr(), v.as_ptr(), 1);
                }
                libc::execvp(prog_c.as_ptr(), argv_ptrs.as_ptr());
                // Fallback to /bin/sh if configured shell failed
                let fallback = CStr::from_bytes_with_nul_unchecked(b"/bin/sh\0");
                let fb_argv = [fallback.as_ptr(), std::ptr::null()];
                libc::execvp(fallback.as_ptr(), fb_argv.as_ptr());
                libc::_exit(127);
            }
        }

        // Parent process
        unsafe {
            libc::close(slave_fd);
            // Set CLOEXEC and NONBLOCK on master FD for coalesced polling reads
            let flags = libc::fcntl(master_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(master_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
            let fl = libc::fcntl(master_fd, libc::F_GETFL);
            if fl >= 0 {
                libc::fcntl(master_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
        }

        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        Ok(Self {
            master,
            child_pid: pid,
        })
    }

    #[inline]
    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    #[inline]
    pub fn child_pid(&self) -> libc::pid_t {
        self.child_pid
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Single non-blocking write attempt to the PTY master FD.
    /// Returns `Ok(0)` on `EAGAIN`/`WouldBlock` instead of blocking the caller.
    pub fn write_nonblocking(&self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let fd = self.master.as_raw_fd();
        loop {
            let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(err);
        }
    }

    /// Write all bytes to the PTY master FD (bounded wait; prefer `TerminalSession::write_input` async queue).
    pub fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        let fd = self.master.as_raw_fd();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !data.is_empty() {
            let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
            if n > 0 {
                data = &data[n as usize..];
            } else if n == 0 {
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "PTY write timed out"));
                    }
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut pfd, 1, 50) };
                    continue;
                }
                return Err(err);
            }
        }
        Ok(())
    }

    /// Non-blocking check whether child process has exited.
    pub fn try_wait(&self) -> Option<i32> {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(self.child_pid, &mut status, libc::WNOHANG) };
        if rc == self.child_pid {
            if libc::WIFEXITED(status) {
                Some(libc::WEXITSTATUS(status))
            } else if libc::WIFSIGNALED(status) {
                Some(128 + libc::WTERMSIG(status))
            } else {
                Some(0)
            }
        } else {
            None
        }
    }

    pub fn terminate(&self) {
        unsafe {
            // Signal the entire session process group first (setsid() makes child_pid its pgid)
            if self.child_pid > 1 {
                libc::kill(-self.child_pid, libc::SIGHUP);
            }
            libc::kill(self.child_pid, libc::SIGHUP);
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        let _ = self.try_wait();
    }
}
