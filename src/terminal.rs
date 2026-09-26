use std::env;
use std::io::{self, Write};

use crate::ansi::{Attr, sgr_sequence};
use crate::window::{Cell, Window};

const HEIGHT_FALLBACK: usize = 24;
const WIDTH_FALLBACK: usize = 80;

/// Dimensions reported by `TIOCGWINSZ` on standard input; each is `None`
/// when unavailable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WinSize {
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}

fn ioctl_winsize(fd: i32) -> WinSize {
    // SAFETY: TIOCGWINSZ only writes a `winsize` struct through the pointer.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) != 0 {
            return WinSize::default();
        }
        WinSize {
            rows: (ws.ws_row > 0).then_some(ws.ws_row),
            cols: (ws.ws_col > 0).then_some(ws.ws_col),
        }
    }
}

/// The size of the terminal on standard input. Upstream exports these as
/// `LINES` and `COLUMNS` to the command.
pub fn stdin_size() -> WinSize {
    ioctl_winsize(libc::STDIN_FILENO)
}

fn env_dimension(name: &str) -> Option<usize> {
    env::var(name)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|&v| v > 0)
}

/// Screen size as ncurses determines it once upstream has exported the
/// size of standard input: `LINES`/`COLUMNS` take precedence over the
/// output terminal's size, with 80x24 as the last resort.
pub fn screen_size(stdin: WinSize) -> (usize, usize) {
    let out = ioctl_winsize(libc::STDOUT_FILENO);
    let rows = stdin
        .rows
        .map(usize::from)
        .or_else(|| env_dimension("LINES"))
        .or(out.rows.map(usize::from))
        .unwrap_or(HEIGHT_FALLBACK);
    let cols = stdin
        .cols
        .map(usize::from)
        .or_else(|| env_dimension("COLUMNS"))
        .or(out.cols.map(usize::from))
        .unwrap_or(WIDTH_FALLBACK);
    (rows, cols)
}

/// The physical screen: terminal modes, and a copy of what is displayed so
/// that refreshes only send changed cells.
pub struct Terminal {
    tty_fd: i32,
    saved: Option<libc::termios>,
    out: io::Stdout,
    height: usize,
    width: usize,
    shown: Vec<Vec<Cell>>,
    /// Set until the first refresh, which clears the screen like ncurses'
    /// first `doupdate`.
    pristine: bool,
    active: bool,
}

impl Terminal {
    pub fn start(height: usize, width: usize) -> io::Result<Self> {
        // SAFETY: isatty only inspects the descriptor.
        let tty_fd = if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1 {
            libc::STDOUT_FILENO
        } else {
            libc::STDIN_FILENO
        };
        let mut term = Terminal {
            tty_fd,
            saved: None,
            out: io::stdout(),
            height,
            width,
            shown: vec![vec![Cell::blank(); width]; height],
            pristine: true,
            active: false,
        };
        term.enter()?;
        Ok(term)
    }

    /// Switches to cbreak, no-echo mode with CR/NL input translation off.
    /// Unlike raw mode, the terminal still turns Ctrl+C into SIGINT for the
    /// whole foreground process group, so it reaches the command too.
    fn enter(&mut self) -> io::Result<()> {
        // SAFETY: tcgetattr/tcsetattr only access the termios struct passed in.
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(self.tty_fd, &mut tio) == 0 {
                self.saved = Some(tio);
                tio.c_lflag &= !(libc::ICANON | libc::ECHO);
                tio.c_lflag |= libc::ISIG;
                tio.c_iflag &= !libc::ICRNL;
                tio.c_cc[libc::VMIN] = 1;
                tio.c_cc[libc::VTIME] = 0;
                libc::tcsetattr(self.tty_fd, libc::TCSADRAIN, &tio);
            }
        }
        self.active = true;
        write!(self.out, "\x1b[?1049h\x1b[?25l")?;
        self.out.flush()
    }

    fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let result =
            write!(self.out, "\x1b[0m\x1b[?25h\x1b[?1049l").and_then(|()| self.out.flush());
        if let Some(tio) = self.saved {
            // SAFETY: restores the attributes read in `enter`.
            unsafe {
                libc::tcsetattr(self.tty_fd, libc::TCSADRAIN, &tio);
            }
        }
        result
    }

    /// Leaves the screen before the process stops for job control.
    pub fn suspend(&mut self) -> io::Result<()> {
        self.leave()
    }

    /// Restores the screen after the process is continued, redrawing
    /// everything that was shown.
    pub fn resume(&mut self) -> io::Result<()> {
        self.enter()?;
        let shown = std::mem::take(&mut self.shown);
        self.shown = vec![vec![Cell::blank(); self.width]; self.height];
        self.clear_physical()?;
        for (y, row) in shown.iter().enumerate() {
            self.draw_row(y, row)?;
        }
        self.out.flush()
    }

    fn clear_physical(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[0m\x1b[H\x1b[2J")?;
        for row in &mut self.shown {
            row.fill(Cell::blank());
        }
        Ok(())
    }

    /// Adopts a new terminal size and clears the screen, as upstream's
    /// `endwin(); refresh();` on resize does.
    pub fn resize(&mut self, height: usize, width: usize) -> io::Result<()> {
        self.height = height;
        self.width = width;
        self.shown = vec![vec![Cell::blank(); width]; height];
        self.pristine = false;
        self.clear_physical()?;
        self.out.flush()
    }

    /// Refreshes the never-drawn base screen upstream calls `stdscr`, which
    /// blanks the whole terminal.
    pub fn refresh_blank(&mut self) -> io::Result<()> {
        self.pristine = false;
        self.clear_physical()?;
        self.out.flush()
    }

    /// Displays `win` with its top-left corner on terminal row `top`.
    pub fn refresh(&mut self, top: usize, win: &Window) -> io::Result<()> {
        if self.pristine {
            self.pristine = false;
            self.clear_physical()?;
        }
        for y in 0..win.height() {
            let sy = top + y;
            if sy >= self.height {
                break;
            }
            let cols = win.width().min(self.width);
            let row: Vec<Cell> = (0..cols).map(|x| win.cell(y, x).clone()).collect();
            self.draw_row(sy, &row)?;
        }
        self.out.flush()
    }

    fn draw_row(&mut self, y: usize, row: &[Cell]) -> io::Result<()> {
        let mut cursor: Option<usize> = None;
        let mut current: Option<(Attr, bool)> = None;
        let mut x = 0;
        while x < row.len() {
            let cell = &row[x];
            if cell.cont {
                // Right halves are drawn together with their left half.
                self.shown[y][x] = cell.clone();
                x += 1;
                continue;
            }
            let wide = x + 1 < row.len() && row[x + 1].cont;
            let span = if wide { 2 } else { 1 };
            let unchanged =
                self.shown[y][x] == *cell && (!wide || self.shown[y][x + 1] == row[x + 1]);
            if unchanged {
                x += span;
                continue;
            }
            if cursor != Some(x) {
                write!(self.out, "\x1b[{};{}H", y + 1, x + 1)?;
            }
            let style = (cell.attr, cell.standout);
            if current != Some(style) {
                write!(self.out, "{}", sgr_sequence(&cell.attr, cell.standout))?;
                current = Some(style);
            }
            let mut text = String::new();
            text.extend(cell.chars());
            write!(self.out, "{text}")?;
            self.shown[y][x] = cell.clone();
            if wide {
                self.shown[y][x + 1] = row[x + 1].clone();
            }
            x += span;
            cursor = (x < self.width).then_some(x);
        }
        if current.is_some() {
            write!(self.out, "\x1b[0m")?;
        }
        Ok(())
    }

    pub fn beep(&mut self) -> io::Result<()> {
        write!(self.out, "\x07")?;
        self.out.flush()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}
