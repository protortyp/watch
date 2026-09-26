use std::collections::VecDeque;
use std::env;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::queue;
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use hostname::get as get_hostname;
use os_pipe::pipe;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use unicode_width::UnicodeWidthChar;

const HEADER_HEIGHT: usize = 2;
const HEIGHT_FALLBACK: usize = 24;
const WIDTH_FALLBACK: usize = 80;
const TAB_WIDTH: usize = 8;
const MIN_INTERVAL: f64 = 0.1;
const MAX_INTERVAL: f64 = 60.0 * 60.0 * 24.0 * 31.0;
const FOLLOW_HISTORY_LIMIT: usize = 50_000;

#[derive(Debug)]
struct AppError {
    code: i32,
    message: String,
}

impl AppError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct Options {
    beep_on_nonzero: bool,
    color: bool,
    differences: bool,
    differences_permanent: bool,
    errexit: bool,
    follow: bool,
    chgexit: bool,
    equexit_cycles: Option<u64>,
    interval_secs: f64,
    precise: bool,
    no_rerun: bool,
    shotsdir: PathBuf,
    no_title: bool,
    no_wrap: bool,
    exec_mode: bool,
    command_argv: Vec<String>,
    command_display: String,
}

#[derive(Debug)]
enum ParsedArgs {
    Run(Options),
    Help,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TextStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    reverse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: TextStyle,
    hidden: bool,
}

#[derive(Debug, Clone)]
struct ParsedOutput {
    rows: Vec<Vec<Cell>>,
    saw_bell: bool,
}

#[derive(Debug)]
struct CommandResult {
    output: Vec<u8>,
    exit_code: u8,
    interrupted: bool,
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(Color::Reset),
            SetBackgroundColor(Color::Reset),
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Debug)]
struct AppState {
    width: usize,
    height: usize,
    main_height: usize,
    hostname: String,
    first_screen: bool,
    previous_chars: Option<Vec<Vec<char>>>,
    cumulative_diff: Vec<Vec<bool>>,
    follow_history: VecDeque<Vec<Cell>>,
    last_rows: Vec<Vec<Cell>>,
    cycle_count: u64,
    queued_screenshot: bool,
}

impl AppState {
    fn new(opts: &Options) -> io::Result<Self> {
        let (width, height) = terminal_dimensions(opts.no_title);
        let main_height = main_window_height(height, opts.no_title);
        let hostname = get_hostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "localhost".to_string());

        Ok(Self {
            width,
            height,
            main_height,
            hostname,
            first_screen: true,
            previous_chars: None,
            cumulative_diff: vec![vec![false; width]; main_height],
            follow_history: VecDeque::new(),
            last_rows: vec![blank_row(width); main_height],
            cycle_count: 1,
            queued_screenshot: false,
        })
    }

    fn resize(&mut self, opts: &Options) -> io::Result<()> {
        let (width, height) = terminal_dimensions(opts.no_title);
        self.width = width;
        self.height = height;
        self.main_height = main_window_height(self.height, opts.no_title);
        self.first_screen = true;
        self.previous_chars = None;
        self.cumulative_diff = vec![vec![false; self.width]; self.main_height];
        self.last_rows = vec![blank_row(self.width); self.main_height];
        if opts.follow {
            self.follow_history.clear();
        }
        Ok(())
    }
}

fn parse_size_override(name: &str) -> Option<usize> {
    let raw = env::var(name).ok()?;
    let value = raw.trim().parse::<usize>().ok()?;
    if value == 0 { None } else { Some(value) }
}

fn terminal_dimensions(no_title: bool) -> (usize, usize) {
    let (mut width, mut height) = match terminal::size() {
        Ok((w, h)) => (w as usize, h as usize),
        Err(_) => (0, 0),
    };

    if let Some(cols) = parse_size_override("COLUMNS") {
        width = cols;
    }
    if let Some(lines) = parse_size_override("LINES") {
        height = lines;
    }

    if width == 0 {
        width = WIDTH_FALLBACK;
    }

    let min_height = if no_title { 1 } else { HEADER_HEIGHT + 1 };
    if height < min_height {
        height = HEIGHT_FALLBACK.max(min_height);
    }

    (width, height)
}

fn main_window_height(height: usize, no_title: bool) -> usize {
    if no_title {
        height.max(1)
    } else {
        height.saturating_sub(HEADER_HEIGHT).max(1)
    }
}

fn blank_row(width: usize) -> Vec<Cell> {
    vec![
        Cell {
            ch: ' ',
            style: TextStyle::default(),
            hidden: false,
        };
        width
    ]
}

fn parse_interval(raw: &str) -> Result<f64, String> {
    let normalized = raw.trim().replace(',', ".");
    let parsed = normalized
        .parse::<f64>()
        .map_err(|_| format!("failed to parse interval: {raw}"))?;
    if !parsed.is_finite() {
        return Err(format!("failed to parse interval: {raw}"));
    }
    Ok(parsed.clamp(MIN_INTERVAL, MAX_INTERVAL))
}

fn usage(program: &str) -> String {
    format!(
        "Usage: {program} [options] command\n\
         Options:\n\
           -b, --beep                  beep if command has non-zero exit\n\
           -c, --color                 interpret ANSI color/style sequences\n\
           -C, --no-color              do not interpret ANSI color/style sequences\n\
           -d, --differences[=1]       highlight changes between updates\n\
           -e, --errexit               freeze on command error and exit after keypress\n\
           -f, --follow                follow output without clearing screen\n\
           -g, --chgexit               exit when visible output changes\n\
           -q, --equexit <cycles>      exit after unchanged output for cycles\n\
           -n, --interval <secs>       wait interval between updates\n\
           -p, --precise               schedule by run start times\n\
           -r, --no-rerun              do not rerun immediately on resize\n\
           -s, --shotsdir <dir>        directory for screenshots\n\
           -t, --no-title              disable header\n\
           -w, --no-wrap               truncate long lines\n\
           -x, --exec                  execute command directly (no shell)\n\
           -h, --help                  show help\n\
           -v, --version               show version\n\n\
         Keys: q quit, space rerun now, s screenshot"
    )
}

fn parse_args() -> Result<ParsedArgs, AppError> {
    let program = env::args().next().unwrap_or_else(|| "watch".to_string());

    let mut interval_secs = match env::var("WATCH_INTERVAL") {
        Ok(raw) => parse_interval(&raw).map_err(|e| AppError::new(1, e))?,
        Err(_) => 2.0,
    };

    let mut opts = Options {
        beep_on_nonzero: false,
        color: false,
        differences: false,
        differences_permanent: false,
        errexit: false,
        follow: false,
        chgexit: false,
        equexit_cycles: None,
        interval_secs,
        precise: false,
        no_rerun: false,
        shotsdir: PathBuf::from("."),
        no_title: false,
        no_wrap: false,
        exec_mode: false,
        command_argv: Vec::new(),
        command_display: String::new(),
    };

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];

        if arg == "--" {
            opts.command_argv.extend(args.iter().skip(i + 1).cloned());
            break;
        }

        if !arg.starts_with('-') || arg == "-" {
            opts.command_argv.extend(args.iter().skip(i).cloned());
            break;
        }

        if let Some(mut long) = arg.strip_prefix("--") {
            let mut value: Option<String> = None;
            if let Some((name, v)) = long.split_once('=') {
                long = name;
                value = Some(v.to_string());
            }

            let mut next_value = |name: &str| -> Result<String, AppError> {
                if let Some(v) = value.take() {
                    return Ok(v);
                }
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| AppError::new(1, format!("missing value for --{name}")))
            };

            match long {
                "help" => return Ok(ParsedArgs::Help),
                "version" => return Ok(ParsedArgs::Version),
                "beep" => opts.beep_on_nonzero = true,
                "color" => opts.color = true,
                "no-color" => opts.color = false,
                "differences" => {
                    opts.differences = true;
                    if value.is_some() {
                        opts.differences_permanent = true;
                    }
                }
                "errexit" => opts.errexit = true,
                "follow" => opts.follow = true,
                "chgexit" => opts.chgexit = true,
                "equexit" => {
                    let raw = next_value("equexit")?;
                    let cycles = raw
                        .parse::<u64>()
                        .map_err(|_| AppError::new(1, "failed to parse argument for --equexit"))?;
                    opts.equexit_cycles = Some(cycles.max(1));
                }
                "interval" => {
                    let raw = next_value("interval")?;
                    interval_secs = parse_interval(&raw).map_err(|e| AppError::new(1, e))?;
                    opts.interval_secs = interval_secs;
                }
                "precise" => opts.precise = true,
                "no-rerun" => opts.no_rerun = true,
                "shotsdir" => {
                    let raw = next_value("shotsdir")?;
                    opts.shotsdir = PathBuf::from(raw);
                }
                "no-title" => opts.no_title = true,
                "no-wrap" => opts.no_wrap = true,
                "exec" => opts.exec_mode = true,
                _ => {
                    return Err(AppError::new(
                        1,
                        format!("unknown option: --{long}\n{}", usage(&program)),
                    ));
                }
            }
        } else {
            let mut chars = arg[1..].chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    'h' => return Ok(ParsedArgs::Help),
                    'v' => return Ok(ParsedArgs::Version),
                    'b' => opts.beep_on_nonzero = true,
                    'c' => opts.color = true,
                    'C' => opts.color = false,
                    'e' => opts.errexit = true,
                    'f' => opts.follow = true,
                    'g' => opts.chgexit = true,
                    'p' => opts.precise = true,
                    'r' => opts.no_rerun = true,
                    't' => opts.no_title = true,
                    'w' => opts.no_wrap = true,
                    'x' => opts.exec_mode = true,
                    'd' => {
                        opts.differences = true;
                        if chars.peek().is_some() {
                            opts.differences_permanent = true;
                            break;
                        }
                    }
                    'n' => {
                        let rest: String = chars.collect();
                        let raw = if rest.is_empty() {
                            i += 1;
                            args.get(i).cloned().ok_or_else(|| {
                                AppError::new(1, "missing value for -n/--interval")
                            })?
                        } else {
                            rest
                        };
                        interval_secs = parse_interval(&raw).map_err(|e| AppError::new(1, e))?;
                        opts.interval_secs = interval_secs;
                        break;
                    }
                    'q' => {
                        let rest: String = chars.collect();
                        let raw = if rest.is_empty() {
                            i += 1;
                            args.get(i)
                                .cloned()
                                .ok_or_else(|| AppError::new(1, "missing value for -q/--equexit"))?
                        } else {
                            rest
                        };
                        let cycles = raw
                            .parse::<u64>()
                            .map_err(|_| AppError::new(1, "failed to parse argument for -q"))?;
                        opts.equexit_cycles = Some(cycles.max(1));
                        break;
                    }
                    's' => {
                        let rest: String = chars.collect();
                        let raw = if rest.is_empty() {
                            i += 1;
                            args.get(i).cloned().ok_or_else(|| {
                                AppError::new(1, "missing value for -s/--shotsdir")
                            })?
                        } else {
                            rest
                        };
                        opts.shotsdir = PathBuf::from(raw);
                        break;
                    }
                    _ => {
                        return Err(AppError::new(
                            1,
                            format!("unknown option: -{ch}\n{}", usage(&program)),
                        ));
                    }
                }
            }
        }

        i += 1;
    }

    if opts.command_argv.is_empty() {
        return Err(AppError::new(1, usage(&program)));
    }

    opts.command_display = opts.command_argv.join(" ");

    if opts.follow && (opts.differences || opts.chgexit || opts.equexit_cycles.is_some()) {
        return Err(AppError::new(
            1,
            "--follow conflicts with --differences, --chgexit and --equexit",
        ));
    }

    Ok(ParsedArgs::Run(opts))
}

fn ansi_basic_color(n: i32, bright: bool) -> Option<Color> {
    match (n, bright) {
        (0, false) => Some(Color::Black),
        (1, false) => Some(Color::DarkRed),
        (2, false) => Some(Color::DarkGreen),
        (3, false) => Some(Color::DarkYellow),
        (4, false) => Some(Color::DarkBlue),
        (5, false) => Some(Color::DarkMagenta),
        (6, false) => Some(Color::DarkCyan),
        (7, false) => Some(Color::Grey),
        (0, true) => Some(Color::DarkGrey),
        (1, true) => Some(Color::Red),
        (2, true) => Some(Color::Green),
        (3, true) => Some(Color::Yellow),
        (4, true) => Some(Color::Blue),
        (5, true) => Some(Color::Magenta),
        (6, true) => Some(Color::Cyan),
        (7, true) => Some(Color::White),
        _ => None,
    }
}

fn apply_sgr(style: &mut TextStyle, sgr: &str) {
    let parts: Vec<i32> = if sgr.trim().is_empty() {
        vec![0]
    } else {
        sgr.split(';')
            .filter_map(|s| s.parse::<i32>().ok())
            .collect()
    };

    let mut i = 0usize;
    while i < parts.len() {
        match parts[i] {
            0 => *style = TextStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            5 => style.blink = true,
            7 => style.reverse = true,
            21 | 22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            25 => style.blink = false,
            27 => style.reverse = false,
            30..=37 => {
                style.fg = ansi_basic_color(parts[i] - 30, false);
            }
            39 => style.fg = None,
            40..=47 => {
                style.bg = ansi_basic_color(parts[i] - 40, false);
            }
            49 => style.bg = None,
            90..=97 => {
                style.fg = ansi_basic_color(parts[i] - 90, true);
            }
            100..=107 => {
                style.bg = ansi_basic_color(parts[i] - 100, true);
            }
            38 | 48 => {
                let target_fg = parts[i] == 38;
                if i + 2 < parts.len() && parts[i + 1] == 5 {
                    let n = parts[i + 2].clamp(0, 255) as u8;
                    if target_fg {
                        style.fg = Some(Color::AnsiValue(n));
                    } else {
                        style.bg = Some(Color::AnsiValue(n));
                    }
                    i += 2;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

fn parse_output(
    output: &[u8],
    width: usize,
    no_wrap: bool,
    color: bool,
    max_rows: Option<usize>,
) -> ParsedOutput {
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut saw_bell = false;

    if width == 0 {
        return ParsedOutput { rows, saw_bell };
    }

    let mut style = TextStyle::default();
    rows.push(blank_row(width));
    let mut row = 0usize;
    let mut col = 0usize;
    let mut truncate_line = false;

    let text = String::from_utf8_lossy(output);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    let newline = |rows: &mut Vec<Vec<Cell>>, row: &mut usize, col: &mut usize| -> bool {
        if let Some(max) = max_rows
            && *row + 1 >= max
        {
            return false;
        }
        rows.push(blank_row(width));
        *row += 1;
        *col = 0;
        true
    };

    while i < chars.len() {
        let ch = chars[i];

        if ch == '\x1b' {
            if i + 1 < chars.len() && chars[i + 1] == '[' {
                let mut j = i + 2;
                let mut buf = String::new();
                while j < chars.len() {
                    let c = chars[j];
                    if c == 'm' {
                        if color {
                            apply_sgr(&mut style, &buf);
                        }
                        i = j + 1;
                        break;
                    }
                    if !(c.is_ascii_digit() || c == ';') {
                        i = j + 1;
                        break;
                    }
                    buf.push(c);
                    j += 1;
                }
                if j >= chars.len() {
                    break;
                }
                continue;
            }
            i += 1;
            continue;
        }

        if ch == '\x07' {
            saw_bell = true;
            i += 1;
            continue;
        }

        if ch == '\n' {
            truncate_line = false;
            if !newline(&mut rows, &mut row, &mut col) {
                break;
            }
            i += 1;
            continue;
        }

        if truncate_line {
            i += 1;
            continue;
        }

        if ch == '\t' {
            let mut next = col + 1;
            while !next.is_multiple_of(TAB_WIDTH) && next < width {
                next += 1;
            }
            while col < next && col < width {
                rows[row][col] = Cell {
                    ch: ' ',
                    style,
                    hidden: false,
                };
                col += 1;
            }
            i += 1;
            continue;
        }

        if ch.is_control() {
            i += 1;
            continue;
        }

        let mut w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w == 0 {
            w = 1;
        }

        if w > width.saturating_sub(col) {
            if no_wrap {
                truncate_line = true;
                i += 1;
                continue;
            }
            if !newline(&mut rows, &mut row, &mut col) {
                break;
            }
        }

        if col >= width {
            if !no_wrap {
                if !newline(&mut rows, &mut row, &mut col) {
                    break;
                }
            } else {
                truncate_line = true;
            }
            i += 1;
            continue;
        }

        rows[row][col] = Cell {
            ch,
            style,
            hidden: false,
        };

        if w > 1 {
            for k in 1..w {
                let pos = col + k;
                if pos < width {
                    rows[row][pos] = Cell {
                        ch: ' ',
                        style,
                        hidden: true,
                    };
                }
            }
        }

        col += w;
        i += 1;
    }

    ParsedOutput { rows, saw_bell }
}

fn rows_to_chars(rows: &[Vec<Cell>], width: usize, height: usize) -> Vec<Vec<char>> {
    let mut out = vec![vec![' '; width]; height];
    for (y, out_row) in out.iter_mut().enumerate().take(height) {
        if let Some(row) = rows.get(y) {
            for (x, out_cell) in out_row.iter_mut().enumerate().take(width) {
                if let Some(cell) = row.get(x) {
                    *out_cell = if cell.hidden { '\0' } else { cell.ch };
                }
            }
        }
    }
    out
}

fn diff_chars(
    prev: &[Vec<char>],
    curr: &[Vec<char>],
    width: usize,
    height: usize,
) -> (bool, Vec<Vec<bool>>) {
    let mut changed = false;
    let mut map = vec![vec![false; width]; height];
    for (y, map_row) in map.iter_mut().enumerate().take(height) {
        for (x, map_cell) in map_row.iter_mut().enumerate().take(width) {
            let p = prev.get(y).and_then(|r| r.get(x)).copied().unwrap_or(' ');
            let c = curr.get(y).and_then(|r| r.get(x)).copied().unwrap_or(' ');
            if p != c {
                *map_cell = true;
                changed = true;
            }
        }
    }
    (changed, map)
}

fn apply_style(stdout: &mut io::Stdout, style: TextStyle, highlight: bool) -> io::Result<()> {
    queue!(
        stdout,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(style.fg.unwrap_or(Color::Reset)),
        SetBackgroundColor(style.bg.unwrap_or(Color::Reset)),
    )?;

    if style.bold {
        queue!(stdout, SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        queue!(stdout, SetAttribute(Attribute::Dim))?;
    }
    if style.italic {
        queue!(stdout, SetAttribute(Attribute::Italic))?;
    }
    if style.underline {
        queue!(stdout, SetAttribute(Attribute::Underlined))?;
    }
    if style.blink {
        queue!(stdout, SetAttribute(Attribute::SlowBlink))?;
    }
    if style.reverse || highlight {
        queue!(stdout, SetAttribute(Attribute::Reverse))?;
    }
    Ok(())
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars <= 3 {
        return "".to_string();
    }
    let mut out = String::new();
    for c in s.chars().take(max_chars - 3) {
        out.push(c);
    }
    out.push_str("...");
    out
}

fn right_aligned(width: usize, s: &str) -> String {
    let len = s.chars().count();
    if len >= width {
        truncate_with_ellipsis(s, width)
    } else {
        let mut out = String::with_capacity(width);
        for _ in 0..(width - len) {
            out.push(' ');
        }
        out.push_str(s);
        out
    }
}

fn render_header(
    stdout: &mut io::Stdout,
    opts: &Options,
    state: &AppState,
    elapsed: Duration,
    exit_code: u8,
) -> io::Result<()> {
    if opts.no_title || state.width == 0 {
        return Ok(());
    }

    let now = Local::now();
    let right = format!("{}: {}", state.hostname, now.format("%c"));
    let left_prefix = format!("Every {:.1}s: ", opts.interval_secs);

    let room_for_left = if right.chars().count() + 1 >= state.width {
        0
    } else {
        state.width - right.chars().count() - 1
    };

    let left = if room_for_left == 0 {
        String::new()
    } else {
        let mut s = String::new();
        s.push_str(&left_prefix);
        s.push_str(&opts.command_display);
        truncate_with_ellipsis(&s, room_for_left)
    };

    let mut line0 = String::new();
    line0.push_str(&left);
    let used = line0.chars().count();
    let right_len = right.chars().count();
    if state.width > used + right_len {
        for _ in 0..(state.width - used - right_len) {
            line0.push(' ');
        }
    }
    line0.push_str(&right);
    line0 = truncate_with_ellipsis(&line0, state.width);

    let span_text = if elapsed > Duration::from_secs(24 * 60 * 60) {
        format!("in >1 day ({exit_code})")
    } else if elapsed < Duration::from_micros(1000) {
        format!("in <0.001s ({exit_code})")
    } else {
        format!("in {:.3}s ({exit_code})", elapsed.as_secs_f64())
    };

    let line1 = right_aligned(state.width, &span_text);

    queue!(
        stdout,
        MoveTo(0, 0),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(Color::Reset),
        SetBackgroundColor(Color::Reset),
        Print(line0),
        MoveTo(0, 1),
        Print(line1)
    )?;

    Ok(())
}

fn render_rows(
    stdout: &mut io::Stdout,
    opts: &Options,
    state: &AppState,
    rows: &[Vec<Cell>],
    highlight: Option<&[Vec<bool>]>,
) -> io::Result<()> {
    let y_offset = if opts.no_title { 0 } else { HEADER_HEIGHT };

    for y in 0..state.main_height {
        let row = rows
            .get(y)
            .cloned()
            .unwrap_or_else(|| blank_row(state.width));
        queue!(
            stdout,
            MoveTo(0, (y + y_offset) as u16),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(Color::Reset),
            SetBackgroundColor(Color::Reset)
        )?;

        let mut current_style: Option<(TextStyle, bool)> = None;
        for x in 0..state.width {
            let cell = row.get(x).cloned().unwrap_or(Cell {
                ch: ' ',
                style: TextStyle::default(),
                hidden: false,
            });

            if cell.hidden {
                continue;
            }

            let hl = highlight
                .and_then(|h| h.get(y))
                .and_then(|r| r.get(x))
                .copied()
                .unwrap_or(false);

            let style_key = (cell.style, hl);
            if current_style != Some(style_key) {
                apply_style(stdout, cell.style, hl)?;
                current_style = Some(style_key);
            }
            queue!(stdout, Print(cell.ch))?;
        }

        queue!(
            stdout,
            SetAttribute(Attribute::Reset),
            SetForegroundColor(Color::Reset),
            SetBackgroundColor(Color::Reset)
        )?;
    }

    Ok(())
}

fn render_message(
    stdout: &mut io::Stdout,
    opts: &Options,
    state: &AppState,
    message: &str,
) -> io::Result<()> {
    if state.main_height == 0 || state.width == 0 {
        return Ok(());
    }
    let y_offset = if opts.no_title { 0 } else { HEADER_HEIGHT };
    let y = y_offset + state.main_height - 1;
    let msg = truncate_with_ellipsis(message, state.width);
    queue!(
        stdout,
        MoveTo(0, y as u16),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(Color::Reset),
        SetBackgroundColor(Color::Reset),
        Print(msg)
    )?;
    Ok(())
}

fn save_screenshot(rows: &[Vec<Cell>], dir: &Path) -> io::Result<PathBuf> {
    let base = format!("watch_{}", Local::now().format("%Y%m%d-%H%M%S"));

    for suffix in 0u16..=999 {
        let mut filename = base.clone();
        if suffix > 0 {
            let _ = write!(&mut filename, "-{suffix:03}");
        }

        let path = dir.join(filename);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                for row in rows {
                    let mut line = String::new();
                    for cell in row {
                        if !cell.hidden {
                            line.push(cell.ch);
                        }
                    }
                    while line.ends_with(' ') {
                        line.pop();
                    }
                    writeln!(file, "{line}")?;
                }
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate screenshot filename",
    ))
}

fn run_command_pipe(opts: &Options) -> Result<CommandResult, AppError> {
    let (mut reader, writer_out) =
        pipe().map_err(|e| AppError::new(2, format!("pipe failed: {e}")))?;
    let writer_err = writer_out
        .try_clone()
        .map_err(|e| AppError::new(2, format!("pipe clone failed: {e}")))?;

    let mut cmd = if opts.exec_mode {
        let exe = opts
            .command_argv
            .first()
            .cloned()
            .ok_or_else(|| AppError::new(2, "missing command"))?;
        let mut c = Command::new(exe);
        c.args(opts.command_argv.iter().skip(1));
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&opts.command_display);
        c
    };

    cmd.stdout(Stdio::from(writer_out));
    cmd.stderr(Stdio::from(writer_err));

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new(2, format!("unable to start command: {e}")))?;
    // Ensure no parent-side write fds are kept alive through `cmd` while we
    // wait for EOF on the read end.
    drop(cmd);

    let reader_handle = thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    });

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| AppError::new(2, format!("failed waiting for command: {e}")))?
        {
            break status;
        }

        if event::poll(Duration::from_millis(50))
            .map_err(|e| AppError::new(1, format!("poll failed: {e}")))?
        {
            match event::read().map_err(|e| AppError::new(1, format!("read event failed: {e}")))? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader_handle.join();
                    return Ok(CommandResult {
                        output: Vec::new(),
                        exit_code: 130,
                        interrupted: true,
                    });
                }
                _ => {}
            }
        }
    };

    let output = reader_handle
        .join()
        .map_err(|_| AppError::new(2, "reader thread panicked"))?
        .map_err(|e| AppError::new(2, format!("failed to read command output: {e}")))?;

    let exit_code_i32 = if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        127
    };

    let exit_code = (exit_code_i32.clamp(0, 255)) as u8;

    Ok(CommandResult {
        output,
        exit_code,
        interrupted: false,
    })
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> io::Result<()> {
    let result = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run_command_pty(opts: &Options, width: usize, height: usize) -> Result<CommandResult, AppError> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: height.max(1) as u16,
        cols: width.max(1) as u16,
        pixel_width: 0,
        pixel_height: 0,
    })
    .map_err(|e| AppError::new(2, format!("pty failed: {e}")))?;

    let cmd = if opts.exec_mode {
        let exe = opts
            .command_argv
            .first()
            .cloned()
            .ok_or_else(|| AppError::new(2, "missing command"))?;
        let mut c = CommandBuilder::new(exe);
        for arg in opts.command_argv.iter().skip(1) {
            c.arg(arg);
        }
        c
    } else {
        let mut c = CommandBuilder::new("sh");
        c.arg("-c");
        c.arg(&opts.command_display);
        c
    };

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| AppError::new(2, format!("unable to start command: {e}")))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| AppError::new(2, format!("pty reader failed: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| AppError::new(2, format!("pty writer failed: {e}")))?;

    let reader_handle = thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    });

    let mut interrupted = false;
    let mut interrupt_started: Option<Instant> = None;
    let mut kill_sent = false;

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| AppError::new(2, format!("failed waiting for command: {e}")))?
        {
            break status;
        }

        if event::poll(Duration::from_millis(50))
            .map_err(|e| AppError::new(1, format!("poll failed: {e}")))?
        {
            match event::read().map_err(|e| AppError::new(1, format!("read event failed: {e}")))? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if !interrupted {
                        interrupted = true;
                        interrupt_started = Some(Instant::now());
                        let _ = writer.write_all(b"\x03");
                        let _ = writer.flush();
                    }
                }
                _ => {}
            }
        }

        if interrupted && !kill_sent {
            if let Some(start) = interrupt_started {
                if start.elapsed() > Duration::from_millis(200) {
                    #[cfg(unix)]
                    {
                        if let Some(pid) = child.process_id() {
                            let _ = kill_pid(pid);
                        }
                    }
                    kill_sent = true;
                }
            }
        }
    };

    let output = reader_handle
        .join()
        .map_err(|_| AppError::new(2, "reader thread panicked"))?
        .map_err(|e| AppError::new(2, format!("failed to read command output: {e}")))?;

    let exit_code = status.exit_code().clamp(0, 255) as u8;

    Ok(CommandResult {
        output,
        exit_code,
        interrupted,
    })
}

fn run_command(opts: &Options, width: usize, height: usize) -> Result<CommandResult, AppError> {
    if opts.color {
        run_command_pty(opts, width, height)
    } else {
        run_command_pipe(opts)
    }
}

fn wait_for_any_key() -> io::Result<()> {
    loop {
        if let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            return Ok(());
        }
    }
}

fn run_app(opts: Options) -> Result<i32, AppError> {
    let _guard = TerminalGuard::new()
        .map_err(|e| AppError::new(1, format!("terminal setup failed: {e}")))?;
    let mut stdout = io::stdout();
    let mut state =
        AppState::new(&opts).map_err(|e| AppError::new(1, format!("terminal size failed: {e}")))?;

    queue!(stdout, Clear(ClearType::All))
        .map_err(|e| AppError::new(1, format!("render failed: {e}")))?;
    stdout
        .flush()
        .map_err(|e| AppError::new(1, format!("flush failed: {e}")))?;

    let interval = Duration::from_secs_f64(opts.interval_secs);
    let mut next_run = Instant::now();
    let mut force_run = true;
    let mut quit = false;
    let mut quit_code = 0;

    loop {
        if force_run || Instant::now() >= next_run {
            let run_started = Instant::now();
            let result = run_command(&opts, state.width, state.main_height)?;
            if result.interrupted {
                return Ok(130);
            }
            let elapsed = run_started.elapsed();

            let parsed = if opts.follow {
                parse_output(&result.output, state.width, opts.no_wrap, opts.color, None)
            } else {
                parse_output(
                    &result.output,
                    state.width,
                    opts.no_wrap,
                    opts.color,
                    Some(state.main_height),
                )
            };

            let mut rows = if opts.follow {
                for row in parsed.rows {
                    state.follow_history.push_back(row);
                }
                while state.follow_history.len() > FOLLOW_HISTORY_LIMIT {
                    state.follow_history.pop_front();
                }

                let mut v: Vec<Vec<Cell>> = state.follow_history.iter().cloned().collect();
                if v.len() > state.main_height {
                    v = v.split_off(v.len() - state.main_height);
                }
                while v.len() < state.main_height {
                    v.push(blank_row(state.width));
                }
                v
            } else {
                let mut v = parsed.rows;
                while v.len() < state.main_height {
                    v.push(blank_row(state.width));
                }
                if v.len() > state.main_height {
                    v.truncate(state.main_height);
                }
                v
            };

            if rows.is_empty() && state.main_height > 0 {
                rows = vec![blank_row(state.width); state.main_height];
            }

            let current_chars = rows_to_chars(&rows, state.width, state.main_height);
            let (screen_changed, diff_map) = if let Some(prev) = &state.previous_chars {
                diff_chars(prev, &current_chars, state.width, state.main_height)
            } else {
                (false, vec![vec![false; state.width]; state.main_height])
            };

            if !state.first_screen && opts.differences && opts.differences_permanent {
                for (y, diff_row) in diff_map.iter().enumerate().take(state.main_height) {
                    for (x, changed) in diff_row.iter().enumerate().take(state.width) {
                        state.cumulative_diff[y][x] |= *changed;
                    }
                }
            }

            let highlight = if opts.differences {
                if opts.differences_permanent {
                    Some(state.cumulative_diff.as_slice())
                } else {
                    Some(diff_map.as_slice())
                }
            } else {
                None
            };

            queue!(stdout, Clear(ClearType::All))
                .map_err(|e| AppError::new(1, format!("render failed: {e}")))?;
            render_header(&mut stdout, &opts, &state, elapsed, result.exit_code)
                .map_err(|e| AppError::new(1, format!("render failed: {e}")))?;
            render_rows(&mut stdout, &opts, &state, &rows, highlight)
                .map_err(|e| AppError::new(1, format!("render failed: {e}")))?;

            if result.exit_code != 0 && opts.beep_on_nonzero {
                queue!(stdout, Print("\x07"))
                    .map_err(|e| AppError::new(1, format!("beep failed: {e}")))?;
            }
            if parsed.saw_bell {
                queue!(stdout, Print("\x07"))
                    .map_err(|e| AppError::new(1, format!("beep failed: {e}")))?;
            }

            stdout
                .flush()
                .map_err(|e| AppError::new(1, format!("flush failed: {e}")))?;

            state.last_rows = rows;
            state.previous_chars = Some(current_chars);

            if state.queued_screenshot {
                let _ = save_screenshot(&state.last_rows, &opts.shotsdir);
                state.queued_screenshot = false;
            }

            if result.exit_code != 0 && opts.errexit {
                render_message(
                    &mut stdout,
                    &opts,
                    &state,
                    "command exit with a non-zero status, press a key to exit",
                )
                .map_err(|e| AppError::new(1, format!("render failed: {e}")))?;
                stdout
                    .flush()
                    .map_err(|e| AppError::new(1, format!("flush failed: {e}")))?;
                wait_for_any_key().map_err(|e| AppError::new(1, format!("input failed: {e}")))?;
                return Ok(result.exit_code as i32);
            }

            if !state.first_screen {
                if opts.chgexit && screen_changed {
                    return Ok(0);
                }
                if let Some(max_cycles) = opts.equexit_cycles {
                    if screen_changed {
                        state.cycle_count = 1;
                    } else if state.cycle_count >= max_cycles {
                        return Ok(0);
                    } else {
                        state.cycle_count += 1;
                    }
                }
            }

            state.first_screen = false;

            if opts.precise {
                next_run = run_started + interval;
            } else {
                next_run = Instant::now() + interval;
            }
            force_run = false;
            continue;
        }

        let now = Instant::now();
        let timeout = if next_run > now {
            next_run - now
        } else {
            Duration::from_millis(0)
        };

        if event::poll(timeout).map_err(|e| AppError::new(1, format!("poll failed: {e}")))? {
            match event::read().map_err(|e| AppError::new(1, format!("read event failed: {e}")))? {
                Event::Resize(_, _) => {
                    state
                        .resize(&opts)
                        .map_err(|e| AppError::new(1, format!("resize failed: {e}")))?;
                    queue!(stdout, Clear(ClearType::All))
                        .map_err(|e| AppError::new(1, format!("render failed: {e}")))?;
                    stdout
                        .flush()
                        .map_err(|e| AppError::new(1, format!("flush failed: {e}")))?;
                    if !opts.no_rerun {
                        force_run = true;
                    }
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('c'), mods) if mods.contains(KeyModifiers::CONTROL) => {
                            quit = true;
                            quit_code = 130;
                        }
                        (KeyCode::Char('q'), _) => {
                            quit = true;
                        }
                        (KeyCode::Char(' '), _) => {
                            force_run = true;
                        }
                        (KeyCode::Char('s'), _) => {
                            if save_screenshot(&state.last_rows, &opts.shotsdir).is_err() {
                                state.queued_screenshot = true;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        } else {
            force_run = true;
        }

        if quit {
            break;
        }
    }

    Ok(quit_code)
}

fn real_main() -> Result<i32, AppError> {
    match parse_args()? {
        ParsedArgs::Help => {
            let program = env::args().next().unwrap_or_else(|| "watch".to_string());
            println!("{}", usage(&program));
            Ok(0)
        }
        ParsedArgs::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        ParsedArgs::Run(opts) => run_app(opts),
    }
}

fn main() {
    match real_main() {
        Ok(code) => process::exit(code),
        Err(err) => {
            eprintln!("{}", err.message);
            process::exit(err.code);
        }
    }
}
