use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

use crate::args::Options;
use crate::command::{self, Fatal, Run};
use crate::locale;
use crate::output::{DiffCache, Flags};
use crate::signals::{Signals, wait_readable};
use crate::terminal::{self, Terminal, WinSize};
use crate::window::Window;

const HEADER_HEIGHT: usize = 2;
const ERREXIT_MESSAGE: &str = "command exit with a non-zero status, press a key to exit";
const ELLIPSIS: &str = "\u{2026}";

/// Display width of `s`, or `None` if it contains a character that is not
/// printable, mirroring procps' `mbswidth`.
fn text_width(s: &str) -> Option<usize> {
    s.chars().map(UnicodeWidthChar::width).sum()
}

/// A string for the header, or empty when it cannot be displayed in the
/// current locale.
fn header_text(bytes: &[u8], utf8: bool) -> String {
    let text = if utf8 || bytes.is_ascii() {
        std::str::from_utf8(bytes).ok()
    } else {
        None
    };
    match text {
        Some(s) if text_width(s).is_some() => s.to_string(),
        _ => String::new(),
    }
}

struct Screenshots {
    last: libc::time_t,
    last_nr: u8,
}

enum Waited {
    Ready(Vec<bool>),
    Signal,
}

struct App {
    opts: Options,
    flags: Flags,
    signals: Signals,
    term: Terminal,
    header: Window,
    main: Option<Window>,
    cache: Option<DiffCache>,
    stdin_size: WinSize,
    first_screen: bool,
    left_header: String,
    command: String,
    host_prefix: String,
    utf8: bool,
    shots: Screenshots,
}

enum Flow {
    Continue,
    Exit(i32),
}

pub fn run(opts: Options) -> Result<i32, Fatal> {
    let signals = Signals::install().map_err(|e| Fatal::io(1, "signal", &e))?;
    let stdin_size = terminal::stdin_size();
    let (height, width) = terminal::screen_size(stdin_size);
    let term = Terminal::start(height, width).map_err(|e| Fatal::io(1, "terminal", &e))?;
    let utf8 = locale::is_utf8();

    let flags = Flags {
        color: opts.color,
        differences: opts.differences,
        cumulative: opts.cumulative,
        track_changes: opts.tracks_changes(),
        follow: opts.follow,
        no_wrap: opts.no_wrap,
        utf8,
    };
    let mut main = Window::new(main_height(height, opts.no_title), width);
    main.scroll = opts.follow;
    let hostname = locale::hostname();

    let mut app = App {
        left_header: format!("Every {}s: ", locale::format_fixed(opts.interval, 1)),
        command: header_text(opts.command.as_bytes(), utf8),
        host_prefix: format!("{hostname}: "),
        opts,
        flags,
        signals,
        term,
        header: Window::new(HEADER_HEIGHT, width),
        main: Some(main),
        cache: Some(DiffCache::default()),
        stdin_size,
        first_screen: true,
        utf8,
        shots: Screenshots {
            last: 0,
            last_nr: 0,
        },
    };
    app.main_loop()
}

fn main_height(height: usize, no_title: bool) -> usize {
    if no_title {
        height.max(1)
    } else {
        height.saturating_sub(HEADER_HEIGHT).max(1)
    }
}

impl App {
    fn main_top(&self) -> usize {
        if self.opts.no_title { 0 } else { HEADER_HEIGHT }
    }

    fn main_window(&self) -> &Window {
        self.main
            .as_ref()
            .expect("main window is only absent while a command runs")
    }

    fn render_error(e: io::Error) -> Fatal {
        Fatal::io(1, "write", &e)
    }

    fn main_loop(&mut self) -> Result<i32, Fatal> {
        let interval = Duration::from_secs_f64(self.opts.interval);
        let mut last_tick = Instant::now();
        let mut cycle_count: i64 = 1;

        loop {
            if self.signals.take_resize() {
                self.resize()?;
            }

            self.clear_low_header();
            self.draw_header();
            let started = Instant::now();
            if self.opts.precise {
                last_tick = started;
            }
            let Some((exit_code, changed)) = self.run_command()? else {
                return Ok(0);
            };
            let now = Instant::now();
            if !self.opts.precise {
                last_tick = now;
            }
            self.draw_low_header(now - started, exit_code);
            if !self.opts.no_title {
                self.term
                    .refresh(0, &self.header)
                    .map_err(Self::render_error)?;
            }

            if exit_code != 0 {
                if self.opts.beep {
                    self.term.beep().map_err(Self::render_error)?;
                }
                if self.opts.errexit {
                    return self.errexit(exit_code);
                }
            }

            if !self.first_screen {
                if self.opts.chgexit && changed {
                    return Ok(0);
                }
                if let Some(max_cycles) = self.opts.equexit {
                    if changed {
                        cycle_count = 1;
                    } else {
                        if cycle_count == max_cycles {
                            return Ok(0);
                        }
                        cycle_count += 1;
                    }
                }
            }

            let top = self.main_top();
            let main = self.main.take().expect("main window present between runs");
            let refreshed = self.term.refresh(top, &main);
            self.main = Some(main);
            refreshed.map_err(Self::render_error)?;
            self.first_screen = false;

            if let Flow::Exit(code) = self.sleep(interval, last_tick)? {
                return Ok(code);
            }
        }
    }

    fn resize(&mut self) -> Result<(), Fatal> {
        self.stdin_size = terminal::stdin_size();
        let (height, width) = terminal::screen_size(self.stdin_size);
        self.term
            .resize(height, width)
            .map_err(Self::render_error)?;
        self.header.resize(HEADER_HEIGHT, width);
        if let Some(main) = self.main.as_mut() {
            main.resize(main_height(height, self.opts.no_title), width);
        }
        self.first_screen = true;
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), Fatal> {
        self.term.suspend().map_err(Self::render_error)?;
        self.signals.stop_self();
        self.term.resume().map_err(Self::render_error)
    }

    /// Waits for readability on `fds` plus the signal pipe. Termination
    /// requests and suspension are handled here; the caller sees other
    /// signals as `Waited::Signal`.
    fn wait(&mut self, fds: &[i32], timeout: Option<Duration>) -> Result<Option<Waited>, Fatal> {
        let mut all = vec![self.signals.fd()];
        all.extend_from_slice(fds);
        let ready = wait_readable(&all, timeout).map_err(|e| Fatal::io(1, "poll", &e))?;
        let interrupted = ready.is_none();
        let ready = ready.unwrap_or_else(|| vec![false; all.len()]);
        if ready[0] {
            self.signals.drain();
        }
        if self.signals.terminate_requested() {
            return Ok(None);
        }
        if self.signals.take_suspend() {
            self.suspend()?;
            return Ok(Some(Waited::Signal));
        }
        if interrupted || ready[0] {
            return Ok(Some(Waited::Signal));
        }
        Ok(Some(Waited::Ready(ready[1..].to_vec())))
    }

    /// Runs the command once. Returns `None` if watch was asked to terminate
    /// meanwhile.
    fn run_command(&mut self) -> Result<Option<(u8, bool)>, Fatal> {
        let window = self.main.take().expect("main window present between runs");
        let cache = self.cache.take().expect("diff cache present between runs");
        let view = (window.height(), window.width());
        let job = command::start(
            Run {
                opts: &self.opts,
                flags: self.flags,
                first_screen: self.first_screen,
                env_size: self.stdin_size,
                view,
            },
            window,
            cache,
        )?;

        loop {
            match self.wait(&[job.fd()], None)? {
                None => return Ok(None),
                Some(Waited::Ready(ready)) if ready[0] => break,
                Some(_) => {}
            }
        }
        let finished = job.join();
        self.main = Some(finished.window);
        self.cache = Some(finished.cache);
        Ok(Some((finished.exit_code, finished.changed)))
    }

    fn clear_low_header(&mut self) {
        if self.opts.no_title {
            return;
        }
        self.header.mv(1, 0);
        self.header.clrtoeol();
    }

    /// Lays out the first header line: interval and command on the left,
    /// hostname and time on the right. The command is shortened with an
    /// ellipsis to keep at least one blank before the right part; when even
    /// the left prefix does not fit, only the right part is shown, and when
    /// that does not fit either, nothing is.
    fn draw_header(&mut self) {
        if self.opts.no_title {
            return;
        }
        let width = self.header.width();
        let right = header_text(
            format!("{}{}", self.host_prefix, locale::strftime_now("%c")).as_bytes(),
            self.utf8,
        );
        let right_width = text_width(&right).unwrap_or(0);
        let left_width = text_width(&self.left_header).unwrap_or(0);
        let command_width = text_width(&self.command).unwrap_or(0);

        if self.first_screen {
            self.header.mv(0, 0);
            self.header.clrtoeol();
        }
        if width < right_width {
            return;
        }
        self.header.mv(0, (width - right_width) as isize);
        self.header.add_str(&right);

        let avail = width as isize - left_width as isize - right_width as isize;
        if avail < 0 {
            return;
        }
        self.header.mv(0, 0);
        self.header.add_str(&self.left_header.clone());
        let ellipsis_width = 1;
        if avail > command_width as isize {
            self.header.add_str(&self.command.clone());
        } else if avail > ellipsis_width {
            let chars: Vec<char> = self.command.chars().collect();
            let limit = avail - ellipsis_width - 1;
            let mut len = chars.len();
            while len > 0 {
                len -= 1;
                let w: usize = chars[..len]
                    .iter()
                    .map(|&c| UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum();
                if w as isize <= limit {
                    break;
                }
            }
            let shortened: String = chars[..len].iter().collect();
            self.header.add_str(&shortened);
            self.header.add_str(ELLIPSIS);
        }
    }

    fn draw_low_header(&mut self, span: Duration, exit_code: u8) {
        if self.opts.no_title {
            return;
        }
        let micros = span.as_micros();
        let text = if micros > 24 * 60 * 60 * 1_000_000 {
            format!("in >1 day ({exit_code})")
        } else if micros < 1000 {
            format!("in <{}s ({exit_code})", locale::format_fixed(0.001, 3))
        } else {
            format!(
                "in {}s ({exit_code})",
                locale::format_fixed(micros as f64 / 1_000_000.0, 3)
            )
        };
        self.header.mv(1, 0);
        self.header.clrtoeol();
        let width = self.header.width() as isize;
        let skip = width - text_width(&text).unwrap_or(0) as isize;
        if skip >= 0 {
            self.header.mv(1, skip);
            self.header.add_str(&text);
        }
    }

    /// Upstream writes its message into the output window but then refreshes
    /// the untouched base screen instead, so the terminal just goes blank
    /// until a key is pressed.
    fn errexit(&mut self, exit_code: u8) -> Result<i32, Fatal> {
        if let Some(main) = self.main.as_mut() {
            let bottom = main.height() as isize - 1;
            main.mv(bottom, 0);
            main.add_str(ERREXIT_MESSAGE);
        }
        discard_pending_input();
        self.term.refresh_blank().map_err(Self::render_error)?;
        loop {
            match self.wait(&[libc::STDIN_FILENO], None)? {
                None => return Ok(0),
                Some(Waited::Ready(ready)) if ready[0] => {
                    let _ = read_byte();
                    return Ok(i32::from(exit_code));
                }
                Some(_) => {}
            }
        }
    }

    /// Waits until the next run is due while handling keys. All pending
    /// input is processed first, including keys pressed while the command
    /// ran: `q` quits, space runs the command right away, and `s` saves a
    /// screenshot (at most once per wait).
    fn sleep(&mut self, interval: Duration, last_tick: Instant) -> Result<Flow, Fatal> {
        let mut dont_sleep = false;
        let mut screenshot_taken = false;
        let mut quit = false;
        loop {
            dont_sleep |= self.signals.resize_pending() && !self.opts.no_rerun;
            let elapsed = last_tick.elapsed();
            let timeout = if dont_sleep || elapsed >= interval {
                Duration::ZERO
            } else {
                interval - elapsed
            };
            let ready = match self.wait(&[libc::STDIN_FILENO], Some(timeout))? {
                None => return Ok(Flow::Exit(0)),
                Some(Waited::Signal) => continue,
                Some(Waited::Ready(ready)) => ready,
            };
            if !ready[0] {
                break;
            }
            match read_byte() {
                Ok(Some(b'q')) => {
                    dont_sleep = true;
                    quit = true;
                }
                Ok(Some(b' ')) => dont_sleep = true,
                Ok(Some(b's')) => {
                    if !screenshot_taken {
                        self.screenshot()?;
                        screenshot_taken = true;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    // End of input sets no errno, so upstream reports errno 0.
                    return Err(Fatal::new(1, format!("getchar(): {}", locale::strerror(0))));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Fatal::io(1, "getchar()", &e)),
            }
        }
        Ok(if quit { Flow::Exit(0) } else { Flow::Continue })
    }

    /// Saves the output area's rows above the cursor, padded to the full
    /// width, to `watch_<timestamp>[-NNN]`. Screenshots within the same
    /// second get increasing numbers starting at `-000`.
    fn screenshot(&mut self) -> Result<(), Fatal> {
        let mut path = self.opts.shotsdir.as_bytes().to_vec();
        if !path.is_empty() && !path.ends_with(b"/") {
            path.push(b'/');
        }
        path.extend_from_slice(b"watch_");
        path.extend_from_slice(locale::strftime_now("%Y%m%d-%H%M%S").as_bytes());
        let now = locale::unix_time();
        if now == self.shots.last {
            path.extend_from_slice(format!("-{:03}", self.shots.last_nr).as_bytes());
            self.shots.last_nr = self.shots.last_nr.saturating_add(1);
        } else {
            self.shots.last = now;
            self.shots.last_nr = 0;
        }
        let shown = String::from_utf8_lossy(&path).into_owned();

        let c_path = CString::new(path)
            .map_err(|_| Fatal::new(1, format!("open({shown}): Invalid argument")))?;
        // SAFETY: `c_path` is a valid NUL-terminated path.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o644 as libc::c_uint,
            )
        };
        if fd == -1 {
            return Err(Fatal::io(
                1,
                &format!("open({shown})"),
                &io::Error::last_os_error(),
            ));
        }

        let main = self.main_window();
        let mut contents = Vec::new();
        for y in 0..main.cursor().0 {
            contents.extend_from_slice(main.row_text(y).as_bytes());
            contents.push(b'\n');
        }
        let mut written = 0;
        while written < contents.len() {
            // SAFETY: the pointer and length describe the unwritten part of `contents`.
            let n = unsafe {
                libc::write(
                    fd,
                    contents[written..].as_ptr().cast(),
                    contents.len() - written,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // SAFETY: fd was opened above and is closed exactly once.
                unsafe { libc::close(fd) };
                return Err(Fatal::io(1, &format!("write({shown})"), &err));
            }
            written += n as usize;
        }
        // SAFETY: fd was opened above and is closed exactly once.
        if unsafe { libc::close(fd) } == -1 {
            return Err(Fatal::io(
                1,
                &format!("close({shown})"),
                &io::Error::last_os_error(),
            ));
        }
        if self.signals.resize_pending() {
            return Err(Fatal::new(
                1,
                format!("screenshot({shown}): {}", locale::strerror(libc::ECANCELED)),
            ));
        }
        Ok(())
    }
}

/// Reads one byte from standard input; `Ok(None)` at end of input.
fn read_byte() -> io::Result<Option<u8>> {
    let mut b = 0u8;
    // SAFETY: reads at most one byte into `b`.
    let n = unsafe { libc::read(libc::STDIN_FILENO, (&mut b as *mut u8).cast(), 1) };
    match n {
        1 => Ok(Some(b)),
        0 => Ok(None),
        _ => Err(io::Error::last_os_error()),
    }
}

fn discard_pending_input() {
    // SAFETY: only toggles O_NONBLOCK on standard input and restores it.
    unsafe {
        let flags = libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL);
        if flags < 0 || libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
        {
            return;
        }
        while matches!(read_byte(), Ok(Some(_))) {}
        libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_text_rejects_unprintable_commands() {
        assert_eq!(header_text(b"ls -l", true), "ls -l");
        assert_eq!(header_text(b"echo a\tb", true), "");
        assert_eq!(header_text("caf\u{e9}".as_bytes(), false), "");
        assert_eq!(header_text("caf\u{e9}".as_bytes(), true), "caf\u{e9}");
    }
}
