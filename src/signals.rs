use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGTSTP, SIGWINCH};

/// Signal state shared with the handlers, plus a self-pipe so that waits on
/// file descriptors wake up when a signal arrives.
pub struct Signals {
    terminate: Arc<AtomicBool>,
    winch: Arc<AtomicBool>,
    tstp: Arc<AtomicBool>,
    wake: UnixStream,
}

/// Whether `sig` currently has its default disposition. A job-control-less
/// parent may have set SIGTSTP to be ignored, and then suspending on it
/// would leave the process stopped with nobody to continue it.
fn is_default(sig: libc::c_int) -> bool {
    // SAFETY: querying the current action with a null new action is always
    // allowed and only writes to `old`.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_DFL
    }
}

impl Signals {
    pub fn install() -> io::Result<Self> {
        let (wake, notify) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        notify.set_nonblocking(true)?;

        let terminate = Arc::new(AtomicBool::new(false));
        let winch = Arc::new(AtomicBool::new(false));
        let tstp = Arc::new(AtomicBool::new(false));

        let register = |sig, flag: &Arc<AtomicBool>| -> io::Result<()> {
            signal_hook::flag::register(sig, Arc::clone(flag))?;
            signal_hook::low_level::pipe::register(sig, notify.try_clone()?)?;
            Ok(())
        };
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            register(sig, &terminate)?;
        }
        register(SIGWINCH, &winch)?;
        if is_default(SIGTSTP) {
            register(SIGTSTP, &tstp)?;
        }

        Ok(Signals {
            terminate,
            winch,
            tstp,
            wake,
        })
    }

    /// Descriptor that becomes readable whenever a handled signal arrives.
    pub fn fd(&self) -> i32 {
        self.wake.as_raw_fd()
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!((&self.wake).read(&mut buf), Ok(n) if n > 0) {}
    }

    pub fn terminate_requested(&self) -> bool {
        self.terminate.load(Ordering::SeqCst)
    }

    pub fn resize_pending(&self) -> bool {
        self.winch.load(Ordering::SeqCst)
    }

    pub fn take_resize(&self) -> bool {
        self.winch.swap(false, Ordering::SeqCst)
    }

    pub fn take_suspend(&self) -> bool {
        self.tstp.swap(false, Ordering::SeqCst)
    }

    /// Stops the process the way the default SIGTSTP action would; returns
    /// once it is continued.
    pub fn stop_self(&self) {
        let _ = signal_hook::low_level::emulate_default_handler(SIGTSTP);
    }
}

/// Waits until one of `fds` is readable or `timeout` passes (`None` waits
/// indefinitely). Returns the readiness of each descriptor, all false on
/// timeout, or `None` when interrupted by a signal.
pub fn wait_readable(fds: &[i32], timeout: Option<Duration>) -> io::Result<Option<Vec<bool>>> {
    let mut pollfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let timeout_ms = match timeout {
        None => -1,
        Some(d) => i32::try_from(d.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX),
    };
    // SAFETY: `pollfds` is a valid, exclusively borrowed array of the given length.
    let rc = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            timeout_ms,
        )
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(err);
    }
    Ok(Some(
        pollfds
            .iter()
            .map(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
            .collect(),
    ))
}
