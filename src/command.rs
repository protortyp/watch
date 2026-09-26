use std::ffi::OsString;
use std::io::{self, Cursor, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::args::Options;
use crate::locale::strerror;
use crate::output::{DiffCache, Flags, render_output};
use crate::terminal::WinSize;
use crate::window::Window;

/// A failure that ends watch with `code`, reported as `watch: <message>`.
#[derive(Debug)]
pub struct Fatal {
    pub code: i32,
    pub message: String,
}

impl Fatal {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Fatal {
            code,
            message: message.into(),
        }
    }

    pub fn io(code: i32, what: &str, err: &io::Error) -> Self {
        let text = match err.raw_os_error() {
            Some(errno) => strerror(errno),
            None => err.to_string(),
        };
        Fatal::new(code, format!("{what}: {text}"))
    }
}

/// Everything a run hands back once the command's output is consumed and
/// the command has exited.
pub struct Finished {
    pub window: Window,
    pub cache: DiffCache,
    pub changed: bool,
    pub exit_code: u8,
}

/// A command whose output is being rendered on a worker thread.
pub struct Job {
    handle: JoinHandle<Finished>,
    done: UnixStream,
}

impl Job {
    /// Descriptor that becomes readable once the job has finished.
    pub fn fd(&self) -> i32 {
        self.done.as_raw_fd()
    }

    pub fn join(self) -> Finished {
        match self.handle.join() {
            Ok(finished) => finished,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

pub struct Run<'a> {
    pub opts: &'a Options,
    pub flags: Flags,
    pub first_screen: bool,
    /// Terminal size exported to the command as `LINES` and `COLUMNS`.
    pub env_size: WinSize,
    /// Size of the output area, used as the pseudo-terminal size.
    pub view: (usize, usize),
}

/// Encodes a wait status the way upstream reports it: the exit status, or
/// 128 plus the number of the signal that terminated the command.
fn exit_code(status: ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code as u8,
        (None, Some(sig)) => 0x80 + (sig & 0x7f) as u8,
        (None, None) => 0x7f,
    }
}

fn wait_pid(pid: libc::pid_t) -> u8 {
    let mut status = 0;
    loop {
        // SAFETY: waitpid only writes the status through the pointer.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 0x7f;
        }
    }
    exit_code(ExitStatus::from_raw(status))
}

fn ring_bell() {
    // SAFETY: writes one byte from a static buffer to standard output.
    unsafe {
        libc::write(libc::STDOUT_FILENO, b"\x07".as_ptr().cast(), 1);
    }
}

fn spawn_failure_output(run: &Run<'_>, err: &io::Error) -> Vec<u8> {
    let what = if run.opts.exec {
        run.opts.command_argv[0].clone()
    } else {
        run.opts.command.clone()
    };
    let mut message = what.into_encoded_bytes();
    if run.opts.exec {
        message.extend_from_slice(b": ");
        let text = match err.raw_os_error() {
            Some(errno) => strerror(errno),
            None => err.to_string(),
        };
        message.extend_from_slice(text.as_bytes());
    } else {
        message.extend_from_slice(b": unable to run");
    }
    message
}

/// Whether a spawn error means no process could be created at all, rather
/// than that the command could not be executed.
fn is_fork_failure(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::ENOMEM))
}

/// Starts the command and renders its output into `window` on a worker thread.
pub fn start(run: Run<'_>, window: Window, cache: DiffCache) -> Result<Job, Fatal> {
    let (done, notify) = UnixStream::pair().map_err(|e| Fatal::io(2, "socketpair", &e))?;
    let flags = run.flags;
    let first_screen = run.first_screen;

    let (output, wait): (Box<dyn Read + Send>, Box<dyn FnOnce() -> u8 + Send>) = if run.opts.tty {
        spawn_tty(&run)?
    } else {
        spawn_piped(&run)?
    };

    let handle = thread::spawn(move || {
        let mut window = window;
        let mut cache = cache;
        let changed = render_output(
            output,
            &mut window,
            &mut cache,
            flags,
            first_screen,
            &mut ring_bell,
        );
        let exit_code = wait();
        drop(notify);
        Finished {
            window,
            cache,
            changed,
            exit_code,
        }
    });
    Ok(Job { handle, done })
}

type Spawned = (Box<dyn Read + Send>, Box<dyn FnOnce() -> u8 + Send>);

fn spawn_piped(run: &Run<'_>) -> Result<Spawned, Fatal> {
    let (reader, writer) =
        os_pipe::pipe().map_err(|e| Fatal::io(2, "unable to create IPC pipes", &e))?;
    let writer_err = writer
        .try_clone()
        .map_err(|e| Fatal::io(2, "unable to create IPC pipes", &e))?;

    let mut cmd = if run.opts.exec {
        let mut c = Command::new(&run.opts.command_argv[0]);
        c.args(&run.opts.command_argv[1..]);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&run.opts.command);
        c
    };
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(writer_err));
    if let Some(rows) = run.env_size.rows {
        cmd.env("LINES", rows.to_string());
    }
    if let Some(cols) = run.env_size.cols {
        cmd.env("COLUMNS", cols.to_string());
    }

    match cmd.spawn() {
        Ok(mut child) => {
            // The parent's copies of the pipe's write end must be closed, or
            // reading would never see end of output.
            drop(cmd);
            let wait = move || match child.wait() {
                Ok(status) => exit_code(status),
                Err(_) => 0x7f,
            };
            Ok((Box::new(reader), Box::new(wait)))
        }
        Err(e) if is_fork_failure(&e) => Err(Fatal::io(2, "unable to fork process", &e)),
        Err(e) => {
            let message = spawn_failure_output(run, &e);
            Ok((Box::new(Cursor::new(message)), Box::new(|| 0x7f)))
        }
    }
}

/// Runs the command on a pseudo-terminal sized like the output area, so
/// that programs which only color or format their output for terminals do
/// so. Pagers are disabled because nothing would ever answer them.
fn spawn_tty(run: &Run<'_>) -> Result<Spawned, Fatal> {
    let (rows, cols) = run.view;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: rows.clamp(1, usize::from(u16::MAX)) as u16,
            cols: cols.clamp(1, usize::from(u16::MAX)) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Fatal::new(2, format!("unable to create pseudo-terminal: {e}")))?;

    let mut cmd = if run.opts.exec {
        let mut c = CommandBuilder::new(&run.opts.command_argv[0]);
        c.args(&run.opts.command_argv[1..]);
        c
    } else {
        let mut c = CommandBuilder::new("sh");
        c.arg("-c");
        c.arg(&run.opts.command);
        c
    };
    if let Ok(dir) = std::env::current_dir() {
        cmd.cwd(dir);
    }
    cmd.env("LINES", rows.to_string());
    cmd.env("COLUMNS", cols.to_string());
    for pager in ["PAGER", "GIT_PAGER"] {
        cmd.env(pager, OsString::from("cat"));
    }

    let child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(e) => {
            let err = e
                .downcast_ref::<io::Error>()
                .map(|io| io::Error::from_raw_os_error(io.raw_os_error().unwrap_or(libc::ENOENT)))
                .unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOENT));
            let message = spawn_failure_output(run, &err);
            return Ok((Box::new(Cursor::new(message)), Box::new(|| 0x7f)));
        }
    };
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| Fatal::new(2, format!("unable to read pseudo-terminal: {e}")))?;
    let master = pair.master;
    let pid = child.process_id();
    let wait = move || {
        let code = match pid {
            Some(pid) => wait_pid(pid as libc::pid_t),
            None => 0x7f,
        };
        drop(child);
        drop(master);
        code
    };
    Ok((reader, Box::new(wait)))
}
